/*
 * SPDX-FileCopyrightText: 2026 Coffey Labs LLC
 *
 * SPDX-License-Identifier: AGPL-3.0-only
 */

//! How much a command says about what it's doing, apart from its result. The
//! result itself goes to standard output; these are the asides on standard
//! error -- what was created, what a reload did, how many rows -- which
//! `--quiet` turns off and `--verbose` adds to. An error is none of these; it
//! is always shown.

use std::sync::atomic::{AtomicU8, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    /// Only the result and errors.
    Quiet = 0,
    /// The result, errors, and a line for each thing done.
    Normal = 1,
    /// The above, and extra detail about what the command is doing.
    Verbose = 2,
}

static LEVEL: AtomicU8 = AtomicU8::new(Level::Normal as u8);

pub fn set(level: Level) {
    LEVEL.store(level as u8, Ordering::Relaxed);
}

fn at_least(level: Level) -> bool {
    LEVEL.load(Ordering::Relaxed) >= level as u8
}

/// A status aside: shown unless `--quiet`.
pub fn emit_note(args: std::fmt::Arguments) {
    if at_least(Level::Normal) {
        eprintln!("{args}");
    }
}

/// An extra diagnostic: shown only with `--verbose`.
pub fn emit_detail(args: std::fmt::Arguments) {
    if at_least(Level::Verbose) {
        eprintln!("{args}");
    }
}

/// `eprintln!`-shaped status aside, silenced by `--quiet`.
macro_rules! note {
    ($($arg:tt)*) => { $crate::report::emit_note(format_args!($($arg)*)) };
}
/// `eprintln!`-shaped diagnostic, shown only with `--verbose`.
macro_rules! detail {
    ($($arg:tt)*) => { $crate::report::emit_detail(format_args!($($arg)*)) };
}
pub(crate) use {detail, note};
