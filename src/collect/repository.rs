//! Finds the git origin each project directory belongs to.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use regex::Regex;

static SCP_REMOTE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?:[^@/]+@)?([^/:]+):([^/].*)$").unwrap());
static REMOTE_SECTION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"^\s*\[\s*remote\s+"([^"]+)"\s*\]"#).unwrap());
static ANY_SECTION: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*\[").unwrap());
static URL_LINE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*url\s*=\s*(.*)$").unwrap());
static GITDIR_LINE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^gitdir:\s*(.+)$").unwrap());

/// Lexically cleans a path as Go's filepath.Clean does, made absolute against the
/// working directory.
fn clean(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    };
    let mut out = PathBuf::from("/");
    for part in absolute.components() {
        match part {
            Component::Normal(p) => out.push(p),
            Component::ParentDir => {
                out.pop();
            }
            _ => {}
        }
    }
    out
}

/// Strips transport and credentials so SSH and HTTPS remotes of one repository match:
/// "github.com/owner/repo".
pub fn canonical_repository(remote: &str) -> String {
    let trimmed = remote.trim();
    let (host, path) = match SCP_REMOTE.captures(trimmed) {
        Some(m) if !trimmed.contains("://") => (m[1].to_string(), m[2].to_string()),
        _ => {
            let Ok(u) = url::Url::parse(trimmed) else {
                return String::new();
            };
            if !matches!(u.scheme(), "https" | "http" | "ssh" | "git") {
                return String::new();
            }
            (
                u.host_str()
                    .unwrap_or_default()
                    .trim_matches(['[', ']'])
                    .to_string(),
                u.path().to_string(),
            )
        }
    };
    let mut path = path.trim_matches('/').to_string();
    if path.to_lowercase().ends_with(".git") {
        path.truncate(path.len() - 4);
    }
    if host.is_empty() || path.is_empty() {
        return String::new();
    }
    let host = host.to_lowercase();
    if matches!(host.as_str(), "github.com" | "gitlab.com" | "bitbucket.org") {
        path = path.to_lowercase();
    }
    format!("{host}/{path}")
}

fn origin_remote(config: &str) -> String {
    let mut origin = false;
    for line in config.split('\n') {
        if let Some(m) = REMOTE_SECTION.captures(line) {
            origin = &m[1] == "origin";
            continue;
        }
        if ANY_SECTION.is_match(line) {
            origin = false;
        }
        if origin && let Some(m) = URL_LINE.captures(line) {
            return canonical_repository(&m[1]);
        }
    }
    String::new()
}

fn read_metadata(file: &Path) -> Option<String> {
    let info = std::fs::metadata(file).ok()?;
    if !info.is_file() || info.len() > 1 << 20 {
        return None;
    }
    std::fs::read(file)
        .ok()
        .map(|d| String::from_utf8_lossy(&d).into_owned())
}

/// Resolves project directories to repositories for one pass, giving up on lookups
/// five seconds after it starts.
pub struct Resolver {
    deadline: Instant,
    resolved: HashMap<String, Option<String>>,
}

impl Resolver {
    pub fn new() -> Self {
        Resolver {
            deadline: Instant::now() + Duration::from_secs(5),
            resolved: HashMap::new(),
        }
    }

    pub fn get(&mut self, cwd: &str) -> Option<String> {
        if let Some(r) = self.resolved.get(cwd) {
            return r.clone();
        }
        let r = self.resolve(cwd);
        self.resolved.insert(cwd.to_string(), r.clone());
        r
    }

    /// The repository a working directory belongs to: its origin remote, "local:<dir>"
    /// for a repository without one, or None outside git.
    fn resolve(&self, cwd: &str) -> Option<String> {
        if cwd.is_empty() || cwd == "Unknown project" || Instant::now() > self.deadline {
            return None;
        }
        let mut directory = clean(Path::new(cwd));
        for _ in 0..100 {
            let git = directory.join(".git");
            if let Ok(info) = std::fs::metadata(&git) {
                let mut git_dir = git.clone();
                if !info.is_dir() {
                    if !info.is_file() {
                        return None;
                    }
                    let pointer = read_metadata(&git)?;
                    let target = GITDIR_LINE.captures(&pointer)?[1].trim().to_string();
                    git_dir = clean(&directory.join(target));
                }
                if let Some(common) = read_metadata(&git_dir.join("commondir")) {
                    git_dir = clean(&git_dir.join(common.trim()));
                }
                let origin = read_metadata(&git_dir.join("config"))
                    .map(|c| origin_remote(&c))
                    .unwrap_or_default();
                if !origin.is_empty() {
                    return Some(origin);
                }
                let parent = git_dir.parent().unwrap_or(Path::new("/"));
                return Some(format!("local:{}", parent.display()));
            }
            let parent = directory.parent()?.to_path_buf();
            directory = parent;
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remotes_canonicalize() {
        assert_eq!(
            canonical_repository("git@github.com:Owner/Repo.git"),
            "github.com/owner/repo"
        );
        assert_eq!(
            canonical_repository("https://user:pw@GitHub.com/Owner/Repo/"),
            "github.com/owner/repo"
        );
        assert_eq!(
            canonical_repository("ssh://git@example.com:22/Team/Repo.git"),
            "example.com/Team/Repo"
        );
        assert_eq!(canonical_repository("file:///tmp/repo"), "");
    }

    #[test]
    fn origin_comes_from_its_section() {
        let config = "[core]\n\turl = nope\n[remote \"upstream\"]\n\turl = git@github.com:a/b\n[remote \"origin\"]\n\turl = https://github.com/c/d.git\n";
        assert_eq!(origin_remote(config), "github.com/c/d");
    }

    #[test]
    fn cleans_like_go() {
        assert_eq!(clean(Path::new("/a/b/../c/./d/")), PathBuf::from("/a/c/d"));
    }
}
