// 時刻・ID・JSON 書き出し・各種パースなど Go 標準ライブラリ相当の小物。
use parking_lot::Mutex;
use rand::RngCore;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// 壁時計の現在時刻 (Unix µs)
#[inline]
pub fn wall_us() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_micros() as i64).unwrap_or(0)
}

#[inline]
pub fn wall_ms() -> i64 {
    wall_us().div_euclid(1000)
}

/// time.Time.UnixMilli 相当 (µs → ms)
#[inline]
pub fn us_to_ms(us: i64) -> i64 {
    us.div_euclid(1000)
}

// ---------------------------------------------------------------- ULID (oklog/ulid の Make 相当: ms 内単調増加)

const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

struct UlidGen {
    ms: u64,
    rand: u128, // 下位 80 bit
}

static ULID: Mutex<UlidGen> = Mutex::new(UlidGen { ms: 0, rand: 0 });
const MASK80: u128 = (1u128 << 80) - 1;

pub fn new_id() -> String {
    let ms = wall_ms() as u64;
    let mut rng = rand::rng();
    let r = {
        let mut g = ULID.lock();
        let mut fresh = true;
        if g.ms == ms {
            // 1..=u32::MAX の乱数だけ増やす (oklog Monotonic(entropy, 0) と同じ)
            let inc = 1 + (rng.next_u32() as u128 % (u32::MAX as u128 - 1));
            let next = g.rand + inc;
            if next <= MASK80 {
                g.rand = next;
                fresh = false;
            }
        }
        if fresh {
            let mut b = [0u8; 16];
            rng.fill_bytes(&mut b);
            g.rand = u128::from_le_bytes(b) & MASK80;
            g.ms = ms;
        }
        g.rand
    };
    let v: u128 = ((ms as u128 & ((1u128 << 48) - 1)) << 80) | r;
    let mut out = [0u8; 26];
    for (i, o) in out.iter_mut().enumerate() {
        let shift = 125 - 5 * i as i32;
        let idx = if shift >= 0 { (v >> shift) & 31 } else { 0 };
        *o = CROCKFORD[idx as usize];
    }
    // SAFETY: CROCKFORD は ASCII
    unsafe { String::from_utf8_unchecked(out.to_vec()) }
}

/// crypto/rand + hex.EncodeToString 相当 (b バイト → 2b 文字)
pub fn secure_random_str(b: usize) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut buf = vec![0u8; b];
    rand::rng().fill_bytes(&mut buf);
    let mut s = Vec::with_capacity(b * 2);
    for x in buf {
        s.push(HEX[(x >> 4) as usize]);
        s.push(HEX[(x & 15) as usize]);
    }
    unsafe { String::from_utf8_unchecked(s) }
}

// ---------------------------------------------------------------- JSON (encoding/json 互換の書き出し)

pub struct J(pub Vec<u8>);

impl J {
    #[inline]
    pub fn new() -> Self {
        J(Vec::with_capacity(256))
    }
    #[inline]
    pub fn raw(&mut self, s: &str) -> &mut Self {
        self.0.extend_from_slice(s.as_bytes());
        self
    }
    #[inline]
    pub fn bytes(&mut self, s: &[u8]) -> &mut Self {
        self.0.extend_from_slice(s);
        self
    }
    #[inline]
    pub fn int(&mut self, v: i64) -> &mut Self {
        let mut b = itoa::Buffer::new();
        self.0.extend_from_slice(b.format(v).as_bytes());
        self
    }
    #[inline]
    pub fn boolean(&mut self, v: bool) -> &mut Self {
        self.raw(if v { "true" } else { "false" })
    }
    /// Go の float64 エンコード ('f' 形式, 最短表現)。1e-6 <= |f| < 1e21 の範囲は Rust の Display と一致する
    pub fn float(&mut self, f: f64) -> &mut Self {
        let a = f.abs();
        if a != 0.0 && (a < 1e-6 || a >= 1e21) {
            // 範囲外 (このアプリでは発生しない): Go の 'e' 形式に寄せる
            let s = format!("{:e}", f);
            let (m, e) = s.split_once('e').unwrap();
            let e: i32 = e.parse().unwrap();
            let mut out = String::from(m);
            out.push('e');
            out.push(if e < 0 { '-' } else { '+' });
            let ea = e.abs();
            if ea < 10 {
                out.push('0');
            }
            out.push_str(&ea.to_string());
            return self.raw(&out);
        }
        let s = format!("{}", f);
        self.raw(&s)
    }
    /// Go encoding/json の文字列エスケープ (HTML エスケープ有効)
    pub fn str(&mut self, s: &str) -> &mut Self {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let b = s.as_bytes();
        self.0.push(b'"');
        let mut start = 0;
        let mut i = 0;
        while i < b.len() {
            let c = b[i];
            if c < 0x80 {
                let esc: Option<&[u8]> = match c {
                    b'"' => Some(b"\\\""),
                    b'\\' => Some(b"\\\\"),
                    b'\n' => Some(b"\\n"),
                    b'\r' => Some(b"\\r"),
                    b'\t' => Some(b"\\t"),
                    0x08 => Some(b"\\b"),
                    0x0c => Some(b"\\f"),
                    _ => None,
                };
                if let Some(e) = esc {
                    self.0.extend_from_slice(&b[start..i]);
                    self.0.extend_from_slice(e);
                    i += 1;
                    start = i;
                } else if c < 0x20 || c == b'<' || c == b'>' || c == b'&' {
                    self.0.extend_from_slice(&b[start..i]);
                    self.0.extend_from_slice(b"\\u00");
                    self.0.push(HEX[(c >> 4) as usize]);
                    self.0.push(HEX[(c & 15) as usize]);
                    i += 1;
                    start = i;
                } else {
                    i += 1;
                }
                continue;
            }
            // U+2028 / U+2029 = E2 80 A8 / E2 80 A9
            if c == 0xE2 && i + 2 < b.len() && b[i + 1] == 0x80 && (b[i + 2] == 0xA8 || b[i + 2] == 0xA9) {
                self.0.extend_from_slice(&b[start..i]);
                self.0.extend_from_slice(if b[i + 2] == 0xA8 { b"\\u2028" } else { b"\\u2029" });
                i += 3;
                start = i;
                continue;
            }
            i += 1;
        }
        self.0.extend_from_slice(&b[start..]);
        self.0.push(b'"');
        self
    }
    /// "key":
    #[inline]
    #[allow(dead_code)]
    pub fn key(&mut self, k: &str) -> &mut Self {
        self.0.push(b'"');
        self.0.extend_from_slice(k.as_bytes());
        self.0.extend_from_slice(b"\":");
        self
    }
    #[inline]
    pub fn coord(&mut self, lat: i64, lon: i64) -> &mut Self {
        self.raw("{\"latitude\":").int(lat).raw(",\"longitude\":").int(lon).raw("}")
    }
}

pub fn error_json(msg: &str) -> Vec<u8> {
    let mut j = J::new();
    j.raw("{\"message\":").str(msg).raw("}");
    j.0
}

// ---------------------------------------------------------------- Go の strconv / time / net/url 相当

/// strconv.Atoi / ParseInt(s, 10, 64)
pub fn atoi(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.is_empty() {
        return None;
    }
    let (neg, digits) = match b[0] {
        b'-' => (true, &b[1..]),
        b'+' => (false, &b[1..]),
        _ => (false, b),
    };
    if digits.is_empty() {
        return None;
    }
    let mut v: i64 = 0;
    for &d in digits {
        if !d.is_ascii_digit() {
            return None;
        }
        let x = (d - b'0') as i64;
        v = if neg { v.checked_mul(10)?.checked_sub(x)? } else { v.checked_mul(10)?.checked_add(x)? };
    }
    Some(v)
}

/// time.ParseDuration 相当
pub fn parse_go_duration(s: &str) -> Option<Duration> {
    let mut s = s;
    let mut neg = false;
    if let Some(r) = s.strip_prefix('-') {
        neg = true;
        s = r;
    } else if let Some(r) = s.strip_prefix('+') {
        s = r;
    }
    if s == "0" {
        return Some(Duration::ZERO);
    }
    if s.is_empty() {
        return None;
    }
    let mut total: f64 = 0.0;
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        let st = i;
        while i < b.len() && (b[i].is_ascii_digit() || b[i] == b'.') {
            i += 1;
        }
        if st == i {
            return None;
        }
        let num: f64 = s[st..i].parse().ok()?;
        let us = i;
        while i < b.len() && !(b[i].is_ascii_digit() || b[i] == b'.') {
            i += 1;
        }
        let mult = match &s[us..i] {
            "ns" => 1.0,
            "us" | "µs" | "μs" => 1e3,
            "ms" => 1e6,
            "s" => 1e9,
            "m" => 60e9,
            "h" => 3600e9,
            _ => return None,
        };
        total += num * mult;
    }
    if neg {
        return None; // 負の待ち時間は使わない
    }
    Some(Duration::from_nanos(total as u64))
}

pub fn env_duration(key: &str, def: Duration) -> Duration {
    match std::env::var(key) {
        Ok(v) if !v.is_empty() => parse_go_duration(&v).unwrap_or(def),
        _ => def,
    }
}

pub fn env_int(key: &str, def: i64) -> i64 {
    std::env::var(key).ok().and_then(|v| atoi(&v)).unwrap_or(def)
}

fn unhex(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// url.QueryUnescape (+ → 空白)
pub fn query_unescape(s: &str, plus: bool) -> Option<String> {
    let b = s.as_bytes();
    if !b.contains(&b'%') && !(plus && b.contains(&b'+')) {
        return Some(s.to_string());
    }
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' => {
                let h = unhex(*b.get(i + 1)?)?;
                let l = unhex(*b.get(i + 2)?)?;
                out.push(h << 4 | l);
                i += 3;
            }
            b'+' if plus => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok().or_else(|| Some(String::new()))
}

/// r.URL.Query().Get(key)
pub fn query_get(query: Option<&str>, key: &str) -> String {
    let Some(q) = query else { return String::new() };
    for part in q.split('&') {
        if part.is_empty() || part.contains(';') {
            continue;
        }
        let (k, v) = part.split_once('=').unwrap_or((part, ""));
        let Some(k) = query_unescape(k, true) else { continue };
        if k != key {
            continue;
        }
        let Some(v) = query_unescape(v, true) else { continue };
        return v;
    }
    String::new()
}

/// r.Cookie(name) の値 (無ければ None)
pub fn cookie_get<'a>(headers: &'a hyper::HeaderMap, name: &str) -> Option<&'a str> {
    for h in headers.get_all(hyper::header::COOKIE) {
        let Ok(s) = h.to_str() else { continue };
        for part in s.split(';') {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            let (k, v) = part.split_once('=').unwrap_or((part, ""));
            if k.trim() != name {
                continue;
            }
            let v = v.trim();
            let v = if v.len() >= 2 && v.starts_with('"') && v.ends_with('"') { &v[1..v.len() - 1] } else { v };
            if v.bytes().any(|c| c < 0x20 || c >= 0x7f || c == b'"' || c == b';' || c == b'\\' || c == b',' || c == b' ') {
                continue;
            }
            return Some(v);
        }
    }
    None
}

// ---------------------------------------------------------------- 日時 (DATETIME(6) <-> Unix µs, UTC)

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

pub fn datetime_to_us(y: i64, mo: i64, d: i64, h: i64, mi: i64, s: i64, us: i64) -> i64 {
    ((days_from_civil(y, mo, d) * 86400 + h * 3600 + mi * 60 + s) * 1_000_000) + us
}

/// "YYYY-MM-DD HH:MM:SS[.ffffff]" → Unix µs
pub fn parse_datetime(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 10 {
        return None;
    }
    let n = |r: std::ops::Range<usize>| -> Option<i64> { s.get(r)?.parse::<i64>().ok() };
    let y = n(0..4)?;
    let mo = n(5..7)?;
    let d = n(8..10)?;
    let (mut h, mut mi, mut sec, mut us) = (0, 0, 0, 0);
    if b.len() >= 19 {
        h = n(11..13)?;
        mi = n(14..16)?;
        sec = n(17..19)?;
        if b.len() > 20 && b[19] == b'.' {
            let frac = &s[20..];
            let mut v: i64 = 0;
            let mut digits = 0;
            for c in frac.bytes().take(6) {
                v = v * 10 + (c - b'0') as i64;
                digits += 1;
            }
            while digits < 6 {
                v *= 10;
                digits += 1;
            }
            us = v;
        }
    }
    Some(datetime_to_us(y, mo, d, h, mi, sec, us))
}

/// Unix µs → "YYYY-MM-DD HH:MM:SS.ffffff"
pub fn format_datetime(us: i64, out: &mut Vec<u8>) {
    let secs = us.div_euclid(1_000_000);
    let frac = us.rem_euclid(1_000_000);
    let days = secs.div_euclid(86400);
    let sod = secs.rem_euclid(86400);
    let (y, m, d) = civil_from_days(days);
    use std::io::Write;
    let _ = write!(out, "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:06}", y, m, d, sod / 3600, sod / 60 % 60, sod % 60, frac);
}

// ---------------------------------------------------------------- ログ (Go の log/slog デフォルト出力に寄せる)

pub fn log_line(level: &str, msg: &str, kv: &str) {
    let us = wall_us();
    let secs = us.div_euclid(1_000_000);
    let (y, m, d) = civil_from_days(secs.div_euclid(86400));
    let sod = secs.rem_euclid(86400);
    let line = format!(
        "{:04}/{:02}/{:02} {:02}:{:02}:{:02} {} {}{}{}\n",
        y,
        m,
        d,
        sod / 3600,
        sod / 60 % 60,
        sod % 60,
        level,
        msg,
        if kv.is_empty() { "" } else { " " },
        kv
    );
    use std::io::Write;
    let _ = std::io::stderr().write_all(line.as_bytes());
}

#[macro_export]
macro_rules! log_info {
    ($msg:expr) => { $crate::util::log_line("INFO", $msg, "") };
    ($msg:expr, $($arg:tt)+) => { $crate::util::log_line("INFO", $msg, &format!($($arg)+)) };
}
#[macro_export]
macro_rules! log_error {
    ($msg:expr) => { $crate::util::log_line("ERROR", $msg, "") };
    ($msg:expr, $($arg:tt)+) => { $crate::util::log_line("ERROR", $msg, &format!($($arg)+)) };
}
#[macro_export]
macro_rules! log_warn {
    ($msg:expr, $($arg:tt)+) => { $crate::util::log_line("WARN", $msg, &format!($($arg)+)) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn float_format() {
        let f = |x: f64| String::from_utf8(J::new().float(x).0.clone()).unwrap();
        assert_eq!(f(3.0), "3");
        assert_eq!(f(0.0), "0");
        assert_eq!(f(4.5), "4.5");
        assert_eq!(f(10.0 / 3.0), "3.3333333333333335");
        assert_eq!(f(13.0 / 3.0), "4.333333333333333");
    }

    #[test]
    fn str_escape() {
        let mut j = J::new();
        j.str("a\"b\\<>&\n\u{1}\u{2028}日本");
        assert_eq!(String::from_utf8(j.0).unwrap(), "\"a\\\"b\\\\\\u003c\\u003e\\u0026\\n\\u0001\\u2028日本\"");
    }

    #[test]
    fn datetime_roundtrip() {
        let us = parse_datetime("2024-12-08 10:00:00.123456").unwrap();
        let mut b = Vec::new();
        format_datetime(us, &mut b);
        assert_eq!(String::from_utf8(b).unwrap(), "2024-12-08 10:00:00.123456");
        assert_eq!(parse_datetime("1970-01-01 00:00:01").unwrap(), 1_000_000);
        assert_eq!(us, 1733652000123456);
    }

    #[test]
    fn ulid_monotonic() {
        let mut prev = new_id();
        for _ in 0..10000 {
            let n = new_id();
            assert!(n > prev, "{n} <= {prev}");
            assert_eq!(n.len(), 26);
            prev = n;
        }
    }

    #[test]
    fn durations() {
        assert_eq!(parse_go_duration("8s"), Some(Duration::from_secs(8)));
        assert_eq!(parse_go_duration("400ms"), Some(Duration::from_millis(400)));
        assert_eq!(parse_go_duration("1.5s"), Some(Duration::from_millis(1500)));
        assert_eq!(parse_go_duration("1m30s"), Some(Duration::from_secs(90)));
        assert_eq!(parse_go_duration("x"), None);
    }
}
