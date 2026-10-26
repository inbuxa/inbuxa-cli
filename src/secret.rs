/*
 * SPDX-FileCopyrightText: 2026 Coffey Labs LLC
 *
 * SPDX-License-Identifier: AGPL-3.0-only
 */

//! Secrets come from the environment or a prompt, never the command line,
//! where they would end up in the shell's history and the process list.

use std::io::{BufRead, IsTerminal, Write};
use std::process::{Command, Stdio};

use crate::error::{Error, Result};

/// The value of `var`, or what the person types at a prompt that doesn't
/// echo. Without a terminal to prompt on, says which variable to set.
pub fn from_env_or_prompt(var: &str, label: &str) -> Result<String> {
    if let Ok(value) = std::env::var(var)
        && !value.is_empty()
    {
        return Ok(value);
    }
    if !std::io::stdin().is_terminal() {
        return Err(Error::Usage(format!(
            "no {label}: set {var}, or run this in a terminal to be asked for it"
        )));
    }
    prompt(label)
}

fn prompt(label: &str) -> Result<String> {
    let mut stderr = std::io::stderr();
    write!(stderr, "{label}: ")?;
    stderr.flush()?;

    let echo_was_off = set_echo(false);
    let mut line = String::new();
    let read = std::io::stdin().lock().read_line(&mut line);
    if echo_was_off {
        set_echo(true);
    }
    writeln!(stderr)?;
    read?;

    let value = line.trim_end_matches(['\r', '\n']);
    if value.is_empty() {
        return Err(Error::Usage(format!("no {label} given")));
    }
    Ok(value.to_owned())
}

/// Turns terminal echo on or off with stty, and says whether it worked.
fn set_echo(on: bool) -> bool {
    Command::new("stty")
        .arg(if on { "echo" } else { "-echo" })
        .stdin(Stdio::inherit())
        .status()
        .is_ok_and(|s| s.success())
}
