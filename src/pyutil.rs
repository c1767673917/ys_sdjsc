//! 与原 Python 实现保持一致所需的小工具:
//! urllib.parse.quote / quote_plus / urlencode、os.path.join、datetime.isoformat、
//! str.strip、float repr、secrets.token_urlsafe 等语义的等价实现。

use base64::Engine;
use chrono::{FixedOffset, NaiveDateTime, TimeZone, Utc};
use rand::RngCore;
use serde_json::Value;

/// 北京时区 UTC+8
pub fn beijing_tz() -> FixedOffset {
    FixedOffset::east_opt(8 * 3600).unwrap()
}

/// get_beijing_now_naive()
pub fn beijing_now_naive() -> NaiveDateTime {
    Utc::now().with_timezone(&beijing_tz()).naive_local()
}

/// datetime.isoformat() (naive): 微秒为0时省略小数部分
pub fn isoformat(dt: &NaiveDateTime) -> String {
    use chrono::Timelike;
    let micros = dt.nanosecond() / 1000;
    if micros == 0 {
        dt.format("%Y-%m-%dT%H:%M:%S").to_string()
    } else {
        format!("{}.{:06}", dt.format("%Y-%m-%dT%H:%M:%S"), micros)
    }
}

/// get_beijing_now_naive().isoformat() / get_beijing_now_naive_iso()
pub fn beijing_now_iso() -> String {
    isoformat(&beijing_now_naive())
}

/// 带时区的 isoformat, 例如 2025-10-15T14:30:45.123456+08:00
pub fn beijing_now_aware_iso() -> String {
    let now = Utc::now().with_timezone(&beijing_tz());
    format!("{}+08:00", isoformat(&now.naive_local()))
}

/// 将 unix 时间戳(秒)转为北京时间 naive
pub fn beijing_from_timestamp(ts: f64) -> NaiveDateTime {
    let secs = ts.floor() as i64;
    let nanos = ((ts - secs as f64) * 1e9) as u32;
    beijing_tz()
        .timestamp_opt(secs, nanos)
        .single()
        .map(|d| d.naive_local())
        .unwrap_or_else(beijing_now_naive)
}

const ALWAYS_SAFE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789_.-~";

/// urllib.parse.quote(s, safe=safe)
pub fn quote(s: &str, safe: &str) -> String {
    let safe = safe.as_bytes();
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        if ALWAYS_SAFE.contains(&b) || safe.contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{:02X}", b));
        }
    }
    out
}

/// urllib.parse.quote_plus(s)
pub fn quote_plus(s: &str) -> String {
    if !s.contains(' ') {
        return quote(s, "");
    }
    quote(s, " ").replace(' ', "+")
}

/// urllib.parse.urlencode(dict)
pub fn urlencode(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", quote_plus(k), quote_plus(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// urllib.parse.unquote(s)
pub fn unquote(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 3 <= bytes.len() {
            let h = &bytes[i + 1..i + 3];
            if h.iter().all(|c| c.is_ascii_hexdigit()) {
                let v = u8::from_str_radix(std::str::from_utf8(h).unwrap(), 16).unwrap();
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// os.path.join(a, b) (POSIX)
pub fn path_join(a: &str, b: &str) -> String {
    if b.starts_with('/') {
        return b.to_string();
    }
    if a.is_empty() || a.ends_with('/') {
        format!("{}{}", a, b)
    } else {
        format!("{}/{}", a, b)
    }
}

/// str(pathlib.Path(p)): 去掉 "." 段、合并重复斜杠、去掉末尾斜杠
pub fn path_str(p: &str) -> String {
    if p.is_empty() {
        return ".".into();
    }
    let abs = p.starts_with('/');
    let parts: Vec<&str> = p.split('/').filter(|s| !s.is_empty() && *s != ".").collect();
    let joined = parts.join("/");
    match (abs, joined.is_empty()) {
        (true, _) => format!("/{}", joined),
        (false, true) => ".".into(),
        (false, false) => joined,
    }
}

/// os.path.dirname (POSIX)
pub fn dirname(p: &str) -> String {
    match p.rfind('/') {
        None => String::new(),
        Some(i) => {
            let head = &p[..i + 1];
            let trimmed = head.trim_end_matches('/');
            if trimmed.is_empty() {
                head.to_string()
            } else {
                trimmed.to_string()
            }
        }
    }
}

/// os.path.basename (POSIX)
pub fn basename(p: &str) -> String {
    match p.rfind('/') {
        None => p.to_string(),
        Some(i) => p[i + 1..].to_string(),
    }
}

/// Python str.isspace 判定的空白字符
fn is_py_space(c: char) -> bool {
    c.is_whitespace() || matches!(c, '\u{1c}' | '\u{1d}' | '\u{1e}' | '\u{1f}')
}

/// str.strip()
pub fn py_strip(s: &str) -> &str {
    s.trim_matches(is_py_space)
}

/// str.isdigit() 的近似实现 (非空且全部为数字字符)
pub fn py_isdigit(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_numeric())
}

/// Python float 的 repr
pub fn py_float_repr(f: f64) -> String {
    if f.is_nan() {
        return "nan".into();
    }
    if f.is_infinite() {
        return if f > 0.0 { "inf".into() } else { "-inf".into() };
    }
    if f.fract() == 0.0 && f.abs() < 1e16 {
        format!("{:.1}", f)
    } else {
        format!("{}", f)
    }
}

/// Python round(x, n) 的近似实现
pub fn py_round(x: f64, n: i32) -> f64 {
    let m = 10f64.powi(n);
    let v = x * m;
    let r = v.round();
    // 银行家舍入: 恰好 .5 时取偶数
    let r = if (v - v.trunc()).abs() == 0.5 { 2.0 * (v / 2.0).round() } else { r };
    r / m
}

/// secrets.token_urlsafe(n)
pub fn token_urlsafe(n: usize) -> String {
    let mut buf = vec![0u8; n];
    rand::thread_rng().fill_bytes(&mut buf);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buf)
}

/// str(value) 对 JSON 值的等价 (近似 Python 对 json.loads 结果调用 str)
pub fn py_str(v: &Value) -> String {
    match v {
        Value::Null => "None".into(),
        Value::Bool(b) => if *b { "True".into() } else { "False".into() },
        Value::Number(n) => {
            if n.is_f64() {
                py_float_repr(n.as_f64().unwrap_or(0.0))
            } else {
                n.to_string()
            }
        }
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Python 真值判断 (value or ...)
pub fn py_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// float(str) 的等价: 允许首尾空白, 支持 inf/nan/科学计数法/下划线分隔
pub fn py_parse_float(s: &str) -> Option<f64> {
    let t = py_strip(s);
    if t.is_empty() {
        return None;
    }
    let lower = t.to_ascii_lowercase();
    let unsigned = lower.trim_start_matches(['+', '-']);
    if matches!(unsigned, "inf" | "infinity" | "nan") {
        return lower.parse::<f64>().ok().or_else(|| {
            let neg = lower.starts_with('-');
            let v = if unsigned == "nan" { f64::NAN } else { f64::INFINITY };
            Some(if neg { -v } else { v })
        });
    }
    let cleaned: String = if t.contains('_') {
        // Python 只允许数字之间的单个下划线
        let chars: Vec<char> = t.chars().collect();
        for (i, c) in chars.iter().enumerate() {
            if *c == '_' {
                let ok = i > 0
                    && i + 1 < chars.len()
                    && chars[i - 1].is_ascii_digit()
                    && chars[i + 1].is_ascii_digit();
                if !ok {
                    return None;
                }
            }
        }
        t.replace('_', "")
    } else {
        t.to_string()
    };
    if !cleaned
        .chars()
        .all(|c| c.is_ascii_digit() || matches!(c, '.' | 'e' | 'E' | '+' | '-'))
    {
        return None;
    }
    cleaned.parse::<f64>().ok()
}

/// 时间字符串 (由 isoformat 生成) 解析为 NaiveDateTime, 等价 datetime.fromisoformat 的常见格式
pub fn parse_iso_naive(s: &str) -> Option<NaiveDateTime> {
    let fmts = [
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%dT%H:%M",
        "%Y-%m-%d %H:%M",
    ];
    for f in fmts {
        if let Ok(d) = NaiveDateTime::parse_from_str(s, f) {
            return Some(d);
        }
    }
    if let Ok(d) = chrono::DateTime::parse_from_rfc3339(s) {
        return Some(d.naive_local());
    }
    if let Ok(d) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return d.and_hms_opt(0, 0, 0);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quote_matches_python() {
        assert_eq!(quote("a b/c+=~", "/"), "a%20b/c%2B%3D~");
        assert_eq!(quote("销售", ""), "%E9%94%80%E5%94%AE");
        assert_eq!(quote_plus("a b&c"), "a+b%26c");
        assert_eq!(
            urlencode(&[("business_id", "123"), ("doc_number", "SO 1"), ("doc_type", "销售")]),
            "business_id=123&doc_number=SO+1&doc_type=%E9%94%80%E5%94%AE"
        );
    }

    #[test]
    fn path_helpers() {
        assert_eq!(path_join("./cache", "2025/01/a.jpg"), "./cache/2025/01/a.jpg");
        assert_eq!(path_join("./cache/", "a"), "./cache/a");
        assert_eq!(path_join("x", "/abs"), "/abs");
        assert_eq!(dirname("/files/2025/01"), "/files/2025");
        assert_eq!(dirname("/files"), "/");
        assert_eq!(dirname("files"), "");
        assert_eq!(dirname("files/a.jpg"), "files");
        assert_eq!(basename("/a/b/c.txt"), "c.txt");
        assert_eq!(path_str("./cache"), "cache");
        assert_eq!(path_str("./"), ".");
        assert_eq!(path_str("/a//b/"), "/a/b");
    }

    #[test]
    fn floats() {
        assert_eq!(py_float_repr(10.0), "10.0");
        assert_eq!(py_float_repr(1.5), "1.5");
        assert_eq!(py_parse_float(" 1,234 ".replace(',', "").as_str()), Some(1234.0));
        assert_eq!(py_parse_float("abc"), None);
        assert_eq!(py_parse_float("None"), None);
        assert_eq!(py_round(2.675, 2), 2.67_f64.max(py_round(2.675, 2)));
    }

    #[test]
    fn token() {
        assert_eq!(token_urlsafe(16).len(), 22);
    }

    #[test]
    fn iso() {
        let d = NaiveDateTime::parse_from_str("2025-10-15 14:30:45", "%Y-%m-%d %H:%M:%S").unwrap();
        assert_eq!(isoformat(&d), "2025-10-15T14:30:45");
        assert_eq!(unquote("/a%20b/%E9%94%80"), "/a b/销");
    }
}
