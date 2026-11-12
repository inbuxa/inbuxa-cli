/*
 * SPDX-FileCopyrightText: 2026 Coffey Labs LLC
 *
 * SPDX-License-Identifier: AGPL-3.0-only
 */

//! `inbuxa-cli logs` and `inbuxa-cli events`: a live tail of the server's
//! trace stream.
//!
//! Both read the server's `GET /api/live/tracing` Server-Sent Events, the
//! same stream the console's live log shows. `logs` prints a line a person
//! reads; `events` prints one JSON object a line, for `jq` and `grep`. A
//! keyword or `key=value` narrows the stream server-side. The tail runs until
//! interrupted, opening a fresh stream when the server ends one (it caps each
//! at 30 minutes).

use std::io::{BufRead, BufReader, IsTerminal, Write};
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};

use crate::client::Client;
use crate::error::{Error, Result};

/// The live stream for trace events.
const TRACING: &str = "tracing";

/// After the server ends a stream we reopen at once; after a failure to
/// connect we wait this long and try again, giving up at [`MAX_RETRIES`].
const RETRY_DELAY: Duration = Duration::from_secs(2);
const MAX_RETRIES: u32 = 5;

/// A reconnect only counts as a retry if the last stream was short; a stream
/// that ran a while and then ended is the server's 30-minute cap, not a fault.
const SETTLED: Duration = Duration::from_secs(60);

macro_rules! out {
    ($($arg:tt)*) => { writeln!(std::io::stdout(), $($arg)*)? };
}

/// How a frame's events are shown: a line each, or a JSON object each.
#[derive(Clone, Copy)]
pub enum View {
    Logs,
    Events,
}

/// Runs the tail. `keywords` is the free-text `filter=`; `pairs` are the
/// `key=value` arguments, each its own query parameter.
pub fn run(client: &Client, view: View, keywords: Option<String>, pairs: &[String]) -> Result<()> {
    let params = params(keywords, pairs)?;
    let mut retries = 0;
    loop {
        let started = Instant::now();
        match tail(client, view, &params) {
            Ok(()) => {
                // The stream ended cleanly. Reopen; count it as a retry only
                // if it barely ran, so a flapping endpoint still gives up.
                if started.elapsed() >= SETTLED {
                    retries = 0;
                } else {
                    retries += 1;
                }
            }
            // A broken pipe is `| head` closing; stop quietly.
            Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::BrokenPipe => return Ok(()),
            Err(e) => {
                if started.elapsed() >= SETTLED {
                    retries = 0;
                }
                retries += 1;
                if retries > MAX_RETRIES {
                    return Err(e);
                }
                if std::io::stderr().is_terminal() {
                    eprintln!("inbuxa-cli: {e}; retrying ({retries}/{MAX_RETRIES})");
                }
            }
        }
        if retries > MAX_RETRIES {
            return Err(Error::Transport(format!(
                "the trace stream kept dropping; gave up after {MAX_RETRIES} tries"
            )));
        }
        std::thread::sleep(RETRY_DELAY);
    }
}

/// Opens one stream and shows its events until it ends.
fn tail(client: &Client, view: View, params: &[(String, String)]) -> Result<()> {
    let stream = client.open_live(TRACING, params)?;
    let reader = BufReader::new(stream);
    let mut frame = Frame::default();
    for line in reader.lines() {
        let line = line?;
        if line.is_empty() {
            if frame.event == "trace" && !frame.data.is_empty() {
                show(view, &frame.data)?;
            }
            frame = Frame::default();
            continue;
        }
        frame.take(&line);
    }
    Ok(())
}

/// One Server-Sent Event as it's read, a field at a time.
#[derive(Default)]
struct Frame {
    event: String,
    data: String,
}

impl Frame {
    fn take(&mut self, line: &str) {
        if let Some(rest) = line.strip_prefix("event:") {
            self.event = rest.trim().to_owned();
        } else if let Some(rest) = line.strip_prefix("data:") {
            if !self.data.is_empty() {
                self.data.push('\n');
            }
            self.data.push_str(rest.strip_prefix(' ').unwrap_or(rest));
        }
        // id:, retry: and `:` comments carry nothing we show.
    }
}

fn show(view: View, data: &str) -> Result<()> {
    let parsed: Value = match serde_json::from_str(data) {
        Ok(value) => value,
        // A frame we can't parse is the server's to explain; skip it rather
        // than end the tail.
        Err(_) => return Ok(()),
    };
    for event in jmap_array(&parsed) {
        match view {
            View::Logs => out!("{}", log_line(event)),
            View::Events => print_json_line(&flatten(event))?,
        }
    }
    Ok(())
}

// ── The query ──

fn params(keywords: Option<String>, pairs: &[String]) -> Result<Vec<(String, String)>> {
    let mut params = Vec::new();
    if let Some(keywords) = keywords {
        let keywords = keywords.trim();
        if !keywords.is_empty() {
            params.push(("filter".to_owned(), keywords.to_owned()));
        }
    }
    for pair in pairs {
        let (key, value) = pair
            .split_once('=')
            .ok_or_else(|| Error::Usage(format!("{pair} isn't key=value")))?;
        params.push((key.to_owned(), value.to_owned()));
    }
    Ok(params)
}

// ── Rendering a trace event ──

/// A log line: the time, the event, then its key=values.
fn log_line(event: &Value) -> String {
    let timestamp = field(event, "timestamp");
    let name = field(event, "event");
    let mut line = String::new();
    if !timestamp.is_empty() {
        line.push_str(&timestamp);
        line.push_str("  ");
    }
    line.push_str(&name);
    for kv in jmap_array(event.get("keyValues").unwrap_or(&Value::Null)) {
        let key = field(kv, "key");
        let value = human(kv.get("value").unwrap_or(&Value::Null));
        if !key.is_empty() {
            line.push_str(&format!(" {key}={value}"));
        }
    }
    line
}

/// A value for a log line, read from its `@type` tag. Durations are
/// milliseconds; a nested event shows its name and its own key=values.
fn human(value: &Value) -> String {
    match tag(value) {
        "String" | "IpAddr" | "UTCDateTime" => str_value(value),
        "UnsignedInt" | "Integer" | "Float" => num_value(value),
        "Boolean" => value
            .get("value")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            .to_string(),
        "Duration" => format!("{}ms", num_value(value)),
        "List" => {
            let parts: Vec<String> = jmap_array(value.get("value").unwrap_or(&Value::Null))
                .iter()
                .map(|v| human(v))
                .collect();
            format!("[{}]", parts.join(", "))
        }
        "Event" => {
            let inner: Vec<String> = jmap_array(value.get("value").unwrap_or(&Value::Null))
                .iter()
                .map(|kv| {
                    format!(
                        "{}={}",
                        field(kv, "key"),
                        human(kv.get("value").unwrap_or(&Value::Null))
                    )
                })
                .collect();
            format!("{}{{{}}}", field(value, "event"), inner.join(" "))
        }
        _ => String::new(),
    }
}

/// A trace event as a flat JSON object for `events`: the time, the event
/// name, then each key with its value in plain JSON (the `@type` tags gone).
fn flatten(event: &Value) -> Value {
    let mut out = Map::new();
    out.insert("timestamp".to_owned(), json!(field(event, "timestamp")));
    out.insert("event".to_owned(), json!(field(event, "event")));
    for kv in jmap_array(event.get("keyValues").unwrap_or(&Value::Null)) {
        let key = field(kv, "key");
        if !key.is_empty() {
            out.insert(key, plain(kv.get("value").unwrap_or(&Value::Null)));
        }
    }
    Value::Object(out)
}

/// A value as plain JSON, read from its `@type` tag.
fn plain(value: &Value) -> Value {
    match tag(value) {
        "String" | "IpAddr" | "UTCDateTime" => json!(str_value(value)),
        "UnsignedInt" | "Integer" | "Duration" => value
            .get("value")
            .and_then(Value::as_i64)
            .map_or(Value::Null, |n| json!(n)),
        "Float" => value
            .get("value")
            .and_then(Value::as_f64)
            .map_or(Value::Null, |n| json!(n)),
        "Boolean" => json!(value.get("value").and_then(Value::as_bool).unwrap_or(false)),
        "List" => Value::Array(
            jmap_array(value.get("value").unwrap_or(&Value::Null))
                .iter()
                .map(|v| plain(v))
                .collect(),
        ),
        "Event" => {
            let mut inner = Map::new();
            inner.insert("event".to_owned(), json!(field(value, "event")));
            for kv in jmap_array(value.get("value").unwrap_or(&Value::Null)) {
                let key = field(kv, "key");
                if !key.is_empty() {
                    inner.insert(key, plain(kv.get("value").unwrap_or(&Value::Null)));
                }
            }
            Value::Object(inner)
        }
        _ => Value::Null,
    }
}

// ── Reading JSON the server's way ──

/// The server sends lists as JMAP maps — `{"0": …, "1": …}` — at every level.
/// An array is itself; a map is its values in index order; anything else is
/// empty, as the console's `jmapMapToArray` does.
fn jmap_array(value: &Value) -> Vec<&Value> {
    match value {
        Value::Array(a) => a.iter().collect(),
        Value::Object(o) => {
            let mut entries: Vec<(&String, &Value)> = o.iter().collect();
            entries.sort_by_key(|(k, _)| k.parse::<i64>().unwrap_or(i64::MAX));
            entries.into_iter().map(|(_, v)| v).collect()
        }
        _ => Vec::new(),
    }
}

fn tag(value: &Value) -> &str {
    value.get("@type").and_then(Value::as_str).unwrap_or("")
}

fn str_value(value: &Value) -> String {
    value
        .get("value")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned()
}

fn num_value(value: &Value) -> String {
    match value.get("value") {
        Some(Value::Number(n)) => n.to_string(),
        _ => "0".to_owned(),
    }
}

fn field(value: &Value, key: &str) -> String {
    value.get(key).and_then(Value::as_str).unwrap_or("").to_owned()
}

fn print_json_line(value: &Value) -> Result<()> {
    let mut out = std::io::stdout().lock();
    serde_json::to_writer(&mut out, value)?;
    writeln!(out)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_frame_reads_event_and_data_fields() {
        let mut frame = Frame::default();
        frame.take("event: trace");
        frame.take("data: {\"a\":1}");
        frame.take("data: more");
        assert_eq!(frame.event, "trace");
        assert_eq!(frame.data, "{\"a\":1}\nmore");
    }

    #[test]
    fn jmap_maps_become_arrays_in_index_order() {
        let map = json!({"1": "b", "0": "a", "10": "c"});
        let got: Vec<&Value> = jmap_array(&map);
        assert_eq!(got, vec![&json!("a"), &json!("b"), &json!("c")]);
        assert_eq!(jmap_array(&json!(["x", "y"])), vec![&json!("x"), &json!("y")]);
        assert!(jmap_array(&Value::Null).is_empty());
    }

    fn event() -> Value {
        json!({
            "event": "smtp.ehlo",
            "timestamp": "2026-11-11T22:00:00Z",
            "keyValues": {
                "0": {"key": "remoteIp", "value": {"@type": "IpAddr", "value": "198.51.100.7"}},
                "1": {"key": "elapsed", "value": {"@type": "Duration", "value": 12}},
                "2": {"key": "tls", "value": {"@type": "Boolean", "value": true}},
            },
        })
    }

    #[test]
    fn a_log_line_reads_time_event_and_values() {
        assert_eq!(
            log_line(&event()),
            "2026-11-11T22:00:00Z  smtp.ehlo remoteIp=198.51.100.7 elapsed=12ms tls=true"
        );
    }

    #[test]
    fn events_flatten_to_plain_json() {
        assert_eq!(
            flatten(&event()),
            json!({
                "timestamp": "2026-11-11T22:00:00Z",
                "event": "smtp.ehlo",
                "remoteIp": "198.51.100.7",
                "elapsed": 12,
                "tls": true,
            })
        );
    }

    #[test]
    fn a_nested_event_and_a_list_render_both_ways() {
        let value = json!({"@type": "Event", "event": "dns.error", "value": {
            "0": {"key": "code", "value": {"@type": "String", "value": "SERVFAIL"}},
        }});
        assert_eq!(human(&value), "dns.error{code=SERVFAIL}");
        assert_eq!(plain(&value), json!({"event": "dns.error", "code": "SERVFAIL"}));

        let list = json!({"@type": "List", "value": {
            "0": {"@type": "String", "value": "a"},
            "1": {"@type": "String", "value": "b"},
        }});
        assert_eq!(human(&list), "[a, b]");
        assert_eq!(plain(&list), json!(["a", "b"]));
    }

    #[test]
    fn a_key_value_pair_must_have_an_equals() {
        let got = params(Some("timeout".into()), &["remoteIp=198.51.100.7".into()]).unwrap();
        assert_eq!(
            got,
            vec![
                ("filter".to_owned(), "timeout".to_owned()),
                ("remoteIp".to_owned(), "198.51.100.7".to_owned()),
            ]
        );
        assert_eq!(params(None, &["noequals".into()]).unwrap_err().exit_code(), 2);
        assert!(params(Some("   ".into()), &[]).unwrap().is_empty());
    }
}
