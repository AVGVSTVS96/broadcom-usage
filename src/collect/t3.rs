//! What T3 Code knows about each harness session: its thread, title and project.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::time::Duration;

use rusqlite::types::ValueRef;
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};

use super::js::{encode_uri_component, js_trim};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Metadata {
    pub session_title: String,
    pub project_name: String,
    pub t3_thread_id: String,
    pub t3_thread_url: String,
    pub repository_paths: Vec<String>,
}

const MAXIMUM_ROWS: usize = 20_000;

#[derive(Debug, Clone, PartialEq)]
enum Cell {
    Null,
    Text(String),
    Other,
}

type Row = HashMap<String, Cell>;

fn text(cell: Option<&Cell>, maximum: usize) -> String {
    match cell {
        Some(Cell::Text(s)) if s.encode_utf16().count() <= maximum => js_trim(s).to_string(),
        _ => String::new(),
    }
}

fn present(cell: Option<&Cell>) -> bool {
    !matches!(cell, None | Some(Cell::Null))
}

/// Reads T3 Code's state databases read-only, keyed "harness:nativeSessionId". Only
/// allowlisted columns and JSON properties leave SQLite. Any failure means no metadata,
/// never a failed collection.
pub fn read(directory: &Path, base_url: &str) -> BTreeMap<String, Metadata> {
    let mut metadata = BTreeMap::new();
    let environment_id = std::fs::metadata(directory.join("environment-id"))
        .ok()
        .filter(|m| m.is_file() && m.len() <= 512)
        .and_then(|_| std::fs::read(directory.join("environment-id")).ok())
        .and_then(|data| String::from_utf8(data).ok())
        .map(|s| text(Some(&Cell::Text(s)), 512))
        .unwrap_or_default();
    let mut deleted = HashSet::new();
    for name in ["statev2.sqlite", "state.sqlite"] {
        let file = directory.join(name);
        if !std::fs::metadata(&file).is_ok_and(|m| m.is_file() && m.len() <= 2 << 30) {
            continue;
        }
        if let Ok(values) = read_database(&file, &environment_id, base_url, &mut deleted) {
            for (key, value) in values {
                metadata.entry(key).or_insert(value);
            }
        }
    }
    metadata
}

fn query(db: &Connection, sql: &str) -> rusqlite::Result<Vec<Row>> {
    let mut statement = db.prepare(sql)?;
    let names: Vec<String> = statement
        .column_names()
        .iter()
        .map(|s| s.to_string())
        .collect();
    let rows = statement.query_map([], |row| {
        let mut out = Row::new();
        for (i, name) in names.iter().enumerate() {
            let cell = match row.get_ref(i)? {
                ValueRef::Null => Cell::Null,
                ValueRef::Text(t) => Cell::Text(String::from_utf8_lossy(t).into_owned()),
                _ => Cell::Other,
            };
            out.insert(name.clone(), cell);
        }
        Ok(out)
    })?;
    rows.collect()
}

fn harness(driver: Option<&Cell>) -> &'static str {
    match driver {
        Some(Cell::Text(d)) => match d.as_str() {
            "claudeAgent" | "claude" => "claude",
            "codex" => "codex",
            "grok" => "grok",
            "pi" => "pi",
            "muse" => "muse",
            _ => "",
        },
        _ => "",
    }
}

/// The shared web route of a thread in the environment that owns it.
fn thread_url(base: &str, environment_id: &str, thread_id: &str) -> String {
    if environment_id.is_empty() || base.ends_with('?') || base.ends_with('#') {
        return String::new();
    }
    let Ok(u) = url::Url::parse(base) else {
        return String::new();
    };
    if !matches!(u.scheme(), "http" | "https")
        || !u.username().is_empty()
        || u.password().is_some()
        || u.query().is_some()
        || u.fragment().is_some()
    {
        return String::new();
    }
    let Some(host) = u.host_str() else {
        return String::new();
    };
    let port = u.port().map(|p| format!(":{p}")).unwrap_or_default();
    format!(
        "{}://{}{port}{}/{}/{}",
        u.scheme(),
        host.to_lowercase(),
        u.path().trim_end_matches('/'),
        encode_uri_component(environment_id),
        encode_uri_component(thread_id)
    )
}

fn read_database(
    file: &Path,
    environment_id: &str,
    base_url: &str,
    deleted: &mut HashSet<String>,
) -> rusqlite::Result<HashMap<String, Metadata>> {
    let db = Connection::open_with_flags(
        file,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    db.busy_timeout(Duration::from_millis(100))?;
    let mut values: HashMap<String, Metadata> = HashMap::new();
    let tables = [
        "orchestration_v2_projection_provider_threads",
        "orchestration_v2_projection_threads",
        "projection_projects",
        "projection_threads",
        "projection_thread_sessions",
        "provider_session_runtime",
    ];
    let schema = query(
        &db,
        &format!(
            "SELECT m.name AS t, p.name AS c FROM sqlite_master m, pragma_table_info(m.name) p \
             WHERE m.type = 'table' AND m.name IN ('{}')",
            tables.join("','")
        ),
    )?;
    let mut columns: HashMap<String, HashSet<String>> = HashMap::new();
    for row in schema {
        columns
            .entry(text(row.get("t"), usize::MAX))
            .or_default()
            .insert(text(row.get("c"), usize::MAX));
    }
    let has = |table: &str, required: &[&str]| {
        required
            .iter()
            .all(|c| columns.get(table).is_some_and(|t| t.contains(*c)))
    };
    let column = |table: &str, c: &str| has(table, &[c]);
    let rows = |sql: &str| query(&db, sql).unwrap_or_default();
    let limit = MAXIMUM_ROWS;
    if has(
        "orchestration_v2_projection_threads",
        &["thread_id", "deleted_at"],
    ) {
        for row in rows(&format!(
            "SELECT thread_id AS threadId FROM orchestration_v2_projection_threads WHERE deleted_at IS NOT NULL LIMIT {limit}"
        )) {
            let id = text(row.get("threadId"), 512);
            if !id.is_empty() {
                deleted.insert(id);
            }
        }
    }
    let mut insert = |row: &Row, legacy: bool, deleted: &mut HashSet<String>| {
        let provider = harness(row.get("driver"));
        let native_id = text(row.get("nativeId"), 512);
        let thread_id = text(row.get("threadId"), 512);
        if provider.is_empty() || native_id.is_empty() || thread_id.is_empty() {
            return;
        }
        let key = format!("{provider}:{native_id}");
        if values.contains_key(&key) {
            return;
        }
        if present(row.get("deletedAt")) {
            deleted.insert(thread_id.clone());
        }
        if deleted.contains(&thread_id) {
            values.insert(
                key,
                Metadata {
                    t3_thread_id: thread_id,
                    ..Default::default()
                },
            );
            return;
        }
        if legacy && deleted.len() >= MAXIMUM_ROWS {
            return;
        }
        let mut m = Metadata {
            session_title: text(row.get("title"), 1024),
            t3_thread_url: thread_url(base_url, environment_id, &thread_id),
            t3_thread_id: thread_id,
            ..Default::default()
        };
        let project = text(row.get("projectName"), 1024);
        if project != "No project" {
            m.project_name = project;
        }
        for path in [
            text(row.get("worktreePath"), 4096),
            text(row.get("workspaceRoot"), 4096),
        ] {
            if !path.is_empty() && m.repository_paths.first() != Some(&path) {
                m.repository_paths.push(path);
            }
        }
        values.insert(key, m);
    };
    let projects = has(
        "projection_projects",
        &["project_id", "title", "workspace_root"],
    );
    let (project_fields, project_join) = if projects {
        (
            "p.title AS projectName, p.workspace_root AS workspaceRoot",
            "LEFT JOIN projection_projects p ON p.project_id = t.project_id",
        )
    } else {
        ("NULL AS projectName, NULL AS workspaceRoot", "")
    };
    if has(
        "orchestration_v2_projection_provider_threads",
        &["thread_id", "payload_json"],
    ) && has(
        "orchestration_v2_projection_threads",
        &["thread_id", "project_id", "title", "payload_json"],
    ) {
        let deleted_field = if column("orchestration_v2_projection_threads", "deleted_at") {
            "t.deleted_at AS deletedAt"
        } else {
            "NULL AS deletedAt"
        };
        let (ordering, inner) =
            if column("orchestration_v2_projection_provider_threads", "updated_at") {
                ("ORDER BY pt.updated_at DESC", ", updated_at")
            } else {
                ("", "")
            };
        let sql = format!(
            "SELECT substr(json_extract(pt.payload_json, '$.nativeThreadRef.nativeId'), 1, 513) AS nativeId,
                json_extract(pt.payload_json, '$.nativeThreadRef.driver') AS driver,
                t.thread_id AS threadId, t.title AS title, {deleted_field}, {project_fields},
                CASE WHEN length(t.payload_json) <= 262144 AND json_valid(t.payload_json)
                    THEN substr(json_extract(t.payload_json, '$.worktreePath'), 1, 4097) END AS worktreePath
            FROM (SELECT thread_id, payload_json {inner}
                FROM orchestration_v2_projection_provider_threads {}
                LIMIT {limit}) pt
            JOIN orchestration_v2_projection_threads t ON t.thread_id = pt.thread_id
            {project_join}
            WHERE length(pt.payload_json) <= 262144 AND json_valid(pt.payload_json)
            {ordering} LIMIT {limit}",
            ordering.replacen("pt.", "", 1)
        );
        for row in rows(&sql) {
            let id = text(row.get("threadId"), 512);
            if !id.is_empty() && present(row.get("deletedAt")) {
                deleted.insert(id);
            }
            insert(&row, false, deleted);
        }
    }
    if has("projection_threads", &["thread_id", "project_id", "title"]) {
        let tombstones = if deleted.len() >= MAXIMUM_ROWS {
            let ids: Vec<&String> = deleted.iter().collect();
            format!(
                "'{}'",
                serde_json::to_string(&ids)
                    .unwrap_or_default()
                    .replace('\'', "''")
            )
        } else {
            "NULL".to_string()
        };
        let (legacy_deleted, deleted_field) = if column("projection_threads", "deleted_at") {
            ("t.deleted_at IS NOT NULL", "t.deleted_at AS deletedAt")
        } else {
            ("0", "NULL AS deletedAt")
        };
        let worktree = if column("projection_threads", "worktree_path") {
            "t.worktree_path"
        } else {
            "NULL"
        };
        let common = format!(
            "t.thread_id AS threadId, t.title AS title, {deleted_field}, {project_fields}, {worktree} AS worktreePath"
        );
        let filter = format!(
            "({tombstones} IS NULL OR t.thread_id IN (SELECT value FROM json_each({tombstones})) OR {legacy_deleted})"
        );
        if has(
            "provider_session_runtime",
            &["thread_id", "provider_name", "resume_cursor_json"],
        ) {
            for row in rows(&format!(
                "SELECT COALESCE(json_extract(r.resume_cursor_json, '$.threadId'), json_extract(r.resume_cursor_json, '$.sessionId')) AS nativeId,
                    r.provider_name AS driver, {common}
                FROM provider_session_runtime r JOIN projection_threads t ON t.thread_id = r.thread_id {project_join}
                WHERE json_valid(r.resume_cursor_json) AND {filter} LIMIT {limit}"
            )) {
                insert(&row, true, deleted);
            }
        }
        if has(
            "projection_thread_sessions",
            &[
                "thread_id",
                "provider_name",
                "provider_thread_id",
                "provider_session_id",
            ],
        ) {
            for row in rows(&format!(
                "SELECT COALESCE(s.provider_thread_id, s.provider_session_id) AS nativeId, s.provider_name AS driver, {common}
                FROM projection_thread_sessions s JOIN projection_threads t ON t.thread_id = s.thread_id {project_join}
                WHERE COALESCE(s.provider_thread_id, s.provider_session_id) IS NOT NULL AND {filter} LIMIT {limit}"
            )) {
                insert(&row, true, deleted);
            }
        }
    }
    Ok(values)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thread_urls_follow_the_environment() {
        assert_eq!(
            thread_url("https://App.T3.codes/", "env 1", "t/1"),
            "https://app.t3.codes/env%201/t%2F1"
        );
        assert_eq!(thread_url("https://app.t3.codes", "", "t"), "");
        assert_eq!(thread_url("ftp://x", "e", "t"), "");
    }

    #[test]
    fn reads_threads_from_a_v2_database() {
        let dir = std::env::temp_dir().join(format!("t3-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("environment-id"), "env\n").unwrap();
        let db = Connection::open(dir.join("statev2.sqlite")).unwrap();
        db.execute_batch(
            "CREATE TABLE orchestration_v2_projection_provider_threads (thread_id TEXT, payload_json TEXT, updated_at TEXT);
             CREATE TABLE orchestration_v2_projection_threads (thread_id TEXT, project_id TEXT, title TEXT, payload_json TEXT, deleted_at TEXT);
             CREATE TABLE projection_projects (project_id TEXT, title TEXT, workspace_root TEXT);
             INSERT INTO orchestration_v2_projection_provider_threads VALUES
               ('t1', '{\"nativeThreadRef\":{\"nativeId\":\"s1\",\"driver\":\"claudeAgent\"}}', '2'),
               ('t2', '{\"nativeThreadRef\":{\"nativeId\":\"s2\",\"driver\":\"codex\"}}', '1');
             INSERT INTO orchestration_v2_projection_threads VALUES
               ('t1', 'p1', ' Fix it ', '{\"worktreePath\":\"/w/tree\"}', NULL),
               ('t2', 'p1', 'gone', '{}', '2026');
             INSERT INTO projection_projects VALUES ('p1', 'proj', '/w');",
        )
        .unwrap();
        drop(db);
        let metadata = read(&dir, "https://app.t3.codes");
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(
            metadata["claude:s1"],
            Metadata {
                session_title: "Fix it".into(),
                project_name: "proj".into(),
                t3_thread_id: "t1".into(),
                t3_thread_url: "https://app.t3.codes/env/t1".into(),
                repository_paths: vec!["/w/tree".into(), "/w".into()],
            }
        );
        assert_eq!(
            metadata["codex:s2"],
            Metadata {
                t3_thread_id: "t2".into(),
                ..Default::default()
            }
        );
    }
}
