//! The interactive SFTP console (issue #188).
//!
//! A terminal pane whose far side is not a shell but this: a REPL that
//! speaks SFTP, echoing its own input and printing its own output as VT
//! bytes. The pane is an ordinary terminal pane, which is what gives the
//! console scrollback, selection, copy, search, themes and session
//! recording for free.
//!
//! Behaviour follows `sftp(1)` from OpenSSH rather than any GUI client's
//! command set: the people who want this learned `get` and `lcd` there,
//! on whatever machine they were sitting at.
//!
//! The module is split so that everything decidable without a network is
//! decided without one:
//!
//! - [`editor`] turns keystrokes into echo bytes and line events;
//! - [`parser`] turns a submitted line into a [`parser::Command`];
//! - [`complete`] turns a Tab into an insertion, a candidate list or
//!   nothing;
//! - [`glob`] expands `*` / `?` / `[...]` over names already listed;
//! - [`render`] formats listings, sizes and the progress meter.
//!
//! Those five are pure and carry the bulk of the tests. The session that
//! drives them against a live [`crate::sftp::SftpClient`] is the only
//! part that needs a server.

pub mod complete;
pub mod editor;
pub mod exec;
pub mod glob;
pub mod parser;
pub mod render;
pub mod session;

pub use complete::{Candidate, Completion, Quote, Space, WordSpan};
pub use editor::{LineEditor, LineEvent};
pub use exec::{Outcome, ShellState};
pub use parser::{ArgSpace, Command, LsOpts, ParseError, Verb, XferOpts};
pub use session::SftpShellSession;

/// The prompt, matching `sftp(1)` so anything copied from a tutorial
/// looks like what the tutorial shows.
pub const PROMPT: &str = "sftp> ";
