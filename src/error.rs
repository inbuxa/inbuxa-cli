/*
 * SPDX-FileCopyrightText: 2026 Coffey Labs LLC
 *
 * SPDX-License-Identifier: AGPL-3.0-only
 */

//! What can go wrong, and the exit status each kind ends the program with.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    /// The command line or a plan file asks for something that can't work.
    #[error("{0}")]
    Usage(String),

    /// The server couldn't be reached, or answered something that isn't HTTP
    /// we understand.
    #[error("{0}")]
    Transport(String),

    /// The server refused the request as a whole: bad credentials, a missing
    /// permission, an unknown capability.
    #[error("{0}")]
    Server(String),

    /// The request went through, but some of what it asked for didn't.
    #[error("{0}")]
    Partial(String),

    #[error("{0}")]
    Io(#[from] std::io::Error),

    #[error("{0}")]
    Json(#[from] serde_json::Error),
}

impl Error {
    pub fn exit_code(&self) -> i32 {
        match self {
            Error::Usage(_) => 2,
            Error::Transport(_) | Error::Io(_) => 3,
            Error::Server(_) | Error::Json(_) => 4,
            Error::Partial(_) => 5,
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;
