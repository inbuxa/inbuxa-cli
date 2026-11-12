/*
 * SPDX-FileCopyrightText: 2026 Coffey Labs LLC
 *
 * SPDX-License-Identifier: AGPL-3.0-only
 */

//! `inbuxa-cli queue`: the outbound SMTP queue, the registry's
//! `x:QueuedMessage` objects.
//!
//! `list` shows what's waiting and why, `show` prints a message in full, and
//! `cancel` drops one with no delivery report. `actions` lists the actions the
//! server's queue view declares — retry, hold, release, whatever this server
//! offers — and `act` applies one by name. The command never hard-codes what
//! those actions do: it reads them from the schema the server serves, the way
//! the console does, so it follows the server rather than guessing.

use std::io::{IsTerminal, Write};

use clap::Subcommand;
use serde_json::{Map, Value, json};

use crate::client::{Client, set_error};
use crate::error::{Error, Result};
use crate::schema::Schema;

/// The registry type (and the console view of the same name) for the queue.
const QUEUE: &str = "QueuedMessage";

/// `list` shows at most this many by default; a queue under load can hold far
/// more than anyone wants on a terminal. `--limit 0` lifts it.
const DEFAULT_LIMIT: u64 = 100;

macro_rules! out {
    ($($arg:tt)*) => { writeln!(std::io::stdout(), $($arg)*)? };
}

#[derive(Subcommand)]
pub enum QueueCommand {
    /// List the messages waiting in the outbound queue
    List {
        /// A JMAP filter, as JSON (such as '{"to":"example.com"}')
        #[arg(long)]
        filter: Option<String>,
        /// Show at most this many, newest the server knows first; 0 for all
        #[arg(long, default_value_t = DEFAULT_LIMIT)]
        limit: u64,
        /// Print the messages as JSON instead of a table
        #[arg(long)]
        json: bool,
    },

    /// Print queued messages as JSON, in full, by id
    Show {
        #[arg(required = true)]
        ids: Vec<String>,
    },

    /// Drop messages from the queue; the sender gets no delivery report
    Cancel {
        #[arg(required = true)]
        ids: Vec<String>,
        /// Drop them without asking
        #[arg(long, short)]
        yes: bool,
    },

    /// List the actions this server's queue offers (retry, hold, release …)
    Actions {
        /// Print the actions as JSON
        #[arg(long)]
        json: bool,
    },

    /// Apply one of those actions to messages, by the name `actions` lists
    Act {
        /// The action's name, or enough of it to pick one out
        action: String,
        #[arg(required = true)]
        ids: Vec<String>,
        /// Apply it without asking
        #[arg(long, short)]
        yes: bool,
    },
}

pub fn run(client: &Client, schema: &Schema, command: QueueCommand) -> Result<()> {
    let object = schema.resolve(QUEUE)?;
    match command {
        QueueCommand::List { filter, limit, json } => list(client, schema, &object, filter, limit, json),
        QueueCommand::Show { ids } => show(client, &object, &ids),
        QueueCommand::Cancel { ids, yes } => cancel(client, &object, &ids, yes),
        QueueCommand::Actions { json } => actions(schema, &object, json),
        QueueCommand::Act { action, ids, yes } => act(client, schema, &object, &action, &ids, yes),
    }
}

// ── Listing ──

fn list(
    client: &Client,
    schema: &Schema,
    object: &str,
    filter: Option<String>,
    limit: u64,
    as_json: bool,
) -> Result<()> {
    let filter = parse_filter(filter)?;
    let (ids, total) = query(client, object, filter.as_ref(), limit)?;

    let columns = columns(schema, object);
    let properties: Vec<&str> = std::iter::once("id")
        .chain(columns.iter().map(|(name, _)| name.as_str()))
        .collect();
    let items = client.get_properties(object, &ids, Some(&properties))?;

    if as_json {
        return print_json(&Value::Array(items.into_iter().map(Value::Object).collect()));
    }

    if items.is_empty() {
        out!("the queue is empty");
        return Ok(());
    }

    let heads: Vec<&str> = std::iter::once("ID")
        .chain(columns.iter().map(|(_, label)| label.as_str()))
        .collect();
    let rows: Vec<Vec<String>> = items
        .iter()
        .map(|item| {
            std::iter::once(text(item, "id"))
                .chain(columns.iter().map(|(name, _)| cell(item.get(name))))
                .collect()
        })
        .collect();
    table(&heads, rows)?;

    if let Some(total) = total
        && total as usize > items.len()
    {
        eprintln!(
            "showing {} of {total}; raise --limit or narrow with --filter",
            items.len()
        );
    }
    Ok(())
}

/// The ids to show, and the total in the queue when the server counts it.
/// `limit` 0 means every message.
fn query(
    client: &Client,
    object: &str,
    filter: Option<&Value>,
    limit: u64,
) -> Result<(Vec<String>, Option<u64>)> {
    if limit == 0 {
        return Ok((client.all_ids(object, filter)?, None));
    }
    let mut args = json!({ "limit": limit, "calculateTotal": true });
    if let Some(filter) = filter {
        args["filter"] = filter.clone();
    }
    let answer = client.call(&format!("{object}/query"), args)?;
    let ids = answer
        .get("ids")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect())
        .unwrap_or_default();
    let total = answer.get("total").and_then(Value::as_u64);
    Ok((ids, total))
}

/// The columns the console's queue view shows, name and heading. Empty when
/// the server declares none, and `list` then shows only the id.
fn columns(schema: &Schema, object: &str) -> Vec<(String, String)> {
    schema
        .list_spec(object)
        .and_then(|list| list.get("columns"))
        .and_then(Value::as_array)
        .map(|cols| {
            cols.iter()
                .filter_map(|c| {
                    let name = c.get("name").and_then(Value::as_str)?;
                    let label = c.get("label").and_then(Value::as_str).unwrap_or(name);
                    Some((name.to_owned(), label.to_uppercase()))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn parse_filter(filter: Option<String>) -> Result<Option<Value>> {
    match filter {
        None => Ok(None),
        Some(text) => serde_json::from_str(&text)
            .map(Some)
            .map_err(|e| Error::Usage(format!("--filter isn't JSON: {e}"))),
    }
}

// ── Showing one ──

fn show(client: &Client, object: &str, ids: &[String]) -> Result<()> {
    let items = client.get(object, ids)?;
    let found: Vec<&str> = items
        .iter()
        .filter_map(|i| i.get("id").and_then(Value::as_str))
        .collect();
    for id in ids {
        if !found.contains(&id.as_str()) {
            eprintln!("no queued message {id}: it may already have been sent or dropped");
        }
    }
    match items.as_slice() {
        [] => Err(Error::Usage("no such queued message".into())),
        [one] if ids.len() == 1 => print_json(&Value::Object(one.clone())),
        _ => print_json(&Value::Array(items.into_iter().map(Value::Object).collect())),
    }
}

// ── Cancelling ──

fn cancel(client: &Client, object: &str, ids: &[String], yes: bool) -> Result<()> {
    if !yes {
        confirm(
            &format!(
                "Drop {} from the queue? The sender gets no delivery report.",
                count(ids.len(), "message")
            ),
            "nothing dropped",
        )?;
    }
    destroy(client, object, ids)
}

fn destroy(client: &Client, object: &str, ids: &[String]) -> Result<()> {
    let answer = client.call(&format!("{object}/set"), json!({ "destroy": ids }))?;
    let gone = answer
        .get("destroyed")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    for id in ids {
        if let Some(err) = answer.pointer(&format!("/notDestroyed/{id}")) {
            eprintln!("{id} not dropped: {}", set_error(err));
        }
    }
    if gone > 0 {
        out!("dropped {}", count(gone, "message"));
    }
    kept_back(ids.len(), gone, "dropped")
}

// ── Actions ──

/// One action the server's queue view declares.
#[derive(Debug)]
struct Action {
    /// A stable handle derived from the label, for the command line.
    name: String,
    label: String,
    kind: ActionKind,
}

#[derive(Debug)]
enum ActionKind {
    /// Write these properties onto each message (retry, hold, release …).
    SetProperty(Map<String, Value>),
    /// Drop each message, as `cancel` does.
    Delete,
}

fn item_actions(schema: &Schema, object: &str) -> Vec<Action> {
    schema
        .list_spec(object)
        .and_then(|list| list.get("itemActions"))
        .and_then(Value::as_array)
        .map(|acts| acts.iter().filter_map(parse_action).collect())
        .unwrap_or_default()
}

fn parse_action(value: &Value) -> Option<Action> {
    let label = value.get("label").and_then(Value::as_str)?;
    let kind = match value.get("type").and_then(Value::as_str)? {
        "setProperty" => {
            ActionKind::SetProperty(value.get("properties").and_then(Value::as_object).cloned()?)
        }
        "delete" => ActionKind::Delete,
        // `query` and `view` actions only navigate the console; they do
        // nothing to a message, so the command line leaves them out.
        _ => return None,
    };
    Some(Action {
        name: slug(label),
        label: label.to_owned(),
        kind,
    })
}

fn actions(schema: &Schema, object: &str, as_json: bool) -> Result<()> {
    let actions = item_actions(schema, object);
    if as_json {
        let array = actions
            .iter()
            .map(|a| {
                json!({
                    "name": a.name,
                    "label": a.label,
                    "sets": match &a.kind {
                        ActionKind::SetProperty(props) => Value::Object(props.clone()),
                        ActionKind::Delete => Value::Null,
                    },
                })
            })
            .collect();
        return print_json(&Value::Array(array));
    }
    if actions.is_empty() {
        out!("this server's queue offers no actions");
        return Ok(());
    }
    table(
        &["NAME", "LABEL", "DOES"],
        actions
            .iter()
            .map(|a| vec![a.name.clone(), a.label.clone(), does(&a.kind)])
            .collect(),
    )
}

fn act(
    client: &Client,
    schema: &Schema,
    object: &str,
    wanted: &str,
    ids: &[String],
    yes: bool,
) -> Result<()> {
    let actions = item_actions(schema, object);
    let action = pick(&actions, wanted)?;
    if !yes {
        confirm(
            &format!("{} on {}?", action.label, count(ids.len(), "message")),
            "nothing done",
        )?;
    }
    match &action.kind {
        ActionKind::Delete => destroy(client, object, ids),
        ActionKind::SetProperty(props) => {
            let update: Map<String, Value> = ids
                .iter()
                .map(|id| (id.clone(), Value::Object(props.clone())))
                .collect();
            let answer = client.call(&format!("{object}/set"), json!({ "update": update }))?;
            let done = answer
                .get("updated")
                .and_then(Value::as_object)
                .map_or(0, Map::len);
            for id in ids {
                if let Some(err) = answer.pointer(&format!("/notUpdated/{id}")) {
                    eprintln!("{id}: {}", set_error(err));
                }
            }
            if done > 0 {
                out!("{} on {}", action.label, count(done, "message"));
            }
            kept_back(ids.len(), done, "done")
        }
    }
}

/// The one action `wanted` names: its exact name, else the only one it's a
/// part of. Nothing matching, or more than one, is a usage error that points
/// back at `queue actions`.
fn pick<'a>(actions: &'a [Action], wanted: &str) -> Result<&'a Action> {
    let wanted = slug(wanted);
    if let Some(exact) = actions.iter().find(|a| a.name == wanted) {
        return Ok(exact);
    }
    let matches: Vec<&Action> = actions.iter().filter(|a| a.name.contains(&wanted)).collect();
    match matches.as_slice() {
        [one] => Ok(one),
        [] => Err(Error::Usage(format!(
            "no queue action `{wanted}`; `inbuxa-cli queue actions` lists them"
        ))),
        many => Err(Error::Usage(format!(
            "`{wanted}` matches {}; name one",
            many.iter()
                .map(|a| a.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}

fn does(kind: &ActionKind) -> String {
    match kind {
        ActionKind::Delete => "drops the message".to_owned(),
        ActionKind::SetProperty(props) => props
            .iter()
            .map(|(k, v)| format!("{k}={}", compact(v)))
            .collect::<Vec<_>>()
            .join(", "),
    }
}

// ── Shared helpers ──

/// A command-line handle from a label: "Retry now" becomes "retry-now".
fn slug(label: &str) -> String {
    let mut out = String::with_capacity(label.len());
    let mut dash = false;
    for c in label.chars() {
        if c.is_ascii_alphanumeric() {
            if dash && !out.is_empty() {
                out.push('-');
            }
            out.extend(c.to_lowercase());
            dash = false;
        } else {
            dash = true;
        }
    }
    out
}

/// A cell for the table: scalars as they read, an array of addresses or names
/// joined, anything deeper summarised. `--json` keeps the full value.
fn cell(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::Array(a)) => match a.iter().map(scalar).collect::<Option<Vec<_>>>() {
            // Every element reads as a scalar (addresses, names): join them.
            Some(parts) => {
                let shown: Vec<String> = parts.into_iter().filter(|s| !s.is_empty()).collect();
                shown.join(", ")
            }
            // Something in it is an opaque object or a nested array: count it.
            None => format!("[{}]", a.len()),
        },
        Some(Value::Object(o)) => {
            scalar(&Value::Object(o.clone())).unwrap_or_else(|| format!("{{{}}}", o.len()))
        }
    }
}

/// A value as a short string when it shows well in a cell — a scalar, or an
/// object's address or name — and `None` for an opaque object or nested
/// array, which a table should count rather than spell out.
fn scalar(value: &Value) -> Option<String> {
    match value {
        Value::Null => Some(String::new()),
        Value::String(s) => Some(s.clone()),
        Value::Bool(b) => Some(b.to_string()),
        Value::Number(n) => Some(n.to_string()),
        Value::Object(o) => ["address", "email", "addr", "name", "value"]
            .iter()
            .find_map(|k| o.get(*k).and_then(Value::as_str))
            .map(str::to_owned),
        Value::Array(_) => None,
    }
}

/// A short, one-line form of a JSON value for the actions table.
fn compact(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn text(item: &Map<String, Value>, key: &str) -> String {
    match item.get(key) {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
    }
}

/// "1 message" or "3 messages".
fn count(n: usize, noun: &str) -> String {
    if n == 1 {
        format!("1 {noun}")
    } else {
        format!("{n} {noun}s")
    }
}

/// An error when the server did fewer than were asked, so a script notices.
fn kept_back(asked: usize, done: usize, verb: &str) -> Result<()> {
    if done < asked {
        Err(Error::Partial(format!("{} of {asked} not {verb}", asked - done)))
    } else {
        Ok(())
    }
}

fn confirm(question: &str, refused: &str) -> Result<()> {
    if !std::io::stdin().is_terminal() {
        return Err(Error::Usage(format!(
            "{question} Give --yes to do it without asking"
        )));
    }
    eprint!("{question} [y/N] ");
    std::io::stderr().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    if matches!(answer.trim(), "y" | "Y" | "yes") {
        Ok(())
    } else {
        Err(Error::Usage(refused.into()))
    }
}

fn print_json(value: &Value) -> Result<()> {
    let mut out = std::io::stdout().lock();
    serde_json::to_writer_pretty(&mut out, value)?;
    writeln!(out)?;
    Ok(())
}

fn table(heads: &[&str], rows: Vec<Vec<String>>) -> Result<()> {
    let widths: Vec<usize> = heads
        .iter()
        .enumerate()
        .map(|(n, h)| {
            rows.iter()
                .map(|r| r.get(n).map_or(0, |c| c.chars().count()))
                .chain([h.chars().count()])
                .max()
                .unwrap_or(0)
        })
        .collect();
    let line = |cells: &[String]| -> Result<()> {
        let last = cells.len().saturating_sub(1);
        let parts: Vec<String> = cells
            .iter()
            .enumerate()
            .map(|(i, c)| {
                if i == last {
                    c.clone()
                } else {
                    format!("{c:w$}", w = widths[i])
                }
            })
            .collect();
        out!("{}", parts.join("  ").trim_end());
        Ok(())
    };
    line(&heads.iter().map(|h| (*h).to_owned()).collect::<Vec<_>>())?;
    for row in rows {
        line(&row)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn object(value: Value) -> Map<String, Value> {
        value.as_object().unwrap().clone()
    }

    #[test]
    fn a_label_becomes_a_command_line_name() {
        assert_eq!(slug("Retry now"), "retry-now");
        assert_eq!(slug("Hold"), "hold");
        assert_eq!(slug("Release from hold"), "release-from-hold");
        assert_eq!(slug("  spaced  "), "spaced");
        assert_eq!(slug("already-a-slug"), "already-a-slug");
    }

    #[test]
    fn a_cell_reads_addresses_out_of_a_recipients_array() {
        let recipients = json!([
            {"address": "a@example.com", "status": "queued"},
            {"address": "b@example.net", "status": "failed"},
        ]);
        assert_eq!(cell(Some(&recipients)), "a@example.com, b@example.net");
        assert_eq!(cell(Some(&json!(["x@a.com", "y@b.com"]))), "x@a.com, y@b.com");
        assert_eq!(cell(Some(&json!([]))), "");
        assert_eq!(cell(Some(&json!([{"k": 1}, {"k": 2}]))), "[2]");
        assert_eq!(cell(None), "");
        assert_eq!(cell(Some(&json!(3))), "3");
        assert_eq!(cell(Some(&Value::Null)), "");
    }

    #[test]
    fn columns_come_from_the_schema_list() {
        let schema = Schema::new(json!({
            "objects": {"x:QueuedMessage": {"type": "object"}},
            "schemas": {},
            "fields": {},
            "lists": {"x:QueuedMessage": {"columns": [
                {"name": "recipients", "label": "To"},
                {"name": "nextRetry", "label": "Next try"},
            ]}},
        }))
        .unwrap();
        assert_eq!(
            columns(&schema, "x:QueuedMessage"),
            vec![
                ("recipients".to_owned(), "TO".to_owned()),
                ("nextRetry".to_owned(), "NEXT TRY".to_owned()),
            ]
        );
        assert!(columns(&schema, "x:Missing").is_empty());
    }

    fn actions() -> Vec<Action> {
        vec![
            Action {
                name: "retry-now".into(),
                label: "Retry now".into(),
                kind: ActionKind::SetProperty(object(json!({"retry": true}))),
            },
            Action {
                name: "hold".into(),
                label: "Hold".into(),
                kind: ActionKind::SetProperty(object(json!({"held": true}))),
            },
            Action {
                name: "release".into(),
                label: "Release".into(),
                kind: ActionKind::SetProperty(object(json!({"held": false}))),
            },
        ]
    }

    #[test]
    fn pick_takes_an_exact_name_then_an_unambiguous_part() {
        let actions = actions();
        assert_eq!(pick(&actions, "hold").unwrap().name, "hold");
        assert_eq!(pick(&actions, "Retry now").unwrap().name, "retry-now");
        // "retry" is only part of one name
        assert_eq!(pick(&actions, "retry").unwrap().name, "retry-now");
    }

    #[test]
    fn pick_refuses_the_unknown_and_the_ambiguous() {
        let actions = actions();
        assert_eq!(pick(&actions, "resume").unwrap_err().exit_code(), 2);
        // "re" is in both retry-now and release
        let err = pick(&actions, "re").unwrap_err();
        assert_eq!(err.exit_code(), 2);
        assert!(err.to_string().contains("retry-now"), "{err}");
    }

    #[test]
    fn parse_action_keeps_only_what_changes_a_message() {
        assert!(
            parse_action(&json!({"type": "setProperty", "label": "Hold", "properties": {"held": true}}))
                .is_some()
        );
        assert!(parse_action(&json!({"type": "delete", "label": "Cancel"})).is_some());
        assert!(parse_action(&json!({"type": "view", "label": "Open", "objectName": "x:Account"})).is_none());
        assert!(parse_action(&json!({"type": "separator"})).is_none());
    }

    #[test]
    fn a_filter_must_be_json() {
        assert!(parse_filter(None).unwrap().is_none());
        assert_eq!(
            parse_filter(Some(r#"{"to":"example.com"}"#.into()))
                .unwrap()
                .unwrap(),
            json!({"to": "example.com"})
        );
        assert_eq!(parse_filter(Some("not json".into())).unwrap_err().exit_code(), 2);
    }

    #[test]
    fn shortfall_is_a_partial_failure() {
        assert!(kept_back(3, 3, "dropped").is_ok());
        assert_eq!(kept_back(3, 1, "dropped").unwrap_err().exit_code(), 5);
        assert_eq!(count(1, "message"), "1 message");
        assert_eq!(count(2, "message"), "2 messages");
    }
}
