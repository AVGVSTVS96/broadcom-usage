//! Record ids hash JSON.stringify output and parse timestamps the way Bun does, so these
//! helpers reproduce JavaScript's string, number and date behavior exactly.

use std::borrow::Cow;
use std::fmt::Write;

use chrono::{Local, NaiveDateTime, Offset, TimeZone};
use sha2::{Digest, Sha256};

pub fn js_space(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\u{b}' | '\u{c}' | '\r' | ' ' | '\u{85}' | '\u{a0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200a}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202f}'
                | '\u{205f}'
                | '\u{3000}'
                | '\u{feff}'
    )
}

pub fn js_trim(s: &str) -> &str {
    s.trim_matches(js_space)
}

/// A UTF-8 text's length in UTF-16 code units; an invalid byte counts as one.
pub fn js_length(bytes: &[u8]) -> usize {
    bytes
        .iter()
        .map(|&b| match b {
            0x80..=0xbf => 0,
            0xf0..=0xff => 2,
            _ => 1,
        })
        .sum()
}

fn js_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Formats a number as Number.prototype.toString does.
pub fn js_number(f: f64) -> String {
    if !f.is_finite() {
        return "null".into();
    }
    if f == 0.0 {
        return "0".into();
    }
    let (sign, f) = if f < 0.0 { ("-", -f) } else { ("", f) };
    let e = format!("{f:e}");
    let (mantissa, exp) = e.split_once('e').unwrap_or((&e, "0"));
    let digits = mantissa.replace('.', "");
    let k = digits.len() as i32;
    let n = exp.parse::<i32>().unwrap_or(0) + 1;
    if k <= n && n <= 21 {
        return format!("{sign}{digits}{}", "0".repeat((n - k) as usize));
    }
    if 0 < n && n <= 21 {
        let (int, frac) = digits.split_at(n as usize);
        return format!("{sign}{int}.{frac}");
    }
    if -6 < n && n <= 0 {
        return format!("{sign}0.{}{digits}", "0".repeat(-n as usize));
    }
    let frac = if k > 1 {
        format!(".{}", &digits[1..])
    } else {
        String::new()
    };
    let exp_sign = if n - 1 < 0 { '-' } else { '+' };
    format!("{sign}{}{frac}e{exp_sign}{}", &digits[..1], (n - 1).abs())
}

pub enum J<'a> {
    S(&'a str),
    I(i64),
    F(f64),
}

/// JSON.stringify of an array of strings and numbers.
pub fn json_array(values: &[J]) -> String {
    let mut out = String::from("[");
    for (i, v) in values.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        match v {
            J::S(s) => js_string(&mut out, s),
            J::I(n) => out.push_str(&n.to_string()),
            J::F(f) => out.push_str(&js_number(*f)),
        }
    }
    out.push(']');
    out
}

pub fn sha256_hex(s: &str) -> String {
    Sha256::digest(s.as_bytes())
        .iter()
        .fold(String::with_capacity(64), |mut out, b| {
            let _ = write!(out, "{b:02x}");
            out
        })
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// Date.prototype.toISOString, or "" where JavaScript's Date is invalid or the value is
/// not positive (the parsers' own rule).
pub fn iso_millis(ms: f64) -> String {
    if !ms.is_finite() || ms <= 0.0 || ms > 8.64e15 {
        return String::new();
    }
    let ms = ms as i64;
    let (days, rest) = (ms.div_euclid(86_400_000), ms.rem_euclid(86_400_000));
    let (year, month, day) = civil_from_days(days);
    let year = if year <= 9999 {
        format!("{year:04}")
    } else {
        format!("+{year:06}")
    };
    format!(
        "{year}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        rest / 3_600_000,
        rest / 60_000 % 60,
        rest / 1000 % 60,
        rest % 1000
    )
}

/// Bun's Date.parse for ISO-like strings: fractional seconds truncate to milliseconds,
/// out-of-range days roll over, hour 24 is allowed, and a date-time without an offset is
/// local time. Anything else is NaN.
pub fn parse_date(s: &str) -> f64 {
    DateParser {
        s: s.as_bytes(),
        i: 0,
    }
    .parse()
    .or_else(|| parse_http_date(s))
    .unwrap_or(f64::NAN)
}

/// Go's RFC1123, RFC1123Z, RFC850 and ANSIC layouts; the weekday is checked for nothing.
fn parse_http_date(s: &str) -> Option<f64> {
    let (_, rest) = s.split_once(' ')?;
    let utc = |t: NaiveDateTime| t.and_utc().timestamp_millis() as f64;
    for layout in [
        "%d %b %Y %H:%M:%S %z",
        "%d-%b-%y %H:%M:%S %Z",
        "%d %b %Y %H:%M:%S %Z",
        "%b %e %H:%M:%S %Y",
    ] {
        if layout.ends_with("%z") {
            if let Ok(t) = chrono::DateTime::parse_from_str(rest, layout) {
                return Some(t.timestamp_millis() as f64);
            }
        } else if let Ok(t) = NaiveDateTime::parse_from_str(rest, layout) {
            return Some(utc(t));
        }
    }
    None
}

struct DateParser<'a> {
    s: &'a [u8],
    i: usize,
}

impl DateParser<'_> {
    fn digits(&mut self, n: usize) -> Option<i64> {
        let part = self.s.get(self.i..self.i + n)?;
        let mut v = 0;
        for &c in part {
            if !c.is_ascii_digit() {
                return None;
            }
            v = v * 10 + i64::from(c - b'0');
        }
        self.i += n;
        Some(v)
    }

    fn peek(&self, chars: &[u8]) -> bool {
        self.s.get(self.i).is_some_and(|c| chars.contains(c))
    }

    fn parse(&mut self) -> Option<f64> {
        let year = if self.peek(b"+-") {
            let neg = self.s[self.i] == b'-';
            self.i += 1;
            let y = self.digits(6)?;
            if neg { -y } else { y }
        } else {
            self.digits(4)?
        };
        let (mut month, mut day) = (1, 1);
        if self.peek(b"-") {
            self.i += 1;
            month = self.digits(2)?;
            if self.peek(b"-") {
                self.i += 1;
                day = self.digits(2)?;
            }
        }
        if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
            return None;
        }
        let (mut hour, mut minute, mut second, mut millis) = (0, 0, 0, 0);
        let has_time = self.peek(b"Tt ");
        let mut local = false;
        if has_time {
            self.i += 1;
            hour = self.digits(2)?;
            if !self.peek(b":") {
                return None;
            }
            self.i += 1;
            minute = self.digits(2)?;
            if self.peek(b":") {
                self.i += 1;
                second = self.digits(2)?;
                if self.peek(b".") {
                    self.i += 1;
                    let start = self.i;
                    while self.peek(b"0123456789") {
                        self.i += 1;
                    }
                    if start == self.i {
                        return None;
                    }
                    let frac = &self.s[start..self.i.min(start + 3)];
                    millis = frac.iter().fold(0, |v, c| v * 10 + i64::from(c - b'0'))
                        * 10_i64.pow(3 - frac.len() as u32);
                }
            }
            if hour > 24
                || minute > 59
                || second > 59
                || (hour == 24 && (minute > 0 || second > 0 || millis > 0))
            {
                return None;
            }
            local = self.i == self.s.len();
        }
        let mut offset = 0;
        if !local && self.i < self.s.len() {
            if !has_time {
                return None;
            }
            if self.peek(b"Zz") {
                self.i += 1;
            } else if self.peek(b"+-") {
                let sign = if self.s[self.i] == b'-' { -1 } else { 1 };
                self.i += 1;
                let oh = self.digits(2)?;
                if self.peek(b":") {
                    self.i += 1;
                }
                let om = self.digits(2)?;
                if oh > 23 || om > 59 {
                    return None;
                }
                offset = sign * (oh * 60 + om);
            } else {
                return None;
            }
            if self.i != self.s.len() {
                return None;
            }
        }
        let wall = (days_from_civil(year, month, 1) + day - 1) * 86_400_000
            + hour * 3_600_000
            + minute * 60_000
            + second * 1000
            + millis;
        let zone = if local {
            local_offset_millis(wall)
        } else {
            offset * 60_000
        };
        Some((wall - zone) as f64)
    }
}

fn local_offset_millis(wall: i64) -> i64 {
    let Some(naive) = chrono::DateTime::from_timestamp_millis(wall).map(|t| t.naive_utc()) else {
        return 0;
    };
    let offset = Local
        .offset_from_local_datetime(&naive)
        .earliest()
        .unwrap_or_else(|| Local.offset_from_utc_datetime(&naive));
    i64::from(offset.fix().local_minus_utc()) * 1000
}

/// Replaces each invalid byte with U+FFFD, as the Go reader did.
pub fn valid_utf8(bytes: &[u8]) -> Cow<'_, str> {
    if let Ok(s) = std::str::from_utf8(bytes) {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(bytes.len() + 8);
    let mut rest = bytes;
    while !rest.is_empty() {
        match std::str::from_utf8(rest) {
            Ok(s) => {
                out.push_str(s);
                break;
            }
            Err(e) => {
                let valid = e.valid_up_to();
                out.push_str(std::str::from_utf8(&rest[..valid]).unwrap_or_default());
                out.push('\u{fffd}');
                rest = &rest[valid + 1..];
            }
        }
    }
    Cow::Owned(out)
}

pub fn encode_uri_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &c in s.as_bytes() {
        if c.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&c) {
            out.push(c as char);
        } else {
            let _ = write!(out, "%{c:02X}");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_print_like_javascript() {
        for (input, want) in [
            (1e21, "1e+21"),
            (1e-7, "1e-7"),
            (0.000003, "0.000003"),
            (123.456, "123.456"),
            (0.0, "0"),
            (5e-324, "5e-324"),
            (1.7976931348623157e308, "1.7976931348623157e+308"),
            (100.0, "100"),
            (0.30000000000000004, "0.30000000000000004"),
            (9007199254740994.0, "9007199254740994"),
            (1.5e-6, "0.0000015"),
            (123456789012345680000.0, "123456789012345680000"),
        ] {
            assert_eq!(js_number(input), want, "{input}");
        }
    }

    #[test]
    fn dates_parse_like_bun() {
        for (input, want) in [
            ("2025-01-01T00:00:00.1239Z", 1735689600123.0),
            ("2025-01-01T00:00:00.9999Z", 1735689600999.0),
            ("2025-01-01T00:00Z", 1735689600000.0),
            ("2025-01-01", 1735689600000.0),
            ("2025-01", 1735689600000.0),
            ("2025", 1735689600000.0),
            ("2025-01-01T00:00:00.5+02:00", 1735682400500.0),
            ("2025-01-01T00:00:00+0200", 1735682400000.0),
            ("2025-01-01 00:00:00Z", 1735689600000.0),
            ("2025-01-01T24:00:00Z", 1735776000000.0),
            ("2025-02-30T00:00:00Z", 1740873600000.0),
            ("+002025-01-01T00:00:00Z", 1735689600000.0),
            ("2025-01-01T00:00:00.123456789Z", 1735689600123.0),
            ("Tue, 01 Jan 2025 00:00:00 GMT", 1735689600000.0),
            ("2025-01-01t00:00:00z", 1735689600000.0),
            ("2025-01-01T00:00:00.000-00:00", 1735689600000.0),
        ] {
            assert_eq!(parse_date(input), want, "{input}");
        }
        for input in [
            "2025-01-01T00:00:60Z",
            "2025-01-01T00:00:00.Z",
            " 2025-01-01T00:00:00Z",
            "1e3",
        ] {
            assert!(parse_date(input).is_nan(), "{input}");
        }
    }

    #[test]
    fn iso_strings_and_trim() {
        assert_eq!(iso_millis(1735689600123.9), "2025-01-01T00:00:00.123Z");
        assert_eq!(iso_millis(0.0), "");
        assert_eq!(iso_millis(9e15), "");
        assert_eq!(js_trim(" \u{feff}\u{a0}\u{2028}x\u{3000} "), "x");
        assert_eq!(
            json_array(&[J::S("a\"b\n\u{1}"), J::I(3), J::F(1.5)]),
            r#"["a\"b\n\u0001",3,1.5]"#
        );
    }

    #[test]
    fn invalid_bytes_each_become_a_replacement() {
        assert_eq!(valid_utf8(b"a\xe2\x82b"), "a\u{fffd}\u{fffd}b");
    }
}
