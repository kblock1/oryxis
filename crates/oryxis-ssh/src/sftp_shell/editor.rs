//! The console's line editor: bytes in, echo bytes and line events out.
//!
//! A remote shell echoes what you type because a PTY on the far side does
//! it. The SFTP console has no far side to ask, so this is where the
//! echo comes from: every visible effect of a keystroke (the character
//! appearing, the cursor moving, the line being redrawn after an insert
//! in the middle) is a byte sequence this module emits.
//!
//! It is deliberately PURE. No session, no clock, no allocation the
//! caller can't see: [`LineEditor::feed`] takes the bytes that arrived
//! and answers with what to paint plus what happened. That is what makes
//! the whole editing surface testable without a network, which matters
//! because the cases that break line editors are the ones nobody
//! reproduces by hand (an escape sequence split across two packets, a
//! CJK name that advances two columns per character, a line longer than
//! the window).

use unicode_width::UnicodeWidthStr;

/// How many entries the up-arrow can walk back through. The console is a
/// session, not a shell: nobody is scrolling to what they typed an hour
/// ago, and an unbounded history on a long-lived tab is just a leak.
const HISTORY_LIMIT: usize = 500;

/// What the caller has to act on after feeding bytes in. Echo is returned
/// separately (it always happens); these are the things that need a
/// decision outside the editor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LineEvent {
    /// Enter was pressed. Carries the line WITHOUT the newline. May be
    /// empty (a bare Enter), which the caller answers with a fresh
    /// prompt rather than a parse.
    Submitted(String),
    /// Ctrl+C. The line was already discarded and the echo already
    /// carries the `^C` and the new prompt, so the caller only has to
    /// cancel whatever it was doing.
    Interrupted,
    /// Ctrl+D on an EMPTY line, which is how `sftp(1)` quits. On a
    /// non-empty line Ctrl+D deletes forward instead and no event fires.
    Eof,
    /// Tab was pressed. The caller locates the word with
    /// [`super::complete::word_at`], resolves candidates for it and feeds
    /// the answer back through [`LineEditor::apply_completion`].
    ///
    /// Carries the WHOLE line plus the cursor rather than the word,
    /// because the word alone cannot say which namespace it lives in: the
    /// verb decides that, and `put` completes against a directory `get`
    /// never touches.
    CompleteRequested { line: String, cursor: usize },
}

/// Where the escape-sequence decoder stands BETWEEN calls to `feed`.
///
/// This state living in the struct rather than in a local is the whole
/// point: a terminal delivers `\x1b[A` as one packet most of the time and
/// as `\x1b` + `[A` whenever the write happens to split there. An editor
/// that decodes per call sees a lone ESC, discards it, and then prints a
/// literal `[A` into the buffer. The bug shows up as "arrow keys
/// sometimes type garbage", which is unreproducible by hand and obvious
/// in a test.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EscState {
    /// Not inside a sequence.
    Ground,
    /// Saw `\x1b`, waiting for `[` or `O`.
    Escape,
    /// Saw `\x1b[` or `\x1bO`, accumulating parameter bytes.
    Csi,
}

/// A line editor for one console. Owns the buffer, the cursor, the
/// history and the terminal width it draws against.
#[derive(Debug)]
pub struct LineEditor {
    /// The line being edited.
    buf: String,
    /// Cursor position as a BYTE index into `buf`, always on a character
    /// boundary. Byte index rather than char index because every edit
    /// operation here is a `String` splice, and converting back and
    /// forth is where off-by-one bugs live.
    cursor: usize,
    /// What the prompt looks like, so the editor can redraw the whole
    /// line (it has to know how wide the prompt is to place the cursor).
    prompt: String,
    history: Vec<String>,
    /// Where the up/down walk stands. `None` = editing a fresh line.
    /// `Some(i)` = showing `history[i]`.
    hist_pos: Option<usize>,
    /// The line that was being typed when the history walk started, so
    /// walking back down past the newest entry restores it instead of
    /// leaving an empty line.
    hist_stash: Option<String>,
    /// Terminal width, needed to know how many visual lines the logical
    /// line occupies. A redraw that ignores this corrupts the display
    /// the moment a line wraps.
    cols: u16,
    esc: EscState,
    /// Parameter bytes accumulated since the CSI introducer.
    esc_params: String,
    /// Bytes of a multi-byte UTF-8 character seen so far. Same reason
    /// `esc` is a field: a character split across two packets must not
    /// become two broken ones.
    pending: Vec<u8>,
}

impl LineEditor {
    pub fn new(prompt: impl Into<String>, cols: u16) -> Self {
        Self {
            buf: String::new(),
            cursor: 0,
            prompt: prompt.into(),
            history: Vec::new(),
            hist_pos: None,
            hist_stash: None,
            cols: cols.max(1),
            esc: EscState::Ground,
            esc_params: String::new(),
            pending: Vec::new(),
        }
    }

    pub fn set_cols(&mut self, cols: u16) {
        self.cols = cols.max(1);
    }

    /// The current line, for a caller that needs to look without taking.
    pub fn buffer(&self) -> &str {
        &self.buf
    }

    /// Feed raw input bytes. Returns `(echo, events)`.
    ///
    /// Input arrives as UTF-8 from the terminal widget's key encoder. A
    /// byte that is not valid UTF-8 is dropped rather than rendered as a
    /// replacement character: the alternative is a buffer that no longer
    /// matches what the user sees, and a filename built from it would be
    /// wrong in a way the user cannot correct.
    pub fn feed(&mut self, input: &[u8]) -> (Vec<u8>, Vec<LineEvent>) {
        let mut out = Vec::new();
        let mut events = Vec::new();
        // Decode incrementally: `from_utf8_lossy` over the whole slice
        // would corrupt a multi-byte character split across two packets,
        // which is the same class of bug as the split escape sequence.
        for &byte in input {
            self.feed_byte(byte, &mut out, &mut events);
        }
        (out, events)
    }

    fn feed_byte(&mut self, byte: u8, out: &mut Vec<u8>, events: &mut Vec<LineEvent>) {
        match self.esc {
            EscState::Escape => {
                self.esc_params.clear();
                match byte {
                    b'[' | b'O' => self.esc = EscState::Csi,
                    // ESC followed by anything else is not a sequence we
                    // act on. Swallow both bytes rather than printing
                    // them: a stray ESC in the buffer would be invisible
                    // and would travel into a filename.
                    _ => self.esc = EscState::Ground,
                }
                return;
            }
            EscState::Csi => {
                // ECMA-48 shapes a CSI as parameter bytes (0x30-0x3F),
                // then intermediate bytes (0x20-0x2F), then exactly one
                // final byte (0x40-0x7E). Both non-final ranges have to
                // be consumed here, not just the parameters: the
                // emulator answers a DECRQM query with something like
                // `\x1b[?2026;2$y`, and a decoder that stops at the `$`
                // treats it as the final byte and then prints the `y`
                // into the line the user is typing. Those replies reach
                // this editor because the transport's write sender IS
                // the console's input channel.
                match byte {
                    // 0x30-0x3F are the parameter bytes and 0x20-0x2F
                    // the intermediates; contiguous, so one range covers
                    // both, and everything outside it is the final byte.
                    0x20..=0x3f => self.esc_params.push(byte as char),
                    _ => {
                        self.esc = EscState::Ground;
                        let params = std::mem::take(&mut self.esc_params);
                        self.apply_csi(byte, &params, out);
                    }
                }
                return;
            }
            EscState::Ground => {}
        }

        match byte {
            0x1b => self.esc = EscState::Escape,
            // Enter. Both CR and LF submit: the key encoder sends CR, but
            // a paste carries LF and must not be treated as a stray
            // control byte.
            b'\r' | b'\n' => {
                out.extend_from_slice(b"\r\n");
                let line = std::mem::take(&mut self.buf);
                self.cursor = 0;
                self.hist_pos = None;
                self.hist_stash = None;
                self.remember(&line);
                events.push(LineEvent::Submitted(line));
            }
            // Ctrl+C: abandon the line. `sftp(1)` prints nothing special,
            // but a bare reprompt reads as if the key did nothing, so the
            // `^C` is echoed the way every shell does it.
            0x03 => {
                // The `^C` is echoed the way every shell does it, because
                // a bare reprompt reads as if the key did nothing. The
                // PROMPT that follows is left to the caller: it is a new
                // prompt, and only the caller can wrap one in the OSC 133
                // marks that say so.
                out.extend_from_slice(b"^C\r\n");
                self.buf.clear();
                self.cursor = 0;
                self.hist_pos = None;
                self.hist_stash = None;
                events.push(LineEvent::Interrupted);
            }
            // Ctrl+D: EOF on an empty line, forward-delete otherwise.
            // That asymmetry is the shell convention and it matters: a
            // user deleting the last character of a line does not expect
            // the next Ctrl+D to close the session.
            0x04 => {
                if self.buf.is_empty() {
                    events.push(LineEvent::Eof);
                } else {
                    self.delete_forward(out);
                }
            }
            // Backspace. Terminals disagree about which byte this is
            // (0x7f on unix, 0x08 where the key encoder maps ^H), so both
            // are accepted rather than picking a side.
            0x08 | 0x7f => self.delete_back(out),
            b'\t' => events.push(LineEvent::CompleteRequested {
                line: self.buf.clone(),
                cursor: self.cursor,
            }),
            // Ctrl+A / Ctrl+E: start and end of line.
            0x01 => {
                self.cursor = 0;
                out.extend_from_slice(&self.redraw());
            }
            0x05 => {
                self.cursor = self.buf.len();
                out.extend_from_slice(&self.redraw());
            }
            // Ctrl+B / Ctrl+F: the control-key twins of Left and Right,
            // for the emacs hands and for terminals that send neither.
            0x02 => self.move_left(out),
            0x06 => self.move_right(out),
            // Ctrl+U: kill to start of line.
            0x15 => {
                self.buf.drain(..self.cursor);
                self.cursor = 0;
                out.extend_from_slice(&self.redraw());
            }
            // Ctrl+K: kill to end of line.
            0x0b => {
                self.buf.truncate(self.cursor);
                out.extend_from_slice(&self.redraw());
            }
            // Ctrl+W: kill the word before the cursor.
            0x17 => {
                let start = kill_word_start(&self.buf, self.cursor);
                self.buf.drain(start..self.cursor);
                self.cursor = start;
                out.extend_from_slice(&self.redraw());
            }
            // Ctrl+L: clear the screen and redraw the prompt in place,
            // like every readline. The line survives, which is the point.
            0x0c => {
                out.extend_from_slice(b"\x1b[H\x1b[2J");
                out.extend_from_slice(&self.redraw_fresh());
            }
            // Ctrl+P / Ctrl+N: history, for the same hands as Ctrl+B/F.
            0x10 => self.history_prev(out),
            0x0e => self.history_next(out),
            // Any other C0 control byte is not an editing key here.
            // Dropping it keeps the buffer equal to what is on screen.
            0x00..=0x1f => {}
            _ => self.insert_byte(byte, out),
        }
    }

    /// Apply a decoded CSI sequence. `final_byte` is the terminating
    /// character, `params` the digits before it.
    fn apply_csi(&mut self, final_byte: u8, params: &str, out: &mut Vec<u8>) {
        match final_byte {
            b'A' => self.history_prev(out),
            b'B' => self.history_next(out),
            b'C' => self.move_right(out),
            b'D' => self.move_left(out),
            // Home / End arrive as either `\x1b[H` / `\x1b[F` or as
            // `\x1b[1~` / `\x1b[4~`, depending on the terminal's mode.
            b'H' => {
                self.cursor = 0;
                out.extend_from_slice(&self.redraw());
            }
            b'F' => {
                self.cursor = self.buf.len();
                out.extend_from_slice(&self.redraw());
            }
            b'~' => match params {
                "1" | "7" => {
                    self.cursor = 0;
                    out.extend_from_slice(&self.redraw());
                }
                "4" | "8" => {
                    self.cursor = self.buf.len();
                    out.extend_from_slice(&self.redraw());
                }
                "3" => self.delete_forward(out),
                _ => {}
            },
            _ => {}
        }
    }

    /// Buffer one UTF-8 byte, emitting only once a full character is in
    /// hand. `String::push_str` on a partial sequence would panic, and
    /// waiting is exactly what a split multi-byte character needs.
    fn insert_byte(&mut self, byte: u8, out: &mut Vec<u8>) {
        // Fast path: ASCII is one byte and is what almost every keystroke
        // is, so it never touches the pending buffer.
        if byte.is_ascii() {
            self.insert_str(&(byte as char).to_string(), out);
            return;
        }
        self.pending.push(byte);
        match std::str::from_utf8(&self.pending) {
            Ok(s) => {
                let s = s.to_string();
                self.pending.clear();
                self.insert_str(&s, out);
            }
            Err(e) if e.error_len().is_none() => {
                // Incomplete but still valid so far: wait for the rest.
                // A sequence that never completes is capped below so a
                // malformed stream can't grow the buffer forever.
                if self.pending.len() >= 4 {
                    self.pending.clear();
                }
            }
            Err(_) => {
                // Actually invalid, not merely incomplete. Drop it: see
                // the note on `feed` about why nothing is substituted.
                self.pending.clear();
            }
        }
    }

    fn insert_str(&mut self, s: &str, out: &mut Vec<u8>) {
        self.buf.insert_str(self.cursor, s);
        self.cursor += s.len();
        // Appending at the end is the common case and needs no redraw:
        // the character can simply be printed. Anything else moves text
        // to the right of the cursor and has to be repainted.
        if self.cursor == self.buf.len() {
            out.extend_from_slice(s.as_bytes());
            // A character that lands exactly on the last column leaves
            // the cursor in the terminal's "pending wrap" limbo, where
            // the next print jumps a line. Force the wrap now so the
            // editor's idea of the cursor matches the screen's.
            if self.display_col().is_multiple_of(self.cols as usize) {
                out.extend_from_slice(b"\r\n");
            }
        } else {
            out.extend_from_slice(&self.redraw());
        }
    }

    fn delete_back(&mut self, out: &mut Vec<u8>) {
        if self.cursor == 0 {
            return;
        }
        let prev = prev_boundary(&self.buf, self.cursor);
        self.buf.drain(prev..self.cursor);
        self.cursor = prev;
        out.extend_from_slice(&self.redraw());
    }

    fn delete_forward(&mut self, out: &mut Vec<u8>) {
        if self.cursor >= self.buf.len() {
            return;
        }
        let next = next_boundary(&self.buf, self.cursor);
        self.buf.drain(self.cursor..next);
        out.extend_from_slice(&self.redraw());
    }

    fn move_left(&mut self, out: &mut Vec<u8>) {
        if self.cursor == 0 {
            return;
        }
        self.cursor = prev_boundary(&self.buf, self.cursor);
        out.extend_from_slice(&self.redraw());
    }

    fn move_right(&mut self, out: &mut Vec<u8>) {
        if self.cursor >= self.buf.len() {
            return;
        }
        self.cursor = next_boundary(&self.buf, self.cursor);
        out.extend_from_slice(&self.redraw());
    }

    fn history_prev(&mut self, out: &mut Vec<u8>) {
        if self.history.is_empty() {
            return;
        }
        let next = match self.hist_pos {
            None => {
                // Entering the walk: stash whatever was being typed so
                // coming back down restores it.
                self.hist_stash = Some(self.buf.clone());
                self.history.len() - 1
            }
            Some(0) => return,
            Some(i) => i - 1,
        };
        self.hist_pos = Some(next);
        self.buf = self.history[next].clone();
        self.cursor = self.buf.len();
        out.extend_from_slice(&self.redraw());
    }

    fn history_next(&mut self, out: &mut Vec<u8>) {
        let Some(i) = self.hist_pos else {
            return;
        };
        if i + 1 < self.history.len() {
            self.hist_pos = Some(i + 1);
            self.buf = self.history[i + 1].clone();
        } else {
            // Walked past the newest entry: back to the stashed line.
            self.hist_pos = None;
            self.buf = self.hist_stash.take().unwrap_or_default();
        }
        self.cursor = self.buf.len();
        out.extend_from_slice(&self.redraw());
    }

    fn remember(&mut self, line: &str) {
        if line.trim().is_empty() {
            return;
        }
        // A command repeated back to back is one entry, like every shell.
        if self.history.last().map(String::as_str) == Some(line) {
            return;
        }
        self.history.push(line.to_string());
        if self.history.len() > HISTORY_LIMIT {
            self.history.remove(0);
        }
    }

    /// Install the caller's completion answer.
    ///
    /// `text` replaces the buffer from `start` to the cursor. The editor
    /// does NOT locate that span itself: quoting decides where a word
    /// begins (`get "my fi` is one word, not two), and the module that
    /// knows the quoting rules is [`super::complete`], the same one that
    /// built `text` under them. Splitting the two apart is how the
    /// comment here once described a quote-aware word the code did not
    /// have.
    ///
    /// The trailing marker (a separator for a directory, a space
    /// otherwise, plus any closing quote) is part of `text` for the same
    /// reason: only the caller knows what the candidate was.
    pub fn apply_completion(&mut self, start: usize, text: &str) -> Vec<u8> {
        let start = start.min(self.cursor);
        self.buf.replace_range(start..self.cursor, text);
        self.cursor = start + text.len();
        self.redraw()
    }

    /// Bytes that leave the cursor on a fresh row BELOW the whole line.
    ///
    /// Where output printed mid-edit has to start. A bare `\r\n` is not
    /// enough: the line being edited may occupy several rows and the
    /// cursor may be on any of them, so a newline from where it stands
    /// would paint the output over the rest of the line.
    pub fn break_below(&self) -> Vec<u8> {
        let cols = self.cols as usize;
        let end_row = (self.prompt_width() + self.buf.width()) / cols;
        let cur_row = self.display_col() / cols;
        let mut out = Vec::new();
        if end_row > cur_row {
            out.extend_from_slice(format!("\x1b[{}B", end_row - cur_row).as_bytes());
        }
        out.extend_from_slice(b"\r\n");
        out
    }

    /// Repaint the prompt and the line, leaving the cursor where the
    /// editor thinks it is.
    ///
    /// This walks back over EVERY visual line the logical line occupies,
    /// not just the current one. A redraw that assumes one row is correct
    /// until the first line that wraps, at which point it starts painting
    /// over the previous row and the display never recovers. The cost of
    /// getting it right is knowing the column width of the text, which is
    /// why the editor tracks `cols` and measures with `unicode-width`.
    pub fn redraw(&self) -> Vec<u8> {
        let cols = self.cols as usize;
        let mut out = Vec::new();
        // Where the cursor currently is, in visual rows below the row the
        // prompt starts on.
        let cur_row = self.display_col() / cols;
        if cur_row > 0 {
            out.extend_from_slice(format!("\x1b[{cur_row}A").as_bytes());
        }
        out.extend_from_slice(b"\r\x1b[J");
        out.extend_from_slice(self.prompt.as_bytes());
        out.extend_from_slice(self.buf.as_bytes());
        // Place the cursor: total width tells us where printing left it,
        // the cursor's own width tells us where it belongs.
        let end_col = self.prompt_width() + self.buf.width();
        let want_col = self.display_col();
        let end_row = end_col / cols;
        let want_row = want_col / cols;
        if end_row > want_row {
            out.extend_from_slice(format!("\x1b[{}A", end_row - want_row).as_bytes());
        }
        out.extend_from_slice(format!("\r\x1b[{}C", want_col % cols).as_bytes());
        out
    }

    /// Redraw with no attempt to walk back over previous rows, for the
    /// two moments when nothing is on screen to walk back over: the first
    /// prompt of the session, and right after Ctrl+L cleared the display.
    pub fn redraw_fresh(&self) -> Vec<u8> {
        let cols = self.cols as usize;
        let mut out = Vec::new();
        out.extend_from_slice(self.prompt.as_bytes());
        out.extend_from_slice(self.buf.as_bytes());
        let end_col = self.prompt_width() + self.buf.width();
        let want_col = self.display_col();
        let end_row = end_col / cols;
        let want_row = want_col / cols;
        if end_row > want_row {
            out.extend_from_slice(format!("\x1b[{}A", end_row - want_row).as_bytes());
        }
        out.extend_from_slice(format!("\r\x1b[{}C", want_col % cols).as_bytes());
        out
    }

    /// Set a new prompt (the remote cwd is part of it in some styles) and
    /// return the bytes that paint it.
    pub fn set_prompt(&mut self, prompt: impl Into<String>) {
        self.prompt = prompt.into();
    }

    /// Total display columns from the start of the prompt to the cursor.
    fn display_col(&self) -> usize {
        self.prompt_width() + self.buf[..self.cursor].width()
    }

    fn prompt_width(&self) -> usize {
        self.prompt.width()
    }
}

/// Byte index of the character before `idx`.
fn prev_boundary(s: &str, idx: usize) -> usize {
    s[..idx]
        .char_indices()
        .next_back()
        .map(|(i, _)| i)
        .unwrap_or(0)
}

/// Byte index just past the character at `idx`.
fn next_boundary(s: &str, idx: usize) -> usize {
    s[idx..]
        .chars()
        .next()
        .map(|c| idx + c.len_utf8())
        .unwrap_or(idx)
}

/// Start of the word Ctrl+W deletes: skip the trailing spaces first, then
/// run back to the space before the word. That is what makes a Ctrl+W on
/// `get file   ` reach `file` rather than deleting the spaces alone.
fn kill_word_start(s: &str, idx: usize) -> usize {
    let head = &s[..idx];
    let trimmed = head.trim_end_matches(' ');
    match trimmed.rfind(' ') {
        Some(i) => i + 1,
        None => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feed a string and return only the events, for the cases where the
    /// echo is not what is under test.
    fn events(ed: &mut LineEditor, s: &str) -> Vec<LineEvent> {
        ed.feed(s.as_bytes()).1
    }

    fn submit(ed: &mut LineEditor, s: &str) -> String {
        let evs = events(ed, s);
        match evs.into_iter().next() {
            Some(LineEvent::Submitted(line)) => line,
            other => panic!("expected a submit, got {other:?}"),
        }
    }

    #[test]
    fn typing_and_enter_yield_the_line() {
        let mut ed = LineEditor::new("sftp> ", 80);
        assert_eq!(submit(&mut ed, "ls -l\r"), "ls -l");
        assert_eq!(ed.buffer(), "");
    }

    #[test]
    fn plain_characters_echo_themselves() {
        let mut ed = LineEditor::new("sftp> ", 80);
        let (echo, _) = ed.feed(b"cd");
        assert_eq!(echo, b"cd");
    }

    /// The regression this whole module's `esc` field exists for. A
    /// terminal is free to split `\x1b[A` anywhere; an editor that
    /// decodes per call prints a literal `[A` into the buffer.
    #[test]
    fn escape_sequence_split_across_packets_is_still_one_arrow() {
        let mut ed = LineEditor::new("sftp> ", 80);
        submit(&mut ed, "get file.txt\r");
        // The escape arrives alone, then the rest.
        ed.feed(b"\x1b");
        ed.feed(b"[A");
        assert_eq!(ed.buffer(), "get file.txt");
    }

    #[test]
    fn a_whole_arrow_in_one_packet_works_too() {
        let mut ed = LineEditor::new("sftp> ", 80);
        submit(&mut ed, "put a\r");
        ed.feed(b"\x1b[A");
        assert_eq!(ed.buffer(), "put a");
    }

    /// Same class as the split escape, one layer down: a multi-byte
    /// character delivered in two writes must not become two broken ones.
    #[test]
    fn utf8_character_split_across_packets_survives() {
        let mut ed = LineEditor::new("sftp> ", 80);
        let bytes = "é".as_bytes();
        assert_eq!(bytes.len(), 2);
        ed.feed(&bytes[..1]);
        assert_eq!(ed.buffer(), "");
        ed.feed(&bytes[1..]);
        assert_eq!(ed.buffer(), "é");
    }

    #[test]
    fn backspace_removes_a_whole_character_not_a_byte() {
        let mut ed = LineEditor::new("sftp> ", 80);
        ed.feed("café".as_bytes());
        ed.feed(b"\x7f");
        assert_eq!(ed.buffer(), "caf");
    }

    #[test]
    fn both_backspace_encodings_are_accepted() {
        let mut ed = LineEditor::new("sftp> ", 80);
        ed.feed(b"abc\x08");
        assert_eq!(ed.buffer(), "ab");
        ed.feed(b"\x7f");
        assert_eq!(ed.buffer(), "a");
    }

    #[test]
    fn arrows_move_and_insert_lands_at_the_cursor() {
        let mut ed = LineEditor::new("sftp> ", 80);
        ed.feed(b"ls");
        ed.feed(b"\x1b[D\x1b[D");
        ed.feed(b"l");
        assert_eq!(ed.buffer(), "lls");
    }

    #[test]
    fn delete_key_removes_forward() {
        let mut ed = LineEditor::new("sftp> ", 80);
        ed.feed(b"abc");
        ed.feed(b"\x1b[D\x1b[D");
        ed.feed(b"\x1b[3~");
        assert_eq!(ed.buffer(), "ac");
    }

    #[test]
    fn home_and_end_both_encodings() {
        let mut ed = LineEditor::new("sftp> ", 80);
        ed.feed(b"abc");
        ed.feed(b"\x1b[H");
        ed.feed(b"X");
        assert_eq!(ed.buffer(), "Xabc");
        ed.feed(b"\x1b[F");
        ed.feed(b"Y");
        assert_eq!(ed.buffer(), "XabcY");
        ed.feed(b"\x1b[1~");
        ed.feed(b"Z");
        assert_eq!(ed.buffer(), "ZXabcY");
        ed.feed(b"\x1b[4~");
        ed.feed(b"W");
        assert_eq!(ed.buffer(), "ZXabcYW");
    }

    #[test]
    fn ctrl_a_and_ctrl_e_move_to_the_edges() {
        let mut ed = LineEditor::new("sftp> ", 80);
        ed.feed(b"abc\x01X");
        assert_eq!(ed.buffer(), "Xabc");
        ed.feed(b"\x05Y");
        assert_eq!(ed.buffer(), "XabcY");
    }

    #[test]
    fn ctrl_u_kills_to_start_and_ctrl_k_to_end() {
        let mut ed = LineEditor::new("sftp> ", 80);
        ed.feed(b"get some/file");
        ed.feed(b"\x15");
        assert_eq!(ed.buffer(), "");
        ed.feed(b"get some/file\x01\x0b");
        assert_eq!(ed.buffer(), "");
    }

    #[test]
    fn ctrl_w_kills_the_previous_word() {
        let mut ed = LineEditor::new("sftp> ", 80);
        ed.feed(b"get some/file.txt");
        ed.feed(b"\x17");
        assert_eq!(ed.buffer(), "get ");
        ed.feed(b"\x17");
        assert_eq!(ed.buffer(), "");
    }

    /// Trailing spaces belong to the word being deleted, which is what
    /// makes a second Ctrl+W reach the word before it.
    #[test]
    fn ctrl_w_skips_trailing_spaces_first() {
        let mut ed = LineEditor::new("sftp> ", 80);
        ed.feed(b"get file   ");
        ed.feed(b"\x17");
        assert_eq!(ed.buffer(), "get ");
    }

    #[test]
    fn ctrl_c_discards_the_line_and_reports_it() {
        let mut ed = LineEditor::new("sftp> ", 80);
        ed.feed(b"rm -rf /");
        let (echo, evs) = ed.feed(b"\x03");
        assert_eq!(evs, vec![LineEvent::Interrupted]);
        assert_eq!(ed.buffer(), "");
        // The echo is the `^C` alone: the prompt that follows belongs to
        // the caller, which is what lets it carry the OSC 133 marks that
        // announce a new prompt.
        assert_eq!(String::from_utf8(echo).unwrap(), "^C\r\n");
    }

    /// Ctrl+D quits only on an empty line. On a non-empty one it is a
    /// forward delete, and confusing the two closes sessions people did
    /// not mean to close.
    #[test]
    fn ctrl_d_is_eof_only_when_the_line_is_empty() {
        let mut ed = LineEditor::new("sftp> ", 80);
        assert_eq!(events(&mut ed, "\u{4}"), vec![LineEvent::Eof]);

        ed.feed(b"abc\x01");
        let evs = ed.feed(b"\x04").1;
        assert!(evs.is_empty(), "expected a forward delete, got {evs:?}");
        assert_eq!(ed.buffer(), "bc");
    }

    #[test]
    fn history_walks_up_and_back_down() {
        let mut ed = LineEditor::new("sftp> ", 80);
        submit(&mut ed, "ls\r");
        submit(&mut ed, "pwd\r");
        ed.feed(b"\x1b[A");
        assert_eq!(ed.buffer(), "pwd");
        ed.feed(b"\x1b[A");
        assert_eq!(ed.buffer(), "ls");
        ed.feed(b"\x1b[B");
        assert_eq!(ed.buffer(), "pwd");
    }

    /// Walking back down past the newest entry restores what was being
    /// typed when the walk started, rather than leaving an empty line.
    #[test]
    fn history_restores_the_half_typed_line() {
        let mut ed = LineEditor::new("sftp> ", 80);
        submit(&mut ed, "ls\r");
        ed.feed(b"get par");
        ed.feed(b"\x1b[A");
        assert_eq!(ed.buffer(), "ls");
        ed.feed(b"\x1b[B");
        assert_eq!(ed.buffer(), "get par");
    }

    #[test]
    fn history_ignores_blank_and_repeated_lines() {
        let mut ed = LineEditor::new("sftp> ", 80);
        submit(&mut ed, "ls\r");
        submit(&mut ed, "   \r");
        submit(&mut ed, "ls\r");
        ed.feed(b"\x1b[A");
        assert_eq!(ed.buffer(), "ls");
        // Only one entry, so a second Up stays put.
        ed.feed(b"\x1b[A");
        assert_eq!(ed.buffer(), "ls");
    }

    #[test]
    fn history_is_capped() {
        let mut ed = LineEditor::new("sftp> ", 80);
        for i in 0..(HISTORY_LIMIT + 10) {
            submit(&mut ed, &format!("cmd{i}\r"));
        }
        assert_eq!(ed.history.len(), HISTORY_LIMIT);
        // The oldest entries were dropped, not the newest.
        assert_eq!(
            ed.history.last().unwrap(),
            &format!("cmd{}", HISTORY_LIMIT + 9)
        );
    }

    #[test]
    fn a_bare_enter_submits_an_empty_line() {
        let mut ed = LineEditor::new("sftp> ", 80);
        assert_eq!(submit(&mut ed, "\r"), "");
    }

    #[test]
    fn a_pasted_newline_submits_too() {
        let mut ed = LineEditor::new("sftp> ", 80);
        assert_eq!(submit(&mut ed, "pwd\n"), "pwd");
    }

    /// The request carries the whole line, not the word: the verb is what
    /// decides which namespace the word lives in, and it is not in the
    /// word.
    #[test]
    fn tab_asks_for_completion_with_the_whole_line() {
        let mut ed = LineEditor::new("sftp> ", 80);
        ed.feed(b"get /var/lo");
        let evs = ed.feed(b"\t").1;
        assert_eq!(
            evs,
            vec![LineEvent::CompleteRequested {
                line: "get /var/lo".to_string(),
                cursor: 11,
            }]
        );
    }

    /// A Tab in the MIDDLE of a line completes what is behind the cursor
    /// and leaves the rest where it is.
    #[test]
    fn tab_in_the_middle_reports_the_cursor_it_was_pressed_at() {
        let mut ed = LineEditor::new("sftp> ", 80);
        ed.feed(b"get ac.log");
        ed.feed(b"\x1b[D\x1b[D\x1b[D\x1b[D");
        let evs = ed.feed(b"\t").1;
        assert_eq!(
            evs,
            vec![LineEvent::CompleteRequested {
                line: "get ac.log".to_string(),
                cursor: 6,
            }]
        );
    }

    #[test]
    fn completion_replaces_the_span_and_leaves_the_rest() {
        let mut ed = LineEditor::new("sftp> ", 80);
        ed.feed(b"get /var/lo");
        ed.apply_completion(4, "/var/log/");
        assert_eq!(ed.buffer(), "get /var/log/");
    }

    #[test]
    fn completion_on_an_empty_word_still_works() {
        let mut ed = LineEditor::new("sftp> ", 80);
        ed.feed(b"get ");
        let evs = ed.feed(b"\t").1;
        assert_eq!(
            evs,
            vec![LineEvent::CompleteRequested {
                line: "get ".to_string(),
                cursor: 4,
            }]
        );
        ed.apply_completion(4, "access.log ");
        assert_eq!(ed.buffer(), "get access.log ");
    }

    /// Output printed mid-edit starts below the WHOLE line, not below the
    /// row the cursor happens to be on. On a wrapped line the difference
    /// is a candidate list painted over the second half of what the user
    /// typed.
    #[test]
    fn breaking_below_clears_a_wrapped_line_first() {
        let mut ed = LineEditor::new("sftp> ", 20);
        // 30 characters against a 20-column window: two rows, cursor at
        // the start of the first.
        ed.feed(&[b'x'; 30]);
        ed.feed(b"\x01");
        let bytes = String::from_utf8(ed.break_below()).unwrap();
        assert_eq!(bytes, "\x1b[1B\r\n");
    }

    #[test]
    fn breaking_below_a_short_line_is_just_a_newline() {
        let ed = LineEditor::new("sftp> ", 80);
        assert_eq!(ed.break_below(), b"\r\n".to_vec());
    }

    /// A stray ESC that starts no sequence is swallowed. Printing it
    /// would put an invisible byte into a filename.
    #[test]
    fn a_lone_escape_does_not_reach_the_buffer() {
        let mut ed = LineEditor::new("sftp> ", 80);
        ed.feed(b"\x1bZ");
        assert_eq!(ed.buffer(), "");
        ed.feed(b"ok");
        assert_eq!(ed.buffer(), "ok");
    }

    /// The emulator answers in-band queries (DECRQM, cursor position)
    /// down the same channel that carries the user's keystrokes, because
    /// the transport's write sender IS this console's input. Those
    /// replies carry intermediate bytes, and a decoder that treats `$`
    /// as the final byte leaks the character after it into the line the
    /// user is typing.
    #[test]
    fn a_decrqm_reply_does_not_leak_into_the_line() {
        let mut ed = LineEditor::new("sftp> ", 80);
        ed.feed(b"get ");
        ed.feed(b"\x1b[?2026;2$y");
        assert_eq!(ed.buffer(), "get ");
        ed.feed(b"file");
        assert_eq!(ed.buffer(), "get file");
    }

    #[test]
    fn a_cursor_position_report_is_swallowed_whole() {
        let mut ed = LineEditor::new("sftp> ", 80);
        ed.feed(b"ls");
        ed.feed(b"\x1b[24;80R");
        assert_eq!(ed.buffer(), "ls");
    }

    /// A device-attributes reply carries a `>` parameter byte, which is
    /// in the parameter range rather than the digit range the first
    /// version of this decoder accepted.
    #[test]
    fn a_device_attributes_reply_is_swallowed_whole() {
        let mut ed = LineEditor::new("sftp> ", 80);
        ed.feed(b"pwd");
        ed.feed(b"\x1b[>0;276;0c");
        assert_eq!(ed.buffer(), "pwd");
    }

    #[test]
    fn unhandled_control_bytes_are_dropped() {
        let mut ed = LineEditor::new("sftp> ", 80);
        ed.feed(b"a\x00\x1fb");
        assert_eq!(ed.buffer(), "ab");
    }

    // --- redraw geometry -------------------------------------------

    /// A CJK name advances two columns per character, so the cursor
    /// placement in a redraw has to measure width, not count characters.
    #[test]
    fn redraw_places_the_cursor_by_display_width() {
        let mut ed = LineEditor::new("sftp> ", 80);
        ed.feed("get 文档".as_bytes());
        let bytes = String::from_utf8(ed.redraw()).unwrap();
        // prompt (6) + "get " (4) + two wide chars (4) = column 14.
        assert!(bytes.ends_with("\r\x1b[14C"), "got {bytes:?}");
    }

    /// The reason `cols` is tracked at all: past the window width the
    /// redraw has to walk back up over the rows the line occupies, or it
    /// paints over the row above and the display never recovers.
    #[test]
    fn redraw_walks_back_over_wrapped_rows() {
        let mut ed = LineEditor::new("> ", 10);
        // 2 columns of prompt + 20 of text = 22 columns = rows 0..2.
        ed.feed(b"aaaaaaaaaaaaaaaaaaaa");
        let bytes = String::from_utf8(ed.redraw()).unwrap();
        assert!(bytes.starts_with("\x1b[2A"), "got {bytes:?}");
    }

    #[test]
    fn redraw_on_a_short_line_does_not_move_up() {
        let mut ed = LineEditor::new("> ", 80);
        ed.feed(b"abc");
        let bytes = String::from_utf8(ed.redraw()).unwrap();
        assert!(!bytes.contains("A"), "should not move up: {bytes:?}");
        assert!(bytes.ends_with("\r\x1b[5C"), "got {bytes:?}");
    }

    /// Resizing the window changes the geometry the redraw computes
    /// against; a stale width is how a line ends up drawn over itself.
    #[test]
    fn set_cols_changes_the_wrap_arithmetic() {
        let mut ed = LineEditor::new("> ", 80);
        ed.feed(b"aaaaaaaaaaaaaaaaaaaa");
        assert!(!String::from_utf8(ed.redraw()).unwrap().contains('A'));
        ed.set_cols(10);
        assert!(
            String::from_utf8(ed.redraw())
                .unwrap()
                .starts_with("\x1b[2A")
        );
    }

    #[test]
    fn cols_of_zero_is_clamped_rather_than_dividing_by_zero() {
        let mut ed = LineEditor::new("> ", 0);
        ed.feed(b"abc");
        // The assertion is that this does not panic.
        let _ = ed.redraw();
    }
}
