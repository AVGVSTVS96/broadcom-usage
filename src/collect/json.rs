//! A lazy JSON reader with gjson's semantics, which the parsing rules were written
//! against: values stay raw slices of the line until read, an object's repeated key keeps
//! its last value in `fields` and its first in `get`, and lone surrogates read as U+FFFD.

use std::borrow::Cow;
use std::io::{self, BufRead};

use super::js::{js_length, js_trim};

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub enum Val<'a> {
    #[default]
    Missing,
    Null,
    False,
    True,
    Num(f64),
    /// A string's raw contents between its quotes.
    Str(&'a str),
    Obj(&'a str),
    Arr(&'a str),
}

impl<'a> Val<'a> {
    /// Reads the value starting at the first non-space byte of `raw`.
    pub fn parse(raw: &'a str) -> Self {
        let raw = raw.trim_start_matches([' ', '\t', '\n', '\r']);
        match raw.as_bytes().first() {
            Some(b'{') => Val::Obj(raw),
            Some(b'[') => Val::Arr(raw),
            Some(b'"') => Val::Str(&raw[1..string_end(raw, 0) - 1]),
            Some(b't') => Val::True,
            Some(b'f') => Val::False,
            Some(b'n') => Val::Null,
            Some(b'-' | b'0'..=b'9') => {
                let end = raw
                    .bytes()
                    .position(|c| !matches!(c, b'0'..=b'9' | b'-' | b'+' | b'.' | b'e' | b'E'))
                    .unwrap_or(raw.len());
                Val::Num(raw[..end].parse().unwrap_or(0.0))
            }
            _ => Val::Missing,
        }
    }

    pub fn is_object(&self) -> bool {
        matches!(self, Val::Obj(_))
    }

    pub fn is_array(&self) -> bool {
        matches!(self, Val::Arr(_))
    }

    pub fn exists(&self) -> bool {
        !matches!(self, Val::Missing)
    }

    pub fn str(&self) -> Option<Cow<'a, str>> {
        match *self {
            Val::Str(raw) if raw.contains('\\') => Some(Cow::Owned(unescape(raw))),
            Val::Str(raw) => Some(Cow::Borrowed(raw)),
            _ => None,
        }
    }

    pub fn is(&self, s: &str) -> bool {
        self.str().is_some_and(|v| v == s)
    }

    /// A trimmed string value, or "" for anything else.
    pub fn text(&self) -> String {
        self.str()
            .map(|s| js_trim(&s).to_string())
            .unwrap_or_default()
    }

    pub fn num(&self) -> Option<f64> {
        match *self {
            Val::Num(n) if n.is_finite() => Some(n),
            _ => None,
        }
    }

    /// An object's members by name; a repeated key keeps its last value.
    pub fn fields<const N: usize>(&self, names: [&str; N]) -> [Val<'a>; N] {
        let mut out = [Val::Missing; N];
        for (key, value) in self.members() {
            if let Some(i) = names.iter().position(|n| *n == key) {
                out[i] = Val::parse(value);
            }
        }
        out
    }

    /// An object's member by name; a repeated key gives its first value.
    pub fn get(&self, name: &str) -> Val<'a> {
        self.members()
            .find(|(key, _)| key == name)
            .map(|(_, v)| Val::parse(v))
            .unwrap_or_default()
    }

    pub fn members(&self) -> Members<'a> {
        let raw = if let Val::Obj(raw) = *self { raw } else { "" };
        Members {
            raw,
            i: if raw.is_empty() { usize::MAX } else { 1 },
        }
    }

    pub fn elements(&self) -> impl Iterator<Item = Val<'a>> + 'a {
        let raw = if let Val::Arr(raw) = *self { raw } else { "" };
        let mut i = usize::from(!raw.is_empty());
        std::iter::from_fn(move || {
            let b = raw.as_bytes();
            while i < b.len() && (b[i] <= b' ' || b[i] == b',') {
                i += 1;
            }
            if i >= b.len() || b[i] == b']' {
                return None;
            }
            let start = i;
            i = value_end(raw, i);
            Some(Val::parse(&raw[start..i]))
        })
    }
}

/// Walks a valid JSON object's members.
pub struct Members<'a> {
    raw: &'a str,
    i: usize,
}

impl<'a> Iterator for Members<'a> {
    type Item = (Cow<'a, str>, &'a str);

    fn next(&mut self) -> Option<Self::Item> {
        let b = self.raw.as_bytes();
        let skip = |i: &mut usize| {
            while *i < b.len() && (b[*i] <= b' ' || b[*i] == b',' || b[*i] == b':') {
                *i += 1;
            }
        };
        skip(&mut self.i);
        if self.i >= b.len() || b[self.i] != b'"' {
            return None;
        }
        let end = string_end(self.raw, self.i);
        let key = &self.raw[self.i + 1..end - 1];
        let key = if key.contains('\\') {
            Cow::Owned(unescape(key))
        } else {
            Cow::Borrowed(key)
        };
        self.i = end;
        skip(&mut self.i);
        let start = self.i;
        self.i = value_end(self.raw, self.i);
        Some((key, &self.raw[start..self.i]))
    }
}

/// The index just past the string starting at s[i] == '"'.
fn string_end(s: &str, i: usize) -> usize {
    let b = s.as_bytes();
    let mut j = i + 1;
    while let Some(k) = b
        .get(j..)
        .and_then(|rest| memchr::memchr2(b'"', b'\\', rest))
    {
        j += k;
        if b[j] == b'"' {
            return j + 1;
        }
        j += 2;
    }
    b.len()
}

/// The index just past the JSON value starting at s[i].
fn value_end(s: &str, i: usize) -> usize {
    let b = s.as_bytes();
    let mut depth = 0;
    let mut j = i;
    while j < b.len() {
        match b[j] {
            b'"' => {
                j = string_end(s, j);
                if depth == 0 {
                    return j;
                }
                continue;
            }
            b'{' | b'[' => depth += 1,
            b'}' | b']' => {
                if depth == 0 {
                    return j;
                }
                depth -= 1;
                if depth == 0 {
                    return j + 1;
                }
            }
            b',' if depth == 0 => return j,
            c if depth == 0 && c <= b' ' => return j,
            _ => {}
        }
        j += 1;
    }
    b.len()
}

fn hex4(s: &[u8]) -> u32 {
    std::str::from_utf8(s)
        .ok()
        .and_then(|h| u32::from_str_radix(h, 16).ok())
        .unwrap_or(0)
}

fn unescape(raw: &str) -> String {
    let b = raw.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            c if c < b' ' => break,
            b'\\' => {
                i += 1;
                let Some(&c) = b.get(i) else { break };
                let simple = match c {
                    b'\\' => b'\\',
                    b'/' => b'/',
                    b'b' => 8,
                    b'f' => 12,
                    b'n' => b'\n',
                    b'r' => b'\r',
                    b't' => b'\t',
                    b'"' => b'"',
                    b'u' => 0,
                    _ => break,
                };
                if c != b'u' {
                    out.push(simple);
                    i += 1;
                    continue;
                }
                if i + 5 > b.len() {
                    break;
                }
                let mut r = hex4(&b[i + 1..i + 5]);
                i += 5;
                if (0xd800..0xe000).contains(&r)
                    && b.len() - i >= 6
                    && b[i] == b'\\'
                    && b[i + 1] == b'u'
                {
                    let low = hex4(&b[i + 2..i + 6]);
                    r = if (0xd800..0xdc00).contains(&r) && (0xdc00..0xe000).contains(&low) {
                        0x10000 + ((r - 0xd800) << 10) + (low - 0xdc00)
                    } else {
                        0xfffd
                    };
                    i += 6;
                }
                let c = char::from_u32(r).unwrap_or('\u{fffd}');
                out.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes());
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8(out).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

/// gjson's validator: one JSON value of any type, surrounded only by JSON whitespace.
pub fn valid(d: &[u8]) -> bool {
    Validator(d).check().is_some()
}

struct Validator<'a>(&'a [u8]);

impl Validator<'_> {
    fn at(&self, i: usize) -> Option<u8> {
        self.0.get(i).copied()
    }

    fn ws(&self, mut i: usize) -> usize {
        while matches!(self.at(i), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            i += 1;
        }
        i
    }

    fn digits(&self, mut i: usize) -> usize {
        while self.at(i).is_some_and(|c| c.is_ascii_digit()) {
            i += 1;
        }
        i
    }

    fn digit(&self, i: usize) -> Option<usize> {
        self.at(i)?.is_ascii_digit().then(|| self.digits(i))
    }

    fn string(&self, mut i: usize) -> Option<usize> {
        loop {
            let rest = self.0.get(i..)?;
            let k = memchr::memchr2(b'"', b'\\', rest)?;
            if rest[..k]
                .iter()
                .fold(false, |control, &c| control | (c < b' '))
            {
                return None;
            }
            i += k;
            if self.0[i] == b'"' {
                return Some(i + 1);
            }
            i += 1;
            match self.at(i)? {
                b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't' => {}
                b'u' => {
                    for _ in 0..4 {
                        i += 1;
                        if !self.at(i)?.is_ascii_hexdigit() {
                            return None;
                        }
                    }
                }
                _ => return None,
            }
            i += 1;
        }
    }

    fn number(&self, mut i: usize) -> Option<usize> {
        if self.at(i) == Some(b'-') {
            i += 1;
        }
        i = if self.at(i) == Some(b'0') {
            i + 1
        } else {
            self.digit(i)?
        };
        if self.at(i) == Some(b'.') {
            i = self.digit(i + 1)?;
        }
        if matches!(self.at(i), Some(b'e' | b'E')) {
            i += 1;
            if matches!(self.at(i), Some(b'+' | b'-')) {
                i += 1;
            }
            i = self.digit(i)?;
        }
        Some(i)
    }

    fn literal(&self, i: usize, word: &[u8]) -> Option<usize> {
        (self.0.get(i..i + word.len())? == word).then_some(i + word.len())
    }

    /// Reads `"key":` and returns the index after the colon.
    fn key(&self, i: usize) -> Option<usize> {
        let i = self.ws(i);
        (self.at(i)? == b'"').then_some(())?;
        let i = self.ws(self.string(i + 1)?);
        (self.at(i)? == b':').then_some(i + 1)
    }

    fn check(&self) -> Option<()> {
        let mut open: Vec<u8> = Vec::new();
        let mut i = 0;
        loop {
            i = self.ws(i);
            i = match self.at(i)? {
                b'{' if self.at(self.ws(i + 1)) == Some(b'}') => self.ws(i + 1) + 1,
                b'[' if self.at(self.ws(i + 1)) == Some(b']') => self.ws(i + 1) + 1,
                b'{' => {
                    open.push(b'}');
                    i = self.key(i + 1)?;
                    continue;
                }
                b'[' => {
                    open.push(b']');
                    i += 1;
                    continue;
                }
                b'"' => self.string(i + 1)?,
                b'-' | b'0'..=b'9' => self.number(i)?,
                b't' => self.literal(i, b"true")?,
                b'f' => self.literal(i, b"false")?,
                b'n' => self.literal(i, b"null")?,
                _ => return None,
            };
            loop {
                let Some(&close) = open.last() else {
                    return (self.ws(i) == self.0.len()).then_some(());
                };
                i = self.ws(i);
                match self.at(i)? {
                    b',' if close == b'}' => {
                        i = self.key(i + 1)?;
                        break;
                    }
                    b',' => {
                        i += 1;
                        break;
                    }
                    c if c == close => {
                        open.pop();
                        i += 1;
                    }
                    _ => return None,
                }
            }
        }
    }
}

const MAXIMUM_LINE: usize = 32 * 1024 * 1024;

/// Where a read stopped: complete lines seen and the bytes through the last newline.
#[derive(Debug, Clone, Copy, Default)]
pub struct Read {
    pub lines: usize,
    pub end: u64,
    pub tail: bool,
}

/// Calls visit for each line as token-tracker's record parser splits a log: every
/// newline ends a complete line, and a final unterminated line is incomplete. Lines over
/// 32 MiB come through as None without being read. Numbering continues from `lines`.
pub fn read_lines<R: BufRead>(
    r: &mut R,
    lines: usize,
    mut visit: impl FnMut(Option<&[u8]>, usize, bool),
) -> io::Result<Read> {
    let mut read = Read {
        lines,
        end: 0,
        tail: false,
    };
    let mut long: Vec<u8> = Vec::new();
    let (mut accumulating, mut too_long) = (false, false);
    let mut pending = 0u64;
    let oversized = |line: &[u8]| line.len() > MAXIMUM_LINE && js_length(line) > MAXIMUM_LINE;
    loop {
        let buf = match r.fill_buf() {
            Ok(buf) => buf,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        if buf.is_empty() {
            if accumulating || too_long {
                read.tail = true;
                let line = (!too_long && !oversized(&long)).then_some(&long[..]);
                visit(line, read.lines + 1, false);
            }
            return Ok(read);
        }
        let Some(n) = memchr::memchr(b'\n', buf) else {
            let n = buf.len();
            if !too_long {
                long.extend_from_slice(buf);
                accumulating = true;
                if long.len() > MAXIMUM_LINE * 3 {
                    long = Vec::new();
                    too_long = true;
                }
            }
            pending += n as u64;
            r.consume(n);
            continue;
        };
        read.lines += 1;
        if too_long {
            visit(None, read.lines, true);
        } else if accumulating {
            long.extend_from_slice(&buf[..n]);
            visit((!oversized(&long)).then_some(&long[..]), read.lines, true);
        } else {
            visit(
                (!oversized(&buf[..n])).then_some(&buf[..n]),
                read.lines,
                true,
            );
        }
        long.clear();
        (accumulating, too_long) = (false, false);
        read.end += pending + n as u64 + 1;
        pending = 0;
        r.consume(n + 1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_like_gjson() {
        for ok in [
            r#"{"a":[1,-2.5e3,true,false,null,"xé"]}"#,
            " 1 ",
            r#""s""#,
            "[]",
            "{ }",
            r#"[{"a":{}}]"#,
        ] {
            assert!(valid(ok.as_bytes()), "{ok}");
        }
        for bad in [
            "",
            "01",
            "1.",
            "-",
            "[1,]",
            r#"{"a":1,}"#,
            "{\"a\"}",
            "tru",
            "\"a\u{1}\"",
            r#""\x""#,
            "{} {}",
            "[1 2]",
        ] {
            assert!(!valid(bad.as_bytes()), "{bad}");
        }
    }

    #[test]
    fn reads_fields_like_gjson() {
        let v = Val::parse(r#"{"a":1,"b":"x\"y","a":2,"c":{"d":[1,{"e":"😀"},"\ud800"]}}"#);
        let [a, b, c] = v.fields(["a", "b", "c"]);
        assert_eq!(a, Val::Num(2.0));
        assert_eq!(v.get("a"), Val::Num(1.0));
        assert_eq!(b.text(), "x\"y");
        let items: Vec<_> = c.get("d").elements().collect();
        assert_eq!(items.len(), 3);
        assert_eq!(items[1].get("e").text(), "\u{1f600}");
        assert_eq!(items[2].text(), "\u{fffd}");
    }

    #[test]
    fn splits_lines_like_the_record_parser() {
        let mut seen = Vec::new();
        let read = read_lines(&mut "a\n\nb\nc".as_bytes(), 0, |line, n, complete| {
            seen.push((
                String::from_utf8(line.unwrap().to_vec()).unwrap(),
                n,
                complete,
            ))
        })
        .unwrap();
        assert_eq!(
            seen,
            [
                ("a".into(), 1, true),
                ("".into(), 2, true),
                ("b".into(), 3, true),
                ("c".into(), 4, false)
            ]
        );
        assert_eq!((read.lines, read.end, read.tail), (3, 5, true));
    }
}
