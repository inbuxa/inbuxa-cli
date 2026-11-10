/*
 * SPDX-FileCopyrightText: 2026 Coffey Labs LLC
 *
 * SPDX-License-Identifier: AGPL-3.0-only
 */

//! `inbuxa-cli store`: the store check, store snapshots and blob store
//! changes (inbuxa-server docs/spec/features/store-check.md,
//! store-snapshot.md, blob-store-change.md). They're under
//! `urn:inbuxa:jmap`, not the registry, so the registry commands don't reach
//! them.
//!
//! The commands that start something wait for it by default and end with an
//! exit status a script can trust: 0 when it's done and clean, 1 when a check
//! or a copy found problems, and the usual errors otherwise.

use std::io::{IsTerminal, Write};
use std::time::Duration;

use clap::Subcommand;
use serde_json::{Map, Value, json};

use crate::client::{Client, set_error};
use crate::error::{Error, Result};

const STORE_CHECK: &str = "inbuxa:StoreCheck";
const STORE_SNAPSHOT: &str = "inbuxa:StoreSnapshot";
const BLOB_STORE_CHANGE: &str = "inbuxa:BlobStoreChange";

/// How often a wait looks again. The server saves progress every 10 seconds;
/// a snapshot is usually done within a few.
const POLL: Duration = Duration::from_secs(2);

macro_rules! out {
    ($($arg:tt)*) => { writeln!(std::io::stdout(), $($arg)*)? };
}

#[derive(Subcommand)]
pub enum StoreCommand {
    /// Check that every blob the store's data needs is there, and wait for
    /// the result. Exits 1 when it found problems
    Check {
        /// Also read every blob back and check its content against its id
        #[arg(long)]
        full: bool,
        /// Blobs a second at most (the server's default is 1,000; 0 is no limit)
        #[arg(long)]
        blobs_per_second: Option<u64>,
        /// Bytes a second at most, in a full check (default 20 MiB; 0 is no limit)
        #[arg(long)]
        bytes_per_second: Option<u64>,
        /// Print the check's id and return without waiting
        #[arg(long)]
        no_wait: bool,
        /// Print the finished check as JSON
        #[arg(long)]
        json: bool,
    },

    /// List the latest checks, or show one with its problems. Exits 1 when
    /// the one shown found problems
    Checks {
        id: Option<String>,
        #[arg(long)]
        json: bool,
    },

    /// Stop a running check
    StopCheck { id: String },

    /// Take a snapshot of the store, wait for it, and print its directory
    Snapshot {
        /// Print the snapshot's id and return without waiting
        #[arg(long)]
        no_wait: bool,
        /// Print the finished snapshot as JSON
        #[arg(long)]
        json: bool,
    },

    /// List the snapshots on disk
    Snapshots {
        #[arg(long)]
        json: bool,
    },

    /// Delete a snapshot's directory
    DeleteSnapshot { id: String },

    /// List the blob store changes, or show one with its problems
    Changes {
        id: Option<String>,
        #[arg(long)]
        json: bool,
    },

    /// Copy what a blob store change left in the old store to the new one,
    /// and wait for the result. Exits 1 when some blobs couldn't be copied
    Copy {
        id: String,
        #[arg(long)]
        blobs_per_second: Option<u64>,
        #[arg(long)]
        bytes_per_second: Option<u64>,
        /// Return once it has started
        #[arg(long)]
        no_wait: bool,
    },

    /// Stop a running copy
    StopCopy { id: String },

    /// Forget a blob store change. The server stops reading through to its
    /// old store; nothing is deleted from either store
    Dismiss {
        id: String,
        /// Dismiss a change that isn't settled, without asking
        #[arg(long, short)]
        yes: bool,
    },
}

pub fn run(client: &Client, command: StoreCommand) -> Result<()> {
    match command {
        StoreCommand::Check {
            full,
            blobs_per_second,
            bytes_per_second,
            no_wait,
            json,
        } => {
            let mut create = json!({ "mode": if full { "full" } else { "quick" } });
            limits(&mut create, blobs_per_second, bytes_per_second);
            let id = create_one(client, STORE_CHECK, create)?;
            if no_wait {
                out!("{id}");
                return Ok(());
            }
            let check = wait(client, STORE_CHECK, &id, "running", |c| {
                format!("checking {} · {} blobs", percent(c), number(c, "blobsChecked"))
            })?;
            report_check(&check, json)
        }
        StoreCommand::Checks { id: Some(id), json } => {
            report_check(&get_one(client, STORE_CHECK, &id)?, json)
        }
        StoreCommand::Checks { id: None, json } => {
            let checks = newest_first(get_all(client, STORE_CHECK)?, "createdAt");
            if json {
                return print_json(&Value::Array(checks.into_iter().map(Value::Object).collect()));
            }
            table(
                &["ID", "MODE", "STATUS", "STARTED", "PROBLEMS"],
                checks
                    .iter()
                    .map(|c| {
                        vec![
                            text(c, "id"),
                            text(c, "mode"),
                            text(c, "status"),
                            text(c, "createdAt"),
                            check_problems(c).to_string(),
                        ]
                    })
                    .collect(),
            )
        }
        StoreCommand::StopCheck { id } => {
            update_one(client, STORE_CHECK, &id, json!({ "action": "stop" }))?;
            out!("stopping {id}: it stops at its next save, within about ten seconds");
            Ok(())
        }
        StoreCommand::Snapshot { no_wait, json } => {
            let id = create_one(client, STORE_SNAPSHOT, json!({}))?;
            if no_wait {
                out!("{id}");
                return Ok(());
            }
            let snapshot = wait(client, STORE_SNAPSHOT, &id, "running", |_| {
                "taking a snapshot".into()
            })?;
            if json {
                print_json(&Value::Object(snapshot.clone()))?;
            }
            match status(&snapshot) {
                "done" => {
                    if !json {
                        out!("{}", text(&snapshot, "path"));
                    }
                    Ok(())
                }
                other => Err(Error::Partial(format!(
                    "the snapshot {other}: {}",
                    text(&snapshot, "error")
                ))),
            }
        }
        StoreCommand::Snapshots { json } => {
            let snapshots = newest_first(get_all(client, STORE_SNAPSHOT)?, "createdAt");
            if json {
                return print_json(&Value::Array(snapshots.into_iter().map(Value::Object).collect()));
            }
            table(
                &["ID", "NAME", "STATUS", "SIZE", "PATH"],
                snapshots
                    .iter()
                    .map(|s| {
                        vec![
                            text(s, "id"),
                            text(s, "name"),
                            text(s, "status"),
                            size(s.get("size").and_then(Value::as_u64).unwrap_or(0)),
                            text(s, "path"),
                        ]
                    })
                    .collect(),
            )
        }
        StoreCommand::DeleteSnapshot { id } => {
            destroy_one(client, STORE_SNAPSHOT, &id)?;
            out!("deleted {id}");
            Ok(())
        }
        StoreCommand::Changes { id: Some(id), json } => {
            report_change(&get_one(client, BLOB_STORE_CHANGE, &id)?, json)
        }
        StoreCommand::Changes { id: None, json } => {
            let changes = newest_first(get_all(client, BLOB_STORE_CHANGE)?, "changedAt");
            if json {
                return print_json(&Value::Array(changes.into_iter().map(Value::Object).collect()));
            }
            table(
                &["ID", "STATUS", "CHANGED", "FROM", "TO"],
                changes
                    .iter()
                    .map(|c| {
                        vec![
                            text(c, "id"),
                            text(c, "status"),
                            text(c, "changedAt"),
                            text(c, "from"),
                            text(c, "to"),
                        ]
                    })
                    .collect(),
            )
        }
        StoreCommand::Copy {
            id,
            blobs_per_second,
            bytes_per_second,
            no_wait,
        } => {
            let mut patch = json!({ "action": "copy" });
            limits(&mut patch, blobs_per_second, bytes_per_second);
            update_one(client, BLOB_STORE_CHANGE, &id, patch)?;
            if no_wait {
                out!("copying {id}");
                return Ok(());
            }
            let change = wait(client, BLOB_STORE_CHANGE, &id, "copying", |c| {
                format!("copying {} · {} copied", percent(c), number(c, "copied"))
            })?;
            report_change(&change, false)
        }
        StoreCommand::StopCopy { id } => {
            update_one(client, BLOB_STORE_CHANGE, &id, json!({ "action": "stop" }))?;
            out!("stopping the copy of {id}: what it copied stays copied");
            Ok(())
        }
        StoreCommand::Dismiss { id, yes } => {
            let change = get_one(client, BLOB_STORE_CHANGE, &id)?;
            if is_unresolved(&change) && !yes {
                confirm(&format!(
                    "{id} isn't settled ({}): the server stops reading from {}, and anything only it has will be missing. Dismiss it?",
                    status(&change),
                    text(&change, "from")
                ))?;
            }
            destroy_one(client, BLOB_STORE_CHANGE, &id)?;
            out!("dismissed {id}; nothing was deleted from either store");
            Ok(())
        }
    }
}

// ── Talking to the server ──

fn limits(args: &mut Value, blobs: Option<u64>, bytes: Option<u64>) {
    if let Some(n) = blobs {
        args["blobsPerSecond"] = json!(n);
    }
    if let Some(n) = bytes {
        args["bytesPerSecond"] = json!(n);
    }
}

fn get_all(client: &Client, object: &str) -> Result<Vec<Map<String, Value>>> {
    let answer = client.call_inbuxa(&format!("{object}/get"), json!({ "ids": null }))?;
    Ok(list_of(&answer))
}

fn get_one(client: &Client, object: &str, id: &str) -> Result<Map<String, Value>> {
    let answer = client.call_inbuxa(&format!("{object}/get"), json!({ "ids": [id] }))?;
    list_of(&answer)
        .into_iter()
        .next()
        .ok_or_else(|| Error::Usage(format!("no {object} {id}")))
}

fn list_of(answer: &Value) -> Vec<Map<String, Value>> {
    answer
        .get("list")
        .and_then(Value::as_array)
        .map(|l| l.iter().filter_map(|v| v.as_object().cloned()).collect())
        .unwrap_or_default()
}

/// The new object's id, or why the server refused it: another data store,
/// one already running, a blob store change not settled.
fn create_one(client: &Client, object: &str, create: Value) -> Result<String> {
    let answer = client.call_inbuxa(&format!("{object}/set"), json!({ "create": { "c": create } }))?;
    if let Some(err) = answer.pointer("/notCreated/c") {
        return Err(Error::Server(format!("refused: {}", set_error(err))));
    }
    answer
        .pointer("/created/c/id")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| Error::Transport(format!("{object}/set created nothing")))
}

fn update_one(client: &Client, object: &str, id: &str, patch: Value) -> Result<()> {
    let answer = client.call_inbuxa(&format!("{object}/set"), json!({ "update": { id: patch } }))?;
    match answer.get("notUpdated").and_then(|n| n.get(id)) {
        Some(err) => Err(Error::Server(format!("refused: {}", set_error(err)))),
        None => Ok(()),
    }
}

fn destroy_one(client: &Client, object: &str, id: &str) -> Result<()> {
    let answer = client.call_inbuxa(&format!("{object}/set"), json!({ "destroy": [id] }))?;
    match answer.get("notDestroyed").and_then(|n| n.get(id)) {
        Some(err) => Err(Error::Server(format!("refused: {}", set_error(err)))),
        None => Ok(()),
    }
}

/// Looks again until the object leaves `running`, showing `progress` on a
/// terminal. Interrupting stops the wait, not the job.
fn wait(
    client: &Client,
    object: &str,
    id: &str,
    running: &str,
    progress: impl Fn(&Map<String, Value>) -> String,
) -> Result<Map<String, Value>> {
    let shown = std::io::stderr().is_terminal();
    if shown {
        eprintln!("{id} started; Ctrl-C stops waiting, not the job");
    }
    loop {
        let item = get_one(client, object, id)?;
        if status(&item) != running {
            if shown {
                eprint!("\r\x1b[K");
            }
            return Ok(item);
        }
        if shown {
            eprint!("\r\x1b[K{}", progress(&item));
        }
        std::thread::sleep(POLL);
    }
}

fn confirm(question: &str) -> Result<()> {
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
        Err(Error::Usage("not dismissed".into()))
    }
}

// ── Reports ──

fn report_check(check: &Map<String, Value>, as_json: bool) -> Result<()> {
    if as_json {
        print_json(&Value::Object(check.clone()))?;
    } else {
        out!(
            "{} check {}: {}{}",
            text(check, "mode"),
            text(check, "id"),
            status(check),
            match text(check, "error") {
                e if e.is_empty() => String::new(),
                e => format!(" ({e})"),
            }
        );
        out!(
            "{} blobs, {} links, {} messages checked{}",
            number(check, "blobsChecked"),
            number(check, "linksChecked"),
            number(check, "messagesChecked"),
            if text(check, "mode") == "full" {
                format!(
                    ", {} read",
                    size(check.get("bytesRead").and_then(Value::as_u64).unwrap_or(0))
                )
            } else {
                String::new()
            }
        );
        let counts = counts(check, &["missing", "damaged", "unlinked", "unreadable"]);
        if !counts.is_empty() {
            out!("{counts}");
            let problems = problems(check);
            table(
                &["PROBLEM", "ACCOUNT", "OBJECT", "ID", "BLOB", "DETAIL"],
                problems
                    .iter()
                    .map(|p| {
                        let detail = match (
                            text(p, "kind").as_str(),
                            p.get("committed").and_then(Value::as_bool),
                        ) {
                            ("missing", Some(true)) => join(&text(p, "detail"), "stored, then lost"),
                            ("missing", _) => join(&text(p, "detail"), "never finished being stored"),
                            _ => text(p, "detail"),
                        };
                        vec![
                            text(p, "kind"),
                            first(&[text(p, "accountName"), text(p, "accountId")]),
                            text(p, "object"),
                            text(p, "objectId"),
                            text(p, "blobId"),
                            detail,
                        ]
                    })
                    .collect(),
            )?;
            kept_note(problems.len() as u64, check_problems(check))?;
        }
    }
    outcome(status(check), check_problems(check), "the check")
}

fn report_change(change: &Map<String, Value>, as_json: bool) -> Result<()> {
    if as_json {
        print_json(&Value::Object(change.clone()))?;
    } else {
        out!(
            "change {}: {} → {}, {}",
            text(change, "id"),
            text(change, "from"),
            text(change, "to"),
            status(change)
        );
        if is_unresolved(change) {
            out!("not settled: the server still reads what it can't find from the old store");
        }
        if change.get("copyStartedAt").is_some_and(|v| !v.is_null()) {
            out!(
                "latest copy: {} blobs looked at, {} copied, {} already there, {} copied in all{}",
                number(change, "blobsChecked"),
                number(change, "copied"),
                number(change, "alreadyThere"),
                size(change.get("bytesCopied").and_then(Value::as_u64).unwrap_or(0)),
                match text(change, "error") {
                    e if e.is_empty() => String::new(),
                    e => format!(" ({e})"),
                }
            );
        }
        let counts = counts(change, &["missing", "damaged", "unreadable"]);
        if !counts.is_empty() {
            out!("{counts}");
            let problems = problems(change);
            table(
                &["PROBLEM", "HELD BY", "ACCOUNT", "ID", "BLOB", "DETAIL"],
                problems
                    .iter()
                    .map(|p| {
                        vec![
                            text(p, "kind"),
                            text(p, "holder"),
                            text(p, "accountId"),
                            text(p, "objectId"),
                            text(p, "blobId"),
                            text(p, "detail"),
                        ]
                    })
                    .collect(),
            )?;
            kept_note(problems.len() as u64, change_problems(change))?;
        }
    }
    match status(change) {
        "incomplete" => Err(Error::Found(format!(
            "the copy couldn't copy {} blob(s); the change isn't settled",
            change_problems(change)
        ))),
        "stopped" | "failed" | "interrupted" => Err(Error::Partial(format!(
            "the copy {}; the change isn't settled",
            status(change)
        ))),
        _ => Ok(()),
    }
}

/// What a finished check means for the exit status.
fn outcome(status: &str, problems: u64, what: &str) -> Result<()> {
    match status {
        "done" if problems == 0 => Ok(()),
        "done" => Err(Error::Found(format!("{what} found {problems} problem(s)"))),
        "running" => Ok(()),
        other => Err(Error::Partial(format!("{what} {other}"))),
    }
}

fn kept_note(kept: u64, total: u64) -> Result<()> {
    if total > kept {
        eprintln!("the first {kept} of {total} are kept; every one is in the log as store.data-corruption");
    }
    Ok(())
}

// ── Reading objects ──

fn status(item: &Map<String, Value>) -> &str {
    item.get("status").and_then(Value::as_str).unwrap_or("")
}

fn text(item: &Map<String, Value>, key: &str) -> String {
    match item.get(key) {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
    }
}

fn count(item: &Map<String, Value>, key: &str) -> u64 {
    item.get(key).and_then(Value::as_u64).unwrap_or(0)
}

fn number(item: &Map<String, Value>, key: &str) -> String {
    thousands(count(item, key))
}

fn check_problems(check: &Map<String, Value>) -> u64 {
    ["missing", "damaged", "unlinked", "unreadable"]
        .iter()
        .map(|k| count(check, k))
        .sum()
}

fn change_problems(change: &Map<String, Value>) -> u64 {
    ["missing", "damaged", "unreadable"]
        .iter()
        .map(|k| count(change, k))
        .sum()
}

/// BM-3: a change the server still reads through.
fn is_unresolved(change: &Map<String, Value>) -> bool {
    !matches!(status(change), "copied" | "reverted")
}

fn problems(item: &Map<String, Value>) -> Vec<Map<String, Value>> {
    item.get("problems")
        .and_then(Value::as_array)
        .map(|l| l.iter().filter_map(|v| v.as_object().cloned()).collect())
        .unwrap_or_default()
}

/// "missing 1500 · damaged 1", leaving out the kinds with none.
fn counts(item: &Map<String, Value>, kinds: &[&str]) -> String {
    kinds
        .iter()
        .filter(|k| count(item, k) > 0)
        .map(|k| format!("{k} {}", thousands(count(item, k))))
        .collect::<Vec<_>>()
        .join(" · ")
}

fn percent(item: &Map<String, Value>) -> String {
    let share = item.get("progress").and_then(Value::as_f64).unwrap_or(0.0);
    format!("{}%", (share.clamp(0.0, 1.0) * 100.0).floor() as u32)
}

fn newest_first(mut items: Vec<Map<String, Value>>, date: &str) -> Vec<Map<String, Value>> {
    items.sort_by_key(|item| std::cmp::Reverse(text(item, date)));
    items
}

fn first(values: &[String]) -> String {
    values.iter().find(|v| !v.is_empty()).cloned().unwrap_or_default()
}

fn join(a: &str, b: &str) -> String {
    if a.is_empty() {
        b.to_owned()
    } else {
        format!("{a}; {b}")
    }
}

fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
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
                .map(|r| r[n].chars().count())
                .chain([h.len()])
                .max()
                .unwrap_or(0)
        })
        .collect();
    let line = |cells: Vec<String>| -> Result<()> {
        let last = cells.len() - 1;
        let parts: Vec<String> = cells
            .into_iter()
            .enumerate()
            .map(|(i, c)| {
                if i == last {
                    c
                } else {
                    format!("{c:w$}", w = widths[i])
                }
            })
            .collect();
        out!("{}", parts.join("  ").trim_end());
        Ok(())
    };
    line(heads.iter().map(|h| (*h).to_owned()).collect())?;
    for row in rows {
        line(row)?;
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
    fn a_clean_check_exits_0_and_problems_exit_1() {
        assert!(outcome("done", 0, "the check").is_ok());
        let found = outcome("done", 3, "the check").unwrap_err();
        assert_eq!(found.exit_code(), 1);
        assert_eq!(found.to_string(), "the check found 3 problem(s)");
        assert_eq!(outcome("interrupted", 0, "the check").unwrap_err().exit_code(), 5);
        assert_eq!(outcome("failed", 0, "the check").unwrap_err().exit_code(), 5);
    }

    #[test]
    fn problems_count_past_the_ones_kept() {
        let check = object(json!({"missing": 1500, "damaged": 1, "unlinked": 0, "problems": []}));
        assert_eq!(check_problems(&check), 1501);
        assert_eq!(
            counts(&check, &["missing", "damaged", "unlinked"]),
            "missing 1,500 · damaged 1"
        );
    }

    #[test]
    fn a_change_is_settled_only_once_copied_or_moved_back() {
        for s in [
            "pending",
            "copying",
            "incomplete",
            "stopped",
            "failed",
            "interrupted",
        ] {
            assert!(is_unresolved(&object(json!({ "status": s }))), "{s}");
        }
        for s in ["copied", "reverted"] {
            assert!(!is_unresolved(&object(json!({ "status": s }))), "{s}");
        }
    }

    #[test]
    fn a_copy_that_left_blobs_behind_exits_1() {
        let incomplete = object(json!({"id": "a", "status": "incomplete", "missing": 2, "problems": []}));
        assert_eq!(report_change(&incomplete, true).unwrap_err().exit_code(), 1);
        let stopped = object(json!({"id": "a", "status": "stopped"}));
        assert_eq!(report_change(&stopped, true).unwrap_err().exit_code(), 5);
        let copied = object(json!({"id": "a", "status": "copied"}));
        assert!(report_change(&copied, true).is_ok());
    }

    #[test]
    fn limits_are_sent_only_when_given() {
        let mut args = json!({"mode": "quick"});
        limits(&mut args, None, Some(0));
        assert_eq!(args, json!({"mode": "quick", "bytesPerSecond": 0}));
    }

    #[test]
    fn numbers_read_as_people_write_them() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1_234_567), "1,234,567");
        assert_eq!(size(15), "15 B");
        assert_eq!(size(20 * 1024 * 1024), "20.0 MiB");
        assert_eq!(percent(&object(json!({"progress": 0.429}))), "42%");
    }

    #[test]
    fn lists_are_newest_first() {
        let items = vec![
            object(json!({"createdAt": "2026-11-09T00:00:00Z"})),
            object(json!({"createdAt": "2026-11-10T00:00:00Z"})),
        ];
        let sorted = newest_first(items, "createdAt");
        assert_eq!(text(&sorted[0], "createdAt"), "2026-11-10T00:00:00Z");
    }
}
