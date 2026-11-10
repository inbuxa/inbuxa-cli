/*
 * SPDX-FileCopyrightText: 2026 Coffey Labs LLC
 *
 * SPDX-License-Identifier: AGPL-3.0-only
 */

//! `inbuxa-cli store`: the store check, store snapshots and blob store
//! changes (inbuxa-server docs/spec/features/store-check.md,
//! store-snapshot.md, blob-store-change.md, blob-store-copy-ahead.md,
//! blob-store-cleanup.md).
//! They're under
//! `urn:inbuxa:jmap`, not the registry, so the registry commands don't reach
//! them.
//!
//! The commands that start something wait for it by default and end with an
//! exit status a script can trust: 0 when it's done and clean, 1 when a check,
//! a copy or a cleanup found problems, and the usual errors otherwise.

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

    /// Plan a blob store change before the setting moves: the new store's
    /// settings, as `x:BlobStore` takes them, from FILE or `-` for standard
    /// input. Prints the change's id; copy it ahead with `store copy`
    Plan { file: String },

    /// Copy what a blob store change left in the old store to the new one,
    /// or a planned change's blobs ahead to its store, and wait for the
    /// result. Exits 1 when some blobs couldn't be copied
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

    /// Delete from a settled change's old store each blob the current store
    /// has and checks out, and wait for the result. A blob that doesn't check
    /// out stays and is listed; exits 1 when any did. Backups of the data
    /// store taken before the change need the old store to restore in full
    CleanUp {
        id: String,
        /// Count what would be deleted, and delete nothing
        #[arg(long)]
        dry_run: bool,
        /// Delete without asking: backups from before the change have aged out
        #[arg(long, short)]
        yes: bool,
        #[arg(long)]
        blobs_per_second: Option<u64>,
        #[arg(long)]
        bytes_per_second: Option<u64>,
        /// Return once it has started
        #[arg(long)]
        no_wait: bool,
    },

    /// Stop a running cleanup or preview
    StopCleanUp { id: String },

    /// Write a planned change's store to the blob store setting, once its
    /// copy ahead ended copied. It takes effect when the nodes restart
    Apply { id: String },

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
            let changes = changes_first_planned(get_all(client, BLOB_STORE_CHANGE)?);
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
                            match cleanup(c) {
                                _ if is_planned(c) => format!("planned, {}", status(c)),
                                Some(run) if is_dry_run(run) => {
                                    format!("{}, preview {}", status(c), status(run))
                                }
                                Some(run) => format!("{}, cleanup {}", status(c), status(run)),
                                None => text(c, "status"),
                            },
                            text(c, "changedAt"),
                            text(c, "from"),
                            text(c, "to"),
                        ]
                    })
                    .collect(),
            )
        }
        StoreCommand::Plan { file } => {
            let target = read_target(&file)?;
            let id = create_one(client, BLOB_STORE_CHANGE, json!({ "target": target }))?;
            out!("{id}");
            if std::io::stderr().is_terminal() {
                eprintln!(
                    "planned; copy it ahead with `inbuxa-cli store copy {id}`, then `inbuxa-cli store apply {id}`"
                );
            }
            Ok(())
        }
        StoreCommand::Apply { id } => {
            update_one(client, BLOB_STORE_CHANGE, &id, json!({ "action": "apply" }))?;
            let change = get_one(client, BLOB_STORE_CHANGE, &id)?;
            out!(
                "the blob store setting now names {}; it takes effect when each node restarts",
                text(&change, "to")
            );
            Ok(())
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
        StoreCommand::CleanUp {
            id,
            dry_run,
            yes,
            blobs_per_second,
            bytes_per_second,
            no_wait,
        } => {
            let mut patch = if dry_run {
                json!({ "action": "cleanUp", "dryRun": true })
            } else {
                if !yes {
                    let change = get_one(client, BLOB_STORE_CHANGE, &id)?;
                    confirm(
                        &format!(
                            "Backups of the data store taken before {} need {}'s blobs to restore in full. Delete the server's blobs from {}?",
                            text(&change, "changedAt"),
                            text(&change, "from"),
                            text(&change, "from")
                        ),
                        "nothing deleted",
                    )?;
                }
                json!({ "action": "cleanUp", "confirm": id })
            };
            limits(&mut patch, blobs_per_second, bytes_per_second);
            update_one(client, BLOB_STORE_CHANGE, &id, patch)?;
            if no_wait {
                out!(
                    "{} {id}",
                    if dry_run {
                        "previewing the cleanup of"
                    } else {
                        "cleaning up"
                    }
                );
                return Ok(());
            }
            let change = wait_cleanup(client, &id)?;
            report_cleanup(&change)
        }
        StoreCommand::StopCleanUp { id } => {
            let change = get_one(client, BLOB_STORE_CHANGE, &id)?;
            let preview = cleanup(&change).is_some_and(is_dry_run);
            update_one(client, BLOB_STORE_CHANGE, &id, json!({ "action": "stop" }))?;
            if preview {
                out!("stopping the cleanup preview of {id}");
            } else {
                out!("stopping the cleanup of {id}: what it deleted stays deleted");
            }
            Ok(())
        }
        StoreCommand::Dismiss { id, yes } => {
            let change = get_one(client, BLOB_STORE_CHANGE, &id)?;
            if is_planned(&change) {
                destroy_one(client, BLOB_STORE_CHANGE, &id)?;
                out!(
                    "dismissed {id}; what was copied ahead stays in {}",
                    text(&change, "to")
                );
                return Ok(());
            }
            if is_unresolved(&change) && !yes {
                confirm(
                    &format!(
                        "{id} isn't settled ({}): the server stops reading from {}, and anything only it has will be missing. Dismiss it?",
                        status(&change),
                        text(&change, "from")
                    ),
                    "not dismissed",
                )?;
            }
            destroy_one(client, BLOB_STORE_CHANGE, &id)?;
            out!("dismissed {id}; nothing was deleted from either store");
            Ok(())
        }
    }
}

/// The new store's settings for `store plan`: a JSON object, from a file or
/// standard input, never the command line, since it can hold credentials.
fn read_target(file: &str) -> Result<Value> {
    let text = if file == "-" {
        std::io::read_to_string(std::io::stdin())?
    } else {
        std::fs::read_to_string(file).map_err(|e| Error::Usage(format!("{file}: {e}")))?
    };
    parse_target(&text, file)
}

fn parse_target(text: &str, file: &str) -> Result<Value> {
    let value: Value =
        serde_json::from_str(text).map_err(|e| Error::Usage(format!("{file}: not JSON: {e}")))?;
    if value.get("@type").and_then(Value::as_str).is_none() {
        return Err(Error::Usage(format!(
            "{file}: a blob store setting, as x:BlobStore takes it, with its @type \
             (`inbuxa-cli describe BlobStore` shows them)"
        )));
    }
    Ok(value)
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
            "{}change {}: {} → {}, {}",
            if is_planned(change) { "planned " } else { "" },
            text(change, "id"),
            text(change, "from"),
            text(change, "to"),
            status(change)
        );
        if is_planned(change) {
            match (status(change), text(change, "appliedAt")) {
                (_, at) if !at.is_empty() => out!(
                    "applied {at} by {}: it becomes the change when the nodes restart",
                    text(change, "appliedBy")
                ),
                ("copied", _) => out!(
                    "copied ahead; apply it with `inbuxa-cli store apply {}`",
                    text(change, "id")
                ),
                _ => out!(
                    "not copied ahead yet: `inbuxa-cli store copy {}`",
                    text(change, "id")
                ),
            }
        } else if is_unresolved(change) {
            out!("not settled: the server still reads what it can't find from the old store");
        } else if change.get("oldStoreKept").and_then(Value::as_bool) == Some(true) {
            let id = text(change, "id");
            match cleanup(change) {
                None => out!(
                    "the old store still has its blobs; preview a cleanup with `inbuxa-cli store clean-up {id} --dry-run`"
                ),
                Some(run) if is_dry_run(run) && status(run) == "done" => {
                    out!("previewed; clean up with `inbuxa-cli store clean-up {id}`")
                }
                Some(run) if !is_dry_run(run) && status(run) == "incomplete" => out!(
                    "some blobs stay in the old store; put them right and run `inbuxa-cli store clean-up {id}` again"
                ),
                Some(_) => {}
            }
        }
        if let Some(run) = cleanup(change) {
            out!(
                "latest {}: {}, {} {} ({}), {} kept",
                if is_dry_run(run) {
                    "cleanup preview"
                } else {
                    "cleanup"
                },
                status(run),
                number(run, "deleted"),
                if is_dry_run(run) {
                    "would be deleted"
                } else {
                    "deleted"
                },
                size(count(run, "bytesFreed")),
                thousands(cleanup_kept(run))
            );
            if cleanup_kept(run) > 0 {
                problem_table(&problems(run), cleanup_kept(run))?;
            }
        }
        if let Some(ahead) = change.get("copiedAhead").and_then(Value::as_object) {
            out!(
                "copied ahead: {} copied, {} already there, {}, {}",
                number(ahead, "copied"),
                number(ahead, "alreadyThere"),
                size(ahead.get("bytesCopied").and_then(Value::as_u64).unwrap_or(0)),
                status(ahead)
            );
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
        if !counts.is_empty() && cleanup(change).is_none() {
            out!("{counts}");
            problem_table(&problems(change), change_problems(change))?;
        }
    }
    let (copy, after) = if is_planned(change) {
        ("the copy ahead", "it can't be applied yet")
    } else {
        ("the copy", "the change isn't settled")
    };
    match status(change) {
        "incomplete" => Err(Error::Found(format!(
            "{copy} couldn't copy {} blob(s); {after}",
            change_problems(change)
        ))),
        "stopped" | "failed" | "interrupted" => {
            Err(Error::Partial(format!("{copy} {}; {after}", status(change))))
        }
        _ => Ok(()),
    }
}

/// Looks again until the change's cleanup leaves `running`, as [`wait`] does.
fn wait_cleanup(client: &Client, id: &str) -> Result<Map<String, Value>> {
    let shown = std::io::stderr().is_terminal();
    if shown {
        eprintln!("{id} started; Ctrl-C stops waiting, not the cleanup");
    }
    loop {
        let change = get_one(client, BLOB_STORE_CHANGE, id)?;
        match cleanup(&change) {
            Some(run) if status(run) == "running" => {
                if shown {
                    eprint!(
                        "\r\x1b[K{} {} · {} {}",
                        if is_dry_run(run) {
                            "previewing"
                        } else {
                            "cleaning up"
                        },
                        percent(run),
                        number(run, "deleted"),
                        if is_dry_run(run) { "to delete" } else { "deleted" }
                    );
                }
                std::thread::sleep(POLL);
            }
            _ => {
                if shown {
                    eprint!("\r\x1b[K");
                }
                return Ok(change);
            }
        }
    }
}

/// OC-4 to OC-6: a cleanup or preview, what it kept, and what that means for
/// the exit status.
fn report_cleanup(change: &Map<String, Value>) -> Result<()> {
    let Some(run) = cleanup(change) else {
        return Err(Error::Server("the change has no cleanup".into()));
    };
    let preview = is_dry_run(run);
    out!(
        "{} of {}: {}{}",
        if preview { "cleanup preview" } else { "cleanup" },
        text(change, "from"),
        status(run),
        match text(run, "error") {
            e if e.is_empty() => String::new(),
            e => format!(" ({e})"),
        }
    );
    out!(
        "{} blobs looked at, {} {} ({}), {} not in the old store",
        number(run, "blobsChecked"),
        number(run, "deleted"),
        if preview { "would be deleted" } else { "deleted" },
        size(count(run, "bytesFreed")),
        number(run, "notThere")
    );
    if count(run, "snapshotsBefore") > 0 {
        out!(
            "{} snapshot(s) from before the change still hold the old blobs",
            number(run, "snapshotsBefore")
        );
    }
    let kept = cleanup_kept(run);
    let counts = counts(run, &["missing", "damaged", "different", "unreadable"]);
    if !counts.is_empty() {
        out!("kept in the old store: {counts}");
        problem_table(&problems(run), kept)?;
    }
    match status(run) {
        "done" if preview => {
            out!(
                "nothing deleted; clean up with `inbuxa-cli store clean-up {}`",
                text(change, "id")
            );
            Ok(())
        }
        "done" => {
            out!("none of the server's blobs are in the old store any more");
            Ok(())
        }
        "incomplete" => Err(Error::Found(format!(
            "{kept} blob(s) stay in the old store: the current store's copy didn't check out"
        ))),
        "running" => Ok(()),
        other => Err(Error::Partial(format!("the cleanup {other}"))),
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

/// The problems a copy or a cleanup kept, and a note when there were more.
fn problem_table(problems: &[Map<String, Value>], total: u64) -> Result<()> {
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
    kept_note(problems.len() as u64, total)
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

/// OC-6: a change's latest cleanup or preview.
fn cleanup(change: &Map<String, Value>) -> Option<&Map<String, Value>> {
    change.get("cleanup").and_then(Value::as_object)
}

fn is_dry_run(run: &Map<String, Value>) -> bool {
    run.get("dryRun").and_then(Value::as_bool).unwrap_or(false)
}

fn cleanup_kept(run: &Map<String, Value>) -> u64 {
    ["missing", "damaged", "different", "unreadable"]
        .iter()
        .map(|k| count(run, k))
        .sum()
}

/// BM-3: a change the server still reads through. A planned one never is.
fn is_unresolved(change: &Map<String, Value>) -> bool {
    !is_planned(change) && !matches!(status(change), "copied" | "reverted")
}

/// CA-1: planned, the setting not moved yet.
fn is_planned(change: &Map<String, Value>) -> bool {
    text(change, "stage") == "planned"
}

/// Changes newest first, the planned one, which hasn't happened yet, on top.
fn changes_first_planned(items: Vec<Map<String, Value>>) -> Vec<Map<String, Value>> {
    let (mut planned, rest): (Vec<_>, Vec<_>) = items.into_iter().partition(is_planned);
    planned.extend(newest_first(rest, "changedAt"));
    planned
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
    fn a_cleanup_that_kept_blobs_exits_1() {
        let with = |run: Value| object(json!({"id": "a", "status": "copied", "from": "old", "cleanup": run}));
        let done = with(json!({"status": "done", "deleted": 3, "bytesFreed": 900}));
        assert!(report_cleanup(&done).is_ok());
        let preview = with(json!({"status": "done", "dryRun": true, "deleted": 3}));
        assert!(report_cleanup(&preview).is_ok());
        let kept = with(json!({"status": "incomplete", "missing": 1, "different": 1, "problems": []}));
        assert_eq!(cleanup_kept(cleanup(&kept).unwrap()), 2);
        assert_eq!(report_cleanup(&kept).unwrap_err().exit_code(), 1);
        let stopped = with(json!({"status": "stopped"}));
        assert_eq!(report_cleanup(&stopped).unwrap_err().exit_code(), 5);
        let none = object(json!({"id": "a", "status": "copied"}));
        assert!(report_cleanup(&none).is_err());
    }

    #[test]
    fn a_planned_change_is_never_unsettled() {
        for s in ["pending", "copying", "incomplete", "copied"] {
            assert!(
                !is_unresolved(&object(json!({ "stage": "planned", "status": s }))),
                "{s}"
            );
        }
        assert!(is_unresolved(&object(
            json!({ "stage": "changed", "status": "pending" })
        )));
        // A server from before planning sends no stage
        assert!(is_unresolved(&object(json!({ "status": "pending" }))));
    }

    #[test]
    fn a_copy_ahead_that_left_blobs_behind_exits_1() {
        let incomplete = object(json!({"id": "a", "stage": "planned", "status": "incomplete", "missing": 1}));
        let err = report_change(&incomplete, true).unwrap_err();
        assert_eq!(err.exit_code(), 1);
        assert!(err.to_string().contains("can't be applied yet"), "{err}");
    }

    #[test]
    fn the_planned_change_lists_first() {
        let items = vec![
            object(json!({"id": "a", "changedAt": "2026-11-09T00:00:00Z"})),
            object(json!({"id": "c", "stage": "planned", "changedAt": null})),
            object(json!({"id": "b", "changedAt": "2026-11-10T00:00:00Z"})),
        ];
        let ids: Vec<String> = changes_first_planned(items)
            .iter()
            .map(|c| text(c, "id"))
            .collect();
        assert_eq!(ids, ["c", "b", "a"]);
    }

    #[test]
    fn a_target_needs_its_type() {
        assert!(parse_target(r#"{"@type": "FileSystem", "path": "/srv/blobs"}"#, "f").is_ok());
        assert_eq!(
            parse_target(r#"{"path": "/srv/blobs"}"#, "f")
                .unwrap_err()
                .exit_code(),
            2
        );
        assert_eq!(parse_target("not json", "f").unwrap_err().exit_code(), 2);
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
