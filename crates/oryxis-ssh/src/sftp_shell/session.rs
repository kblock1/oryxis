//! The console as a transport: one task, one loop, the pane's surface.
//!
//! [`SftpShellSession`] exposes what every other terminal transport does
//! (write / resize / senders / is_alive / close), so a pane driven by it
//! is an ordinary pane and every generic path in the app works unchanged.
//! Behind that surface is a single task running the read-eval-print loop
//! over [`super::exec`].

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::mpsc;

use crate::engine::{SshError, SshSession};
use crate::sftp::SftpClient;

use super::PROMPT;
use super::complete::{self, Completion, Space};
use super::editor::{LineEditor, LineEvent};
use super::exec::{self, Outcome, ShellState};
use super::parser;
use super::render::{self, CRLF};

/// A live SFTP console.
///
/// Cheap to clone-by-Arc like the other sessions; the app holds it in
/// `TerminalTransport`.
#[derive(Debug)]
pub struct SftpShellSession {
    /// Keystrokes in. Also what the emulator's in-band query replies
    /// travel down, which is why the line editor's escape decoder has to
    /// swallow whole CSI sequences.
    input_tx: mpsc::UnboundedSender<Vec<u8>>,
    resize_tx: mpsc::UnboundedSender<(u16, u16)>,
    task: tokio::task::JoinHandle<()>,
    /// Latched by [`SftpShellSession::close`] so teardown runs exactly
    /// once even when an explicit close and the `Drop` backstop both
    /// fire.
    closed: AtomicBool,
    /// Set by the REPL task on its way out, BEFORE it drops the output
    /// sender. See [`SftpShellSession::is_alive`].
    repl_done: Arc<AtomicBool>,
    /// The SSH session whose CONNECTION the console's channel rides.
    ///
    /// Held to keep that connection alive, and for nothing else. The
    /// session's own shell channel is closed by the caller right after
    /// the console starts (nobody reads it, and its output would
    /// interleave a login banner into the console), so this reads as
    /// dead almost immediately and must NOT be consulted for health.
    /// What keeps the link up is the reference-counted transport inside
    /// it, which the SFTP channel shares.
    _ssh: Arc<SshSession>,
}

impl SftpShellSession {
    /// Start a console over `client`, which must already be open on
    /// `ssh`.
    ///
    /// Returns the session and the byte stream the pane renders. The
    /// caller maps that receiver into its own output message, exactly as
    /// the SSH and local paths do.
    pub fn spawn(
        ssh: Arc<SshSession>,
        client: SftpClient,
        remote_home: String,
        local_cwd: PathBuf,
        cols: u16,
        label: String,
    ) -> (Self, mpsc::UnboundedReceiver<Vec<u8>>) {
        let (input_tx, input_rx) = mpsc::unbounded_channel();
        let (resize_tx, resize_rx) = mpsc::unbounded_channel();
        let (output_tx, output_rx) = mpsc::unbounded_channel();
        let repl_done = Arc::new(AtomicBool::new(false));

        let task = tokio::spawn(
            Repl {
                client,
                state: ShellState::new(remote_home, local_cwd, cols),
                input_rx,
                resize_rx,
                output_tx,
                repl_done: Arc::clone(&repl_done),
                label,
            }
            .run(),
        );

        (
            Self {
                input_tx,
                resize_tx,
                task,
                closed: AtomicBool::new(false),
                repl_done,
                _ssh: ssh,
            },
            output_rx,
        )
    }

    pub fn write(&self, data: &[u8]) -> Result<(), SshError> {
        self.input_tx
            .send(data.to_vec())
            .map_err(|_| SshError::Channel("sftp console is closed".into()))
    }

    pub fn resize(&self, cols: u16, rows: u16) {
        let _ = self.resize_tx.send((cols, rows));
    }

    pub fn resize_sender(&self) -> mpsc::UnboundedSender<(u16, u16)> {
        self.resize_tx.clone()
    }

    /// The input sender, which the emulator uses for in-band query
    /// replies (cursor position, DECRQM). Those bytes land in the same
    /// place a keystroke does, and the line editor is what tells them
    /// apart.
    pub fn write_sender(&self) -> mpsc::UnboundedSender<Vec<u8>> {
        self.input_tx.clone()
    }

    /// Whether the console is still usable.
    ///
    /// Four signals, in the shape [`SshSession::is_alive`] establishes,
    /// and for the reason spelled out there at length: the app reads the
    /// end of the output stream as the pane's death notice and asks this
    /// before acting on it, so a session that really died while still
    /// answering "alive" would have its own notice discarded.
    ///
    /// `repl_done` is the one with the guaranteed ORDER. The REPL task
    /// sets it before dropping the output sender, in the same task with
    /// no await in between, which makes "dead before silent" true by
    /// construction rather than by scheduling luck.
    ///
    /// The link underneath is deliberately NOT one of the signals, even
    /// though the console cannot outlive it. The `SshSession` this holds
    /// has had its shell channel closed on purpose, so it answers "dead"
    /// while the CONNECTION it rides is perfectly fine, and asking it
    /// would report every console as dead a moment after it opened. A
    /// link that really goes away is noticed where it can be noticed at
    /// all: by the REPL, whose next command fails and whose health probe
    /// then confirms it and ends the loop, which sets `repl_done`.
    pub fn is_alive(&self) -> bool {
        !self.closed.load(Ordering::SeqCst)
            && !self.repl_done.load(Ordering::SeqCst)
            && !self.task.is_finished()
            && !self.input_tx.is_closed()
    }

    /// Tear the console down. Idempotent.
    pub fn close(&self) {
        if self.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        self.task.abort();
        // The SSH session is deliberately NOT closed here. The console
        // may be riding a link a terminal tab also holds (the reuse
        // pool hands out the same transport), and closing it would take
        // that tab's shell down with a console the user merely finished
        // with. Dropping our `Arc`, which happens when this session is
        // dropped, is how the console says it is done with the link.
    }
}

impl Drop for SftpShellSession {
    fn drop(&mut self) {
        self.close();
    }
}

/// The read-eval-print loop.
///
/// Two states, and the difference between them is what makes Ctrl+C work
/// during a four-gigabyte transfer:
///
/// - IDLE: input feeds the line editor, a submitted line becomes a
///   command.
/// - RUNNING: a command's future is in flight and input is NOT fed to
///   the editor. It is scanned for `0x03` and otherwise discarded.
///
/// Feeding the editor while a command runs would collect the keystrokes
/// into a phantom line that appeared, fully typed, the moment the prompt
/// came back. And running the command without also polling input would
/// leave Ctrl+C unread until the transfer finished, which is precisely
/// when nobody needs it any more.
struct Repl {
    client: SftpClient,
    state: ShellState,
    input_rx: mpsc::UnboundedReceiver<Vec<u8>>,
    resize_rx: mpsc::UnboundedReceiver<(u16, u16)>,
    output_tx: mpsc::UnboundedSender<Vec<u8>>,
    repl_done: Arc<AtomicBool>,
    label: String,
}

impl Repl {
    async fn run(self) {
        let Repl {
            client,
            mut state,
            mut input_rx,
            mut resize_rx,
            output_tx,
            repl_done,
            label,
        } = self;
        let mut editor = LineEditor::new(PROMPT, state.cols);
        let mut out = output_tx.clone();

        // The banner names the host the way `sftp(1)` does, so a screenshot
        // of the console says which machine it is.
        emit(
            &out,
            &format!("Connected to {label}.{CRLF}Type \"help\" for a list of commands.{CRLF}"),
        );
        emit_prompt(&out, &editor);

        loop {
            // ---- IDLE ---------------------------------------------------
            let line = tokio::select! {
                chunk = input_rx.recv() => {
                    let Some(chunk) = chunk else { break };
                    let (echo, events) = editor.feed(&chunk);
                    emit_bytes(&out, &echo);
                    let mut submitted = None;
                    let mut reprompt = false;
                    for event in events {
                        match event {
                            LineEvent::Submitted(line) => submitted = Some(line),
                            LineEvent::Eof => {
                                emit_bytes(&out, b"\r\n");
                                break;
                            }
                            // Ctrl+C at the prompt: the editor painted
                            // the `^C` and abandoned the line, and what
                            // replaces it is a NEW prompt, so it goes
                            // out marked like every other one.
                            LineEvent::Interrupted => reprompt = true,
                            LineEvent::CompleteRequested { line, cursor } => {
                                let bytes =
                                    complete(&line, cursor, &client, &state, &mut editor).await;
                                emit_bytes(&out, &bytes);
                            }
                        }
                    }
                    if reprompt {
                        emit_prompt(&out, &editor);
                    }
                    match submitted {
                        Some(line) => line,
                        None => continue,
                    }
                }
                Some((cols, rows)) = resize_rx.recv() => {
                    let _ = rows;
                    state.cols = cols.max(1);
                    editor.set_cols(state.cols);
                    // Repaint at the new geometry: the line the user is
                    // typing was drawn against the old width and would
                    // otherwise be wrong until they touched a key.
                    emit_bytes(&out, &editor.redraw());
                    continue;
                }
            };

            let cmd = match parser::parse(&line) {
                Ok(cmd) => cmd,
                Err(parser::ParseError::Empty) => {
                    emit_prompt(&out, &editor);
                    continue;
                }
                Err(e) => {
                    emit(&out, &format!("{e}{CRLF}"));
                    emit_prompt(&out, &editor);
                    continue;
                }
            };

            // ---- RUNNING ------------------------------------------------
            // A resize arriving mid-command is REMEMBERED, not applied: the
            // running future holds `&mut state`, so there is nothing to
            // assign to until it returns. That restriction happens to be the
            // right behaviour anyway. The command already captured its width
            // and repainting under it would tear a progress meter in half,
            // which is why `sftp(1)` also lets a resize land at the next
            // prompt.
            emit_bytes(&out, render::marks::OUTPUT_START.as_bytes());
            let mut pending_cols: Option<u16> = None;
            let outcome = race_command(
                exec::run(cmd, &client, &mut state, &mut out),
                &mut input_rx,
                &mut resize_rx,
                &mut pending_cols,
            )
            .await;
            if let Some(cols) = pending_cols {
                state.cols = cols;
                editor.set_cols(cols);
            }

            let failed = match outcome {
                Some(Outcome::Quit) => break,
                // Cancelled, or the input channel closed mid-command.
                // Worth a health check too: an interrupt and a link that
                // died under the transfer look identical from here.
                None => {
                    emit(&out, &format!("{CRLF}Interrupted.{CRLF}"));
                    true
                }
                Some(Outcome::Continue { failed }) => failed,
            };

            emit_bytes(&out, render::marks::command_end(failed).as_bytes());

            // Is the link still there? The error cannot say: `SftpClient`
            // maps a missing file and a dead channel to the same
            // `SshError::Channel`, so classifying would mean reading the
            // message, which breaks the first time a server words a
            // status differently. And the `SshSession` cannot say either,
            // because its shell channel was closed on purpose when the
            // console opened.
            //
            // So the console ASKS, with the cheapest question the
            // protocol has, and only when it has reason to: a command
            // that failed. On the happy path this costs nothing, and a
            // REPL that kept prompting over a dead channel would be a tab
            // reading "connected" that answers nothing.
            if failed && client.canonicalize(".").await.is_err() {
                emit(&out, &format!("Connection closed.{CRLF}"));
                break;
            }

            emit_prompt(&out, &editor);
        }

        // The ordering contract, and the only place it can be honoured:
        // mark dead, THEN drop the sender, in this task, with no await in
        // between. Anything that settles later (a JoinHandle, a channel some
        // other task closes) is a race rather than a guarantee.
        repl_done.store(true, Ordering::SeqCst);
        drop(out);
        drop(output_tx);
    }
}

/// Paint a fresh prompt, wrapped in the OSC 133 marks that say where the
/// command line begins.
///
/// Only the FRESH prompt carries them. A redraw during editing repaints
/// the same line and is not a new prompt; emitting the pair on every
/// keystroke would tell a reader that a command started and ended
/// between two characters.
fn emit_prompt(out: &mpsc::UnboundedSender<Vec<u8>>, editor: &LineEditor) {
    let mut bytes = render::marks::PROMPT_START.as_bytes().to_vec();
    bytes.extend_from_slice(&editor.redraw_fresh());
    bytes.extend_from_slice(render::marks::PROMPT_END.as_bytes());
    emit_bytes(out, &bytes);
}

/// Run `future` to completion while STILL READING input, so a Ctrl+C
/// arriving mid-command is seen while there is something to cancel.
///
/// Returns `None` when the command was interrupted (or the input channel
/// closed under it): dropping the future is the cancellation, and it
/// takes effect at the future's next await point.
///
/// Extracted from the loop so the property it exists for can be tested
/// against a future that never finishes. A version that awaited the
/// command and read input afterwards passes every test involving a real
/// transfer on a fast link, because the transfer wins the race before
/// the keystroke is even sent; it fails only for the user, on the slow
/// transfer they actually wanted to stop.
async fn race_command<F, T>(
    future: F,
    input_rx: &mut mpsc::UnboundedReceiver<Vec<u8>>,
    resize_rx: &mut mpsc::UnboundedReceiver<(u16, u16)>,
    pending_cols: &mut Option<u16>,
) -> Option<T>
where
    F: std::future::Future<Output = T>,
{
    tokio::pin!(future);
    loop {
        tokio::select! {
            // Biased so a command that has finished is seen as finished
            // rather than losing a race to a keystroke that arrived in
            // the same wakeup.
            biased;
            done = &mut future => break Some(done),
            chunk = input_rx.recv() => {
                let Some(chunk) = chunk else { break None };
                if chunk.contains(&0x03) {
                    // Dropping the future cancels the transfer at its
                    // next await. The partial file it leaves is the
                    // caller's to sweep; `SftpClient` has
                    // `discard_download_scratch` for exactly this, and
                    // the resume machinery is what makes keeping it
                    // worthwhile.
                    break None;
                }
                // Everything else typed during a command is discarded,
                // not buffered: keystrokes collected here would arrive
                // as a phantom line, already typed, the moment the
                // prompt came back.
            }
            Some((cols, _rows)) = resize_rx.recv() => {
                *pending_cols = Some(cols.max(1));
            }
        }
    }
}

/// Resolve a Tab, and paint the answer.
///
/// The IO half of completion: locate the word, list the ONE directory its
/// candidates can come from, and hand both to [`complete::plan`], which
/// is where every decision is made and tested. A listing that fails
/// paints nothing, and so does a word with nothing to say about it: a
/// bell or an error for a key people press speculatively is noise.
async fn complete(
    line: &str,
    cursor: usize,
    client: &SftpClient,
    state: &ShellState,
    editor: &mut LineEditor,
) -> Vec<u8> {
    let span = complete::word_at(line, cursor);
    let sep = match span.space() {
        Space::Local => complete::local_sep(&span.text),
        _ => '/',
    };
    let (dir, prefix) = complete::split_path(&span.text, sep);
    let (dir, prefix) = (dir.map(str::to_string), prefix.to_string());

    let mut candidates = match span.space() {
        Space::Nothing => return Vec::new(),
        // The verb takes no directory at all, which is why the vocabulary
        // is the one candidate source that cannot fail.
        Space::Verb => complete::verb_candidates(&span.text),
        Space::Remote => {
            // NO separator typed (complete against the working directory)
            // is not the same as a separator typed with nothing before it
            // (complete against the ROOT). Conflating them made `/var`
            // list the working directory and then replace the typed
            // `/var` with a relative `var`.
            let listing = match dir.as_deref() {
                None => state.remote_cwd.clone(),
                Some("") => "/".to_string(),
                Some(d) => state.resolve_remote(d),
            };
            let Ok(entries) = client.list_dir(&listing).await else {
                return Vec::new();
            };
            entries
                .iter()
                .map(|e| complete::Candidate {
                    name: e.name.clone(),
                    is_dir: e.is_dir,
                })
                .collect()
        }
        Space::Local => {
            let listing = match dir.as_deref() {
                None => state.local_cwd.clone(),
                Some("") => PathBuf::from(sep.to_string()),
                Some(d) => state.resolve_local(d),
            };
            let Ok(entries) = local_candidates(&listing).await else {
                return Vec::new();
            };
            entries
        }
    };
    // The server answers in its own order and a directory read answers in
    // the filesystem's; a candidate list is read by eye, so it is sorted.
    candidates.sort_by(|a, b| a.name.cmp(&b.name));

    match complete::plan(&prefix, dir.as_deref(), sep, span.quote, &candidates) {
        Completion::Nothing => Vec::new(),
        Completion::Insert(text) => editor.apply_completion(span.start, &text),
        // The line does not change, so it is REPAINTED rather than
        // reprompted: `redraw_fresh` because the list left the cursor on
        // a clean row with nothing above it to walk back over, and no
        // OSC 133 marks because this is the same prompt, not a new one.
        Completion::List(names) => {
            let names: Vec<&str> = names.iter().map(String::as_str).collect();
            let mut out = editor.break_below();
            out.extend_from_slice(render::columnize_names(&names, state.cols).as_bytes());
            out.extend_from_slice(&editor.redraw_fresh());
            out
        }
    }
}

/// List a local directory as completion candidates.
///
/// A symlink is followed to decide `is_dir`, so a link to a directory
/// invites the next component the way the directory itself would. A
/// broken one falls back to "not a directory", which is the honest answer
/// and costs nothing: the name still completes.
async fn local_candidates(dir: &std::path::Path) -> std::io::Result<Vec<complete::Candidate>> {
    let mut out = Vec::new();
    let mut read = tokio::fs::read_dir(dir).await?;
    while let Some(entry) = read.next_entry().await? {
        let is_dir = tokio::fs::metadata(entry.path())
            .await
            .map(|m| m.is_dir())
            .unwrap_or(false);
        out.push(complete::Candidate {
            name: entry.file_name().to_string_lossy().into_owned(),
            is_dir,
        });
    }
    Ok(out)
}

fn emit(out: &mpsc::UnboundedSender<Vec<u8>>, text: &str) {
    let _ = out.send(text.as_bytes().to_vec());
}

fn emit_bytes(out: &mpsc::UnboundedSender<Vec<u8>>, bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    let _ = out.send(bytes.to_vec());
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The property the console lives or dies by, tested against a
    /// command that never finishes so the result cannot depend on who
    /// wins a race. A loop that awaited the command and read input
    /// afterwards would hang here forever.
    #[tokio::test]
    async fn a_running_command_can_be_interrupted() {
        let (input_tx, mut input_rx) = mpsc::unbounded_channel();
        let (_resize_tx, mut resize_rx) = mpsc::unbounded_channel();
        let mut cols = None;

        input_tx.send(vec![0x03]).unwrap();
        let outcome: Option<()> = race_command(
            std::future::pending::<()>(),
            &mut input_rx,
            &mut resize_rx,
            &mut cols,
        )
        .await;
        assert_eq!(outcome, None, "a never-ending command was not cancelled");
    }

    /// Ctrl+C anywhere in a chunk counts, because a terminal is free to
    /// deliver it batched with whatever else was typed.
    #[tokio::test]
    async fn an_interrupt_is_seen_inside_a_larger_chunk() {
        let (input_tx, mut input_rx) = mpsc::unbounded_channel();
        let (_resize_tx, mut resize_rx) = mpsc::unbounded_channel();
        let mut cols = None;

        input_tx.send(b"abc\x03def".to_vec()).unwrap();
        let outcome: Option<()> = race_command(
            std::future::pending::<()>(),
            &mut input_rx,
            &mut resize_rx,
            &mut cols,
        )
        .await;
        assert_eq!(outcome, None);
    }

    /// Ordinary typing during a command does NOT cancel it, and does not
    /// accumulate either: it is dropped, so nothing appears as a phantom
    /// line when the prompt returns.
    #[tokio::test]
    async fn typing_during_a_command_neither_cancels_nor_buffers() {
        let (input_tx, mut input_rx) = mpsc::unbounded_channel();
        let (_resize_tx, mut resize_rx) = mpsc::unbounded_channel();
        let mut cols = None;

        input_tx.send(b"ls -l\r".to_vec()).unwrap();
        input_tx.send(b"pwd\r".to_vec()).unwrap();
        let outcome = race_command(
            async {
                // Long enough that the two chunks above are consumed by
                // the racer before it resolves.
                tokio::time::sleep(std::time::Duration::from_millis(60)).await;
                42
            },
            &mut input_rx,
            &mut resize_rx,
            &mut cols,
        )
        .await;
        assert_eq!(outcome, Some(42), "typing cancelled a running command");
        // Nothing was left queued for the next prompt to swallow.
        assert!(input_rx.try_recv().is_err());
    }

    /// A command that finishes wins over a keystroke delivered in the
    /// same wakeup: `biased` puts the future first, so a `bye` typed
    /// right as a transfer ends is not read as an interrupt of it.
    #[tokio::test]
    async fn a_finished_command_beats_a_simultaneous_keystroke() {
        let (input_tx, mut input_rx) = mpsc::unbounded_channel();
        let (_resize_tx, mut resize_rx) = mpsc::unbounded_channel();
        let mut cols = None;

        input_tx.send(vec![0x03]).unwrap();
        let outcome = race_command(
            std::future::ready(7),
            &mut input_rx,
            &mut resize_rx,
            &mut cols,
        )
        .await;
        assert_eq!(outcome, Some(7));
    }

    /// A resize mid-command is remembered rather than applied, and does
    /// not end the command.
    #[tokio::test]
    async fn a_resize_during_a_command_is_remembered() {
        let (_input_tx, mut input_rx) = mpsc::unbounded_channel();
        let (resize_tx, mut resize_rx) = mpsc::unbounded_channel();
        let mut cols = None;

        resize_tx.send((120, 40)).unwrap();
        let outcome = race_command(
            async {
                tokio::time::sleep(std::time::Duration::from_millis(60)).await;
                1
            },
            &mut input_rx,
            &mut resize_rx,
            &mut cols,
        )
        .await;
        assert_eq!(outcome, Some(1));
        assert_eq!(cols, Some(120));
    }

    /// A zero width would divide by zero in the editor's geometry, so it
    /// is clamped on the way in rather than at every use.
    #[tokio::test]
    async fn a_zero_width_resize_is_clamped() {
        let (_input_tx, mut input_rx) = mpsc::unbounded_channel();
        let (resize_tx, mut resize_rx) = mpsc::unbounded_channel();
        let mut cols = None;

        resize_tx.send((0, 40)).unwrap();
        let _: Option<i32> = race_command(
            async {
                tokio::time::sleep(std::time::Duration::from_millis(60)).await;
                1
            },
            &mut input_rx,
            &mut resize_rx,
            &mut cols,
        )
        .await;
        assert_eq!(cols, Some(1));
    }

    /// The input channel closing under a command (the pane went away)
    /// ends it, rather than leaving the loop waiting on a sender that
    /// will never send again.
    #[tokio::test]
    async fn a_closed_input_channel_ends_a_running_command() {
        let (input_tx, mut input_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let (_resize_tx, mut resize_rx) = mpsc::unbounded_channel();
        let mut cols = None;

        drop(input_tx);
        let outcome: Option<()> = race_command(
            std::future::pending::<()>(),
            &mut input_rx,
            &mut resize_rx,
            &mut cols,
        )
        .await;
        assert_eq!(outcome, None);
    }

    /// A local Tab lists the LOCAL directory, which is the half that was
    /// missing: every word went to `list_dir` on the server, so a `put`
    /// looking for a file on this machine found nothing and painted
    /// nothing. No server is needed to prove it, only a directory.
    #[tokio::test]
    async fn a_local_word_takes_its_candidates_from_the_filesystem() {
        let dir = std::env::temp_dir().join(format!("oryxis-complete-{}", std::process::id()));
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(dir.join("sub")).await.unwrap();
        tokio::fs::write(dir.join("alpha.txt"), b"x").await.unwrap();

        let mut found = local_candidates(&dir).await.unwrap();
        found.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(
            found,
            vec![
                complete::Candidate::file("alpha.txt"),
                complete::Candidate::dir("sub"),
            ]
        );
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    /// A directory that cannot be read is not an error to report: the
    /// user pressed a key speculatively and the answer is that there is
    /// nothing to say.
    #[tokio::test]
    async fn an_unreadable_local_directory_yields_no_candidates() {
        let missing = std::env::temp_dir().join("oryxis-complete-does-not-exist");
        assert!(local_candidates(&missing).await.is_err());
    }
}
