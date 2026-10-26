/*
 * SPDX-FileCopyrightText: 2026 Coffey Labs LLC
 *
 * SPDX-License-Identifier: AGPL-3.0-only
 */

//! Properties given on the command line as `name=value`.

use serde_json::{Map, Value};

use crate::error::{Error, Result};

/// `name=value` pairs as a JSON object. A value that reads as JSON (`true`,
/// `42`, `null`, `["a"]`, `{"@type": "Stdout"}`) is taken as JSON; anything
/// else is a string, so `name=example.com` needs no quoting. To pass a string
/// that looks like JSON, quote it: `description='"true"'`.
///
/// A name with slashes (`limits/maxSize=10`) is a JMAP patch path, which an
/// update applies to that one nested property.
pub fn parse_pairs(pairs: &[String]) -> Result<Map<String, Value>> {
    let mut out = Map::new();
    for pair in pairs {
        let (name, raw) = pair
            .split_once('=')
            .ok_or_else(|| Error::Usage(format!("{pair:?} isn't name=value")))?;
        if name.is_empty() {
            return Err(Error::Usage(format!("{pair:?} has no property name")));
        }
        let value = serde_json::from_str::<Value>(raw).unwrap_or_else(|_| Value::String(raw.to_owned()));
        out.insert(name.to_owned(), value);
    }
    Ok(out)
}

/// A JSON object from a file, or from standard input for `-`.
pub fn read_object(path: &str) -> Result<Map<String, Value>> {
    let text = if path == "-" {
        std::io::read_to_string(std::io::stdin())?
    } else {
        std::fs::read_to_string(path).map_err(|e| Error::Usage(format!("{path}: {e}")))?
    };
    match serde_json::from_str::<Value>(&text) {
        Ok(Value::Object(map)) => Ok(map),
        Ok(_) => Err(Error::Usage(format!("{path} isn't a JSON object"))),
        Err(e) => Err(Error::Usage(format!("{path}: {e}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn json_where_it_parses_and_strings_elsewhere() {
        let m = parse_pairs(&[
            "name=example.com".into(),
            "isEnabled=true".into(),
            "max=10".into(),
            "tags=[\"a\"]".into(),
            "description=\"true\"".into(),
            "empty=".into(),
        ])
        .unwrap();
        assert_eq!(m["name"], json!("example.com"));
        assert_eq!(m["isEnabled"], json!(true));
        assert_eq!(m["max"], json!(10));
        assert_eq!(m["tags"], json!(["a"]));
        assert_eq!(m["description"], json!("true"));
        assert_eq!(m["empty"], json!(""));
    }

    #[test]
    fn a_pair_needs_a_name_and_an_equals_sign() {
        assert!(parse_pairs(&["name".into()]).is_err());
        assert!(parse_pairs(&["=x".into()]).is_err());
    }
}
