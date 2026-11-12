/*
 * SPDX-FileCopyrightText: 2026 Coffey Labs LLC
 *
 * SPDX-License-Identifier: AGPL-3.0-only
 */

//! How a command's result is rendered: a table for people, or JSON or YAML for
//! other programs. The structured formats render the same JSON value the server
//! gave; the YAML is written here, so the tool carries no YAML dependency.

use clap::ValueEnum;
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "lower")]
pub enum Format {
    /// Columns and rows, for reading
    Table,
    /// JSON, for jq and other programs
    Json,
    /// YAML, for reading and for programs
    Yaml,
}

impl Format {
    /// The format a command uses: the one asked for, or its own default when
    /// none was.
    pub fn or(self_opt: Option<Format>, default: Format) -> Format {
        self_opt.unwrap_or(default)
    }
}

/// A JSON value as pretty JSON, newline-terminated.
pub fn json(value: &Value) -> String {
    let mut text = serde_json::to_string_pretty(value).unwrap_or_else(|_| "null".into());
    text.push('\n');
    text
}

/// A JSON value as YAML, in block style, newline-terminated.
pub fn yaml(value: &Value) -> String {
    let mut out = String::new();
    write_block(value, 0, &mut out);
    if out.is_empty() {
        out.push_str("null");
    }
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

fn indent_by(out: &mut String, level: usize) {
    for _ in 0..level {
        out.push_str("  ");
    }
}

/// Writes a value as a block: a map as `key:` lines, an array as `-` lines,
/// anything else as one scalar line, each at `level`.
fn write_block(value: &Value, level: usize, out: &mut String) {
    match value {
        Value::Object(map) if !map.is_empty() => {
            for (key, v) in map {
                indent_by(out, level);
                out.push_str(&scalar_string(key));
                out.push(':');
                write_child(v, level, out);
            }
        }
        Value::Array(items) if !items.is_empty() => {
            for item in items {
                indent_by(out, level);
                out.push('-');
                write_child(item, level, out);
            }
        }
        _ => {
            indent_by(out, level);
            out.push_str(&scalar(value));
            out.push('\n');
        }
    }
}

/// Writes what follows a `key:` or `-`: a scalar on the same line, a non-empty
/// map or array as a block one level deeper.
fn write_child(value: &Value, level: usize, out: &mut String) {
    match value {
        Value::Object(map) if !map.is_empty() => {
            out.push('\n');
            write_block(value, level + 1, out);
        }
        Value::Array(items) if !items.is_empty() => {
            out.push('\n');
            write_block(value, level + 1, out);
        }
        _ => {
            out.push(' ');
            out.push_str(&scalar(value));
            out.push('\n');
        }
    }
}

/// One scalar, with an empty map or array as its flow form.
fn scalar(value: &Value) -> String {
    match value {
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => scalar_string(s),
        Value::Array(_) => "[]".into(),
        Value::Object(_) => "{}".into(),
    }
}

/// A string as a YAML scalar: plain where that's unambiguous, double-quoted
/// where a plain scalar would be read as something else.
fn scalar_string(s: &str) -> String {
    if needs_quoting(s) { quote(s) } else { s.to_owned() }
}

fn needs_quoting(s: &str) -> bool {
    if s.is_empty() {
        return true;
    }
    // Words a reader would take for another type.
    if matches!(
        s.to_ascii_lowercase().as_str(),
        "null" | "~" | "true" | "false" | "yes" | "no" | "on" | "off"
    ) {
        return true;
    }
    // A plain number, or inf/nan, would parse as one.
    if s.parse::<f64>().is_ok() {
        return true;
    }
    let first = s.chars().next().expect("not empty");
    // An indicator at the start changes what the line means.
    if "!&*?|>%@`\"'#,[]{}:-".contains(first) || first.is_whitespace() {
        return true;
    }
    // Interior and trailing hazards for a plain scalar.
    if s.contains(": ") || s.contains(" #") || s.ends_with(':') {
        return true;
    }
    s != s.trim() || s.chars().any(char::is_control)
}

fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c if c.is_control() && (c as u32) < 0x100 => out.push_str(&format!("\\x{:02x}", c as u32)),
            c if c.is_control() => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn scalars_are_plain_or_quoted_by_what_they_could_be_mistaken_for() {
        // Plain where it's safe.
        assert_eq!(scalar(&json!("example.com")), "example.com");
        assert_eq!(scalar(&json!("alice@example.com")), "alice@example.com");
        assert_eq!(scalar(&json!("x:Domain")), "x:Domain");
        // Quoted where a plain scalar would be read as something else.
        assert_eq!(
            scalar(&json!("@x:Domain/example.com")),
            "\"@x:Domain/example.com\""
        );
        assert_eq!(scalar(&json!("true")), "\"true\"");
        assert_eq!(scalar(&json!("42")), "\"42\"");
        assert_eq!(scalar(&json!("")), "\"\"");
        assert_eq!(scalar(&json!("****")), "\"****\"");
        assert_eq!(scalar(&json!(" leading")), "\" leading\"");
        assert_eq!(scalar(&json!("a: b")), "\"a: b\"");
        // Real scalars keep their type, unquoted.
        assert_eq!(scalar(&json!(true)), "true");
        assert_eq!(scalar(&json!(42)), "42");
        assert_eq!(scalar(&json!(null)), "null");
    }

    #[test]
    fn control_characters_are_escaped() {
        assert_eq!(scalar(&json!("a\nb\tc")), "\"a\\nb\\tc\"");
    }

    #[test]
    fn a_map_becomes_block_yaml() {
        let v = json!({"name": "example.com", "isEnabled": true, "count": 3});
        // Serde maps keep insertion order with preserve_order; assert the lines.
        let y = yaml(&v);
        assert!(y.contains("name: example.com\n"), "{y}");
        assert!(y.contains("isEnabled: true\n"), "{y}");
        assert!(y.contains("count: 3\n"), "{y}");
    }

    #[test]
    fn nested_maps_and_arrays_indent() {
        let v = json!({
            "name": "alice",
            "aliases": [{"name": "sales"}, {"name": "info"}],
            "limits": {"maxSize": 10}
        });
        let y = yaml(&v);
        assert_eq!(
            y,
            "name: alice\n\
             aliases:\n\
             \x20 -\n\
             \x20   name: sales\n\
             \x20 -\n\
             \x20   name: info\n\
             limits:\n\
             \x20 maxSize: 10\n"
        );
    }

    #[test]
    fn an_array_of_objects_at_the_top_is_a_sequence() {
        let v = json!([{"name": "a"}, {"name": "b"}]);
        let y = yaml(&v);
        assert_eq!(y, "-\n  name: a\n-\n  name: b\n");
    }

    #[test]
    fn empty_collections_are_flow_scalars() {
        assert_eq!(scalar(&json!({})), "{}");
        assert_eq!(scalar(&json!([])), "[]");
        let y = yaml(&json!({"aliases": [], "quotas": {}}));
        assert_eq!(y, "aliases: []\nquotas: {}\n");
    }

    #[test]
    fn a_whole_value_at_the_top_renders() {
        assert_eq!(yaml(&json!({})), "{}\n");
        assert_eq!(yaml(&json!([])), "[]\n");
        assert_eq!(yaml(&json!(null)), "null\n");
        assert_eq!(yaml(&json!("hi")), "hi\n");
        assert_eq!(yaml(&json!(42)), "42\n");
    }
}
