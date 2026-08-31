use super::*;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum SshError {
    #[error("Connection failed: {0}")]
    ConnectionFailed(String),

    #[error("Authentication failed")]
    AuthFailed,

    #[error("Channel error: {0}")]
    Channel(String),

    #[error("Russh error: {0}")]
    Russh(#[from] russh::Error),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Key error: {0}")]
    Key(String),

    #[error("Proxy error: {0}")]
    Proxy(String),

    /// A `ProxyType::Command` proxy was not approved for execution on
    /// this device, so the dial stopped before spawning anything.
    ///
    /// The command line is deliberately NOT in the message: it is
    /// user-authored and can embed credentials (the same reason the
    /// connect log prints only the proxy's type), and every caller that
    /// needs to show it already holds the connection it came from.
    #[error("Command proxy not approved on this device")]
    ProxyCommandNotApproved,

    /// A `ProxyType::Command` proxy failed on its way to carrying a
    /// dial. Kept apart from the free-text `Proxy` above so the cause
    /// survives as data; see [`ProxyCommandError`].
    #[error("Proxy error: {0}")]
    ProxyCommand(#[from] ProxyCommandError),

    #[error("Jump host error: {0}")]
    JumpHost(String),
}

/// Why a `ProxyType::Command` proxy could not carry a dial.
///
/// Structured rather than a formatted `SshError::Proxy(String)` for the
/// same reason [`NegotiationFailure`] is a type: naming the cause in the
/// user's language is the app's job, and parsing an error string back
/// apart is not a language boundary. Nothing here is translated yet (the
/// sentence the connect path wraps it in is still English too); the
/// shape is what makes translating it later a rendering change instead
/// of a rewrite.
#[derive(Debug, Clone, Error)]
pub enum ProxyCommandError {
    /// A `%h` / `%n` / `%r` value is not the shape of a host or a login
    /// name, so it never reached a shell. `token` is the spelling as
    /// written in the line (`"%h"`).
    ///
    /// The value is echoed because it is the connection's own hostname,
    /// label or username, all of which the host editor already shows in
    /// the clear. The command line, which may not be, still is not.
    #[error("ProxyCommand {token} refused: {value:?} is not a plain host or user name")]
    UnsafeValue {
        token: &'static str,
        value: String,
    },

    /// The local shell could not be started at all.
    #[error("ProxyCommand spawn: {0}")]
    Spawn(String),

    /// The proxy ran and the SSH transport over it failed. `stderr` is
    /// the proxy's own last words, which is usually the only account of
    /// why: russh only ever saw an EOF during version exchange.
    ///
    /// `transport` is the russh failure already rendered, not a
    /// `source`: naming the field that would make `thiserror` look for
    /// an `Error` behind it, and what is kept here is the sentence.
    #[error("SSH over ProxyCommand: {transport}{}", proxy_said(.stderr))]
    Transport {
        transport: String,
        stderr: Vec<String>,
    },
}

/// Render a proxy's last words onto the end of a transport failure.
///
/// Joined onto one line rather than stacked, because this lands in a
/// connect-progress row and an error dialog, neither of which is a log
/// viewer.
fn proxy_said(lines: &[String]) -> String {
    if lines.is_empty() {
        String::new()
    } else {
        format!(" (the proxy said: {})", lines.join(" | "))
    }
}

/// Which SSH negotiation category had no common algorithm. Mirrors the
/// per-host override categories so the UI can expand exactly the right
/// one (or all) on a legacy-fallback retry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NegCategory {
    Kex,
    HostKey,
    Cipher,
    Mac,
}

/// A "no common algorithm" handshake failure, surfaced structurally so
/// the app can offer "this server only speaks legacy X, connect anyway?"
/// rather than parsing an error string.
#[derive(Debug, Clone)]
pub struct NegotiationFailure {
    pub category: NegCategory,
    /// The algorithms the server offered for the failed category.
    pub server_offers: Vec<String>,
}

impl SshError {
    /// If this is a russh "no common algorithm" failure, return the
    /// failed category and what the server offered. Compression failures
    /// are not user-actionable here, so they map to `None`.
    pub fn negotiation_failure(&self) -> Option<NegotiationFailure> {
        let SshError::Russh(russh::Error::NoCommonAlgo { kind, theirs, .. }) = self else {
            return None;
        };
        let category = match kind {
            russh::AlgorithmKind::Kex => NegCategory::Kex,
            russh::AlgorithmKind::Key => NegCategory::HostKey,
            russh::AlgorithmKind::Cipher => NegCategory::Cipher,
            russh::AlgorithmKind::Mac => NegCategory::Mac,
            russh::AlgorithmKind::Compression => return None,
        };
        Some(NegotiationFailure {
            category,
            server_offers: theirs.clone(),
        })
    }

    /// If this is a command-proxy failure, the structured cause.
    ///
    /// The counterpart to [`Self::negotiation_failure`], and there for
    /// the same reason: it is what lets the app say why in the user's
    /// language without matching on a formatted string.
    pub fn proxy_command_failure(&self) -> Option<&ProxyCommandError> {
        match self {
            SshError::ProxyCommand(e) => Some(e),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Client handler
// ---------------------------------------------------------------------------

/// Result of checking a host key against known hosts.
#[derive(Debug, Clone)]
pub enum HostKeyStatus {
    /// Host is known and fingerprint matches, accept silently.
    Known,
    /// Host is known but fingerprint CHANGED, potential MITM.
    Changed { old_fingerprint: String },
    /// Host is not known, need to ask the user.
    Unknown,
}

/// Query about a host key that the UI must answer.
#[derive(Debug, Clone)]
pub struct HostKeyQuery {
    pub hostname: String,
    pub port: u16,
    pub key_type: String,
    pub fingerprint: String,
    pub status: HostKeyStatus,
}

/// Sync callback that checks known hosts and returns the status.
pub type HostKeyCheckCallback = Arc<dyn Fn(&str, u16, &str, &str) -> HostKeyStatus + Send + Sync>;

/// Channel for asking the UI to verify a host key. The UI sends `true` (accept) or `false` (reject).
pub type HostKeyAskSender = tokio::sync::mpsc::Sender<(HostKeyQuery, tokio::sync::oneshot::Sender<bool>)>;

/// Query about a command proxy the engine is about to spawn.
///
/// Same shape as [`HostKeyQuery`] on purpose: both ask the person at the
/// keyboard to accept something about the ROUTE before any traffic
/// flows, and a command proxy is the more consequential of the two,
/// since answering it runs a local process rather than trusting a
/// remote key.
#[derive(Debug, Clone)]
pub struct ProxyCommandQuery {
    /// The exact line that would be handed to `sh -c`. Shown verbatim:
    /// the user cannot judge what they are approving from a summary.
    pub command: String,
    /// The SSH endpoint this proxy would carry, so the prompt can say
    /// which connect raised it.
    pub target_host: String,
    pub target_port: u16,
}

/// Channel for asking the UI to approve spawning a command proxy. The
/// UI sends `true` (spawn) or `false` (refuse).
///
/// An engine built WITHOUT this channel refuses every command proxy.
/// That default is the point: headless callers (boot-time port forwards,
/// the MCP server) have nobody to ask, and a route that arrived from a
/// sync peer or an imported file must not execute merely because no
/// prompt was available.
pub type ProxyCommandAskSender =
    tokio::sync::mpsc::Sender<(ProxyCommandQuery, tokio::sync::oneshot::Sender<bool>)>;

/// A single keyboard-interactive prompt line. `prompt` is the raw label
/// the server sent (e.g. `"Password:"`, `"Verification code:"`) and must
/// be rendered verbatim, never translated. `echo` says whether the typed
/// answer should be visible (`true`) or masked (`false`).
#[derive(Debug, Clone)]
pub struct KbiPromptField {
    pub prompt: String,
    pub echo: bool,
}

/// A keyboard-interactive challenge round the UI must answer. `name` and
/// `instructions` are server-provided headers (e.g. `"Two-factor
/// authentication"`); both can be empty. One round can carry several
/// prompts (password + OTP, etc.).
#[derive(Debug, Clone)]
pub struct KbiQuery {
    pub name: String,
    pub instructions: String,
    pub prompts: Vec<KbiPromptField>,
}

/// Channel for asking the UI to answer a keyboard-interactive round. The
/// UI sends `Some(answers)` (one per prompt, in order) or `None` to
/// cancel the authentication.
pub type KbiAskSender =
    tokio::sync::mpsc::Sender<(KbiQuery, tokio::sync::oneshot::Sender<Option<Vec<String>>>)>;

/// How a keyboard-interactive exchange ended. `Rejected` (server said no,
/// or no answer source was available) and `Cancelled` (the user dismissed
/// the prompt) are kept apart so callers can fall back to another method
/// after a refusal without ever re-prompting after an explicit cancel.
/// `Partial` is RFC 4252 partial success: the exchange itself was
/// accepted, but the server requires one more of the carried methods
/// before granting access (issue #125).
pub(crate) enum KbiOutcome {
    Success,
    Rejected,
    Cancelled,
    Partial(russh::MethodSet),
}
