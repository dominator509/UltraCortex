//! Minimal TOML subset parser — SPEC-DERIVED-§2 (BootstrapOperator.md).
//!
//! Supports exactly what `ultracortex.toml` needs: `[section]` headers,
//! `key = value` pairs with string / integer / float / boolean / array
//! values, `#` comments, and blank lines. Nested tables beyond one level,
//! inline tables, dates, and multi-line strings are intentionally out of
//! scope (config §2.1 uses none of them). Unknown keys are preserved so the
//! bootstrap operator can warn on them.

use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq)]
pub enum TomlValue {
    Str(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    Array(Vec<TomlValue>),
}

impl TomlValue {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            TomlValue::Str(s) => Some(s),
            _ => None,
        }
    }
    pub fn as_int(&self) -> Option<i64> {
        match self {
            TomlValue::Int(i) => Some(*i),
            _ => None,
        }
    }
    pub fn as_float(&self) -> Option<f64> {
        match self {
            TomlValue::Float(f) => Some(*f),
            TomlValue::Int(i) => Some(*i as f64),
            _ => None,
        }
    }
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            TomlValue::Bool(b) => Some(*b),
            _ => None,
        }
    }
}

/// `section -> key -> value`. Top-level (pre-section) keys live under `""`.
pub type TomlDoc = BTreeMap<String, BTreeMap<String, TomlValue>>;

pub fn parse(input: &str) -> Result<TomlDoc, String> {
    let mut doc: TomlDoc = BTreeMap::new();
    let mut section = String::new();
    doc.entry(section.clone()).or_default();

    for (lineno, raw) in input.lines().enumerate() {
        let line = strip_comment(raw).trim().to_string();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') {
            if !line.ends_with(']') {
                return Err(format!("line {}: unterminated section header", lineno + 1));
            }
            section = line[1..line.len() - 1].trim().to_string();
            if section.is_empty() {
                return Err(format!("line {}: empty section name", lineno + 1));
            }
            doc.entry(section.clone()).or_default();
            continue;
        }
        let eq = line
            .find('=')
            .ok_or_else(|| format!("line {}: expected `key = value`", lineno + 1))?;
        let key = line[..eq].trim().to_string();
        let val_str = line[eq + 1..].trim();
        if key.is_empty() {
            return Err(format!("line {}: empty key", lineno + 1));
        }
        let val = parse_value(val_str)
            .map_err(|e| format!("line {}: {}", lineno + 1, e))?;
        doc.get_mut(&section).unwrap().insert(key, val);
    }
    Ok(doc)
}

/// Strip a `#` comment, respecting quoted strings.
fn strip_comment(line: &str) -> &str {
    let mut in_str = false;
    let mut escape = false;
    for (i, c) in line.char_indices() {
        if escape {
            escape = false;
            continue;
        }
        match c {
            '\\' if in_str => escape = true,
            '"' => in_str = !in_str,
            '#' if !in_str => return &line[..i],
            _ => {}
        }
    }
    line
}

fn parse_value(s: &str) -> Result<TomlValue, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("empty value".into());
    }
    if s.starts_with('"') {
        return parse_string(s).map(TomlValue::Str);
    }
    if s.starts_with('[') {
        if !s.ends_with(']') {
            return Err("unterminated array".into());
        }
        let inner = &s[1..s.len() - 1];
        let mut items = Vec::new();
        for part in split_array_items(inner)? {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            items.push(parse_value(part)?);
        }
        return Ok(TomlValue::Array(items));
    }
    match s {
        "true" => return Ok(TomlValue::Bool(true)),
        "false" => return Ok(TomlValue::Bool(false)),
        _ => {}
    }
    // Numeric: allow underscores as digit separators.
    let cleaned: String = s.chars().filter(|c| *c != '_').collect();
    if cleaned.contains('.') || cleaned.contains('e') || cleaned.contains('E') {
        if let Ok(f) = cleaned.parse::<f64>() {
            return Ok(TomlValue::Float(f));
        }
    }
    if let Ok(i) = cleaned.parse::<i64>() {
        return Ok(TomlValue::Int(i));
    }
    Err(format!("cannot parse value: {s}"))
}

fn parse_string(s: &str) -> Result<String, String> {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() < 2 || chars[0] != '"' {
        return Err("expected string".into());
    }
    let mut out = String::new();
    let mut i = 1;
    while i < chars.len() {
        match chars[i] {
            '"' => {
                // Must be the final character.
                if i != chars.len() - 1 {
                    return Err("trailing characters after string".into());
                }
                return Ok(out);
            }
            '\\' => {
                i += 1;
                if i >= chars.len() {
                    return Err("dangling escape".into());
                }
                match chars[i] {
                    'n' => out.push('\n'),
                    't' => out.push('\t'),
                    'r' => out.push('\r'),
                    '"' => out.push('"'),
                    '\\' => out.push('\\'),
                    other => return Err(format!("unknown escape \\{other}")),
                }
            }
            c => out.push(c),
        }
        i += 1;
    }
    Err("unterminated string".into())
}

/// Split array body on top-level commas (strings may contain commas).
fn split_array_items(s: &str) -> Result<Vec<String>, String> {
    let mut items = Vec::new();
    let mut cur = String::new();
    let mut in_str = false;
    let mut escape = false;
    let mut depth = 0usize;
    for c in s.chars() {
        if escape {
            cur.push(c);
            escape = false;
            continue;
        }
        match c {
            '\\' if in_str => {
                cur.push(c);
                escape = true;
            }
            '"' => {
                in_str = !in_str;
                cur.push(c);
            }
            '[' if !in_str => {
                depth += 1;
                cur.push(c);
            }
            ']' if !in_str => {
                depth = depth.saturating_sub(1);
                cur.push(c);
            }
            ',' if !in_str && depth == 0 => {
                items.push(std::mem::take(&mut cur));
            }
            _ => cur.push(c),
        }
    }
    if in_str {
        return Err("unterminated string in array".into());
    }
    if !cur.trim().is_empty() {
        items.push(cur);
    }
    Ok(items)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_config_shape() {
        let doc = parse(
            r#"
# UltraCortex config
data_dir = "/var/lib/ultracortex"   # comment after value
shards = 4
encryption_tier = "T1"

[listen]
uds = "/run/ultracortex/ultracortex.sock"
tcp = "127.0.0.1:7741"
tcp_enabled = false

[curator]
disagreement_quota_low = 0.92
disagreement_quota_high = 0.97
probe_rate = 0.001
adjudicator_pool = ["phi-3.5-mini", "llama-3.2-3b", "smollm2-1.7b"]
"#,
        )
        .unwrap();
        assert_eq!(doc[""]["shards"].as_int(), Some(4));
        assert_eq!(doc[""]["encryption_tier"].as_str(), Some("T1"));
        assert_eq!(doc["listen"]["tcp_enabled"].as_bool(), Some(false));
        assert_eq!(doc["curator"]["disagreement_quota_low"].as_float(), Some(0.92));
        match &doc["curator"]["adjudicator_pool"] {
            TomlValue::Array(items) => assert_eq!(items.len(), 3),
            _ => panic!("expected array"),
        }
    }

    #[test]
    fn string_escapes_and_hash_in_string() {
        let doc = parse(r##"path = "a#b\nc""##).unwrap();
        assert_eq!(doc[""]["path"].as_str(), Some("a#b\nc"));
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse("[unclosed").is_err());
        assert!(parse("novalue").is_err());
        assert!(parse("k = ").is_err());
    }
}
