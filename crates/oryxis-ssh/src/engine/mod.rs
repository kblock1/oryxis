use std::sync::Arc;

use russh::keys::{PublicKey, HashAlg, PrivateKeyWithHashAlg};
use russh::{client, ChannelMsg};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

use oryxis_core::models::connection::{
    AddressFamily, AuthMethod, Connection, PortForward, ProxyConfig, ProxyType,
};
use oryxis_core::models::port_forward_rule::{ForwardKind, PortForwardRule};
use thiserror::Error;

mod agent;
mod auth;
mod builder;
mod connect;
mod errors;
mod exec;
mod forwarding;
mod handler;
mod kbi;
mod monitor_conn;
mod net_quality;
mod proxy_consent;
mod proxy_spawn;
mod session;
mod transport;
mod terminfo;

pub use errors::*;
pub use forwarding::*;
pub use monitor_conn::MonitorConn;
pub use net_quality::{NetQuality, NetQualitySnapshot};
pub use proxy_consent::trusted_only_proxy_command_ask;
pub use session::*;
pub use transport::SshTransport;
pub use terminfo::TermFallback;
pub(crate) use terminfo::*;
pub(crate) use handler::*;
pub(crate) use proxy_spawn::ProxyTokens;
use agent::*;
use kbi::*;

// ---------------------------------------------------------------------------
// SSH Engine
// ---------------------------------------------------------------------------

/// A reading of what the reachable ssh-agents hold right now: sorted
/// `<endpoint> <fingerprint>` lines across every discovered agent (live
/// pipes on Windows, `SSH_AUTH_SOCK` on unix). Meaningless in isolation,
/// callers only ever compare it against an earlier reading: a difference
/// means the key situation moved.
///
/// The point is the arrival edge. A tool that hands its keys to an agent
/// only when its own database unlocks (KeePassXC, issue #101) leaves
/// anything that needed those keys failing until it runs, and the
/// endpoint set alone cannot see it: the key usually lands in an agent
/// that was already running. Watching the rosters turns "retry in up to
/// two minutes" into "retry on the next heartbeat".
///
/// Costs one dial + LIST per endpoint (each bounded, and LIST never
/// prompts), so poll it only while something is actually waiting on a
/// key, never on an idle app.
pub async fn agent_key_census() -> Vec<String> {
    agent::agent_key_census().await
}

/// The key material offered during publickey auth: the private key PEM
/// and, optionally, an OpenSSH user certificate to present alongside it
/// (B2). Bundling the two guarantees the certificate travels with the
/// exact key it was resolved from -- the caller builds this from a
/// single key lookup, so a cert can never be paired with the wrong key.
///
/// `Copy` (both fields are references) so it forwards through the connect
/// pipeline exactly like the `Option<&str>` it replaced.
#[derive(Clone, Copy)]
pub struct KeyMaterial<'a> {
    /// The decrypted private key in OpenSSH/PEM form.
    pub private_pem: &'a str,
    /// The full `ssh-*-cert-v01@openssh.com AAAA... comment` public line,
    /// when the key carries a certificate. `None` = plain publickey.
    pub certificate: Option<&'a str>,
}

impl<'a> KeyMaterial<'a> {
    /// A key with no certificate (plain publickey), the common case.
    pub fn plain(private_pem: &'a str) -> Self {
        Self { private_pem, certificate: None }
    }

    /// A key with an optional certificate.
    pub fn new(private_pem: &'a str, certificate: Option<&'a str>) -> Self {
        Self { private_pem, certificate }
    }
}

/// Resolves connections for jump hosts.
pub struct ConnectionResolver {
    pub connections: Vec<Connection>,
    pub passwords: std::collections::HashMap<uuid::Uuid, String>,
    pub private_keys: std::collections::HashMap<uuid::Uuid, String>,
    /// Per-jump-host OpenSSH certificate (B2), keyed like `private_keys`.
    /// Each hop authenticates with its own key, so its cert follows the
    /// same per-hop shape. Empty = no hop presents a certificate.
    pub certificates: std::collections::HashMap<uuid::Uuid, String>,
    /// Effective proxy per jump-host id, hydrated by the caller via
    /// `Vault::resolve_proxy`. Only the first jump's entry is used
    /// subsequent hops travel inside an SSH-tunneled `direct-tcpip`
    /// channel where a proxy doesn't apply.
    pub proxies: std::collections::HashMap<uuid::Uuid, ProxyConfig>,
}

pub struct SshEngine {
    host_key_check: Option<HostKeyCheckCallback>,
    host_key_ask_tx: Option<HostKeyAskSender>,
    /// Optional channel for surfacing keyboard-interactive prompts to the
    /// UI (set only for `AuthMethod::Interactive` on the terminal path).
    /// When absent, interactive auth degrades to filling every prompt with
    /// the stored password, so headless callers (boot port forwards) still
    /// work without a modal.
    kbi_ask_tx: Option<KbiAskSender>,
    /// Optional channel for asking the UI to approve a command proxy
    /// before it is spawned. Absent means REFUSE, never "spawn anyway":
    /// see [`ProxyCommandAskSender`] and `proxy_command`.
    proxy_cmd_ask_tx: Option<ProxyCommandAskSender>,
    /// Localized labels for the `AuthMethod::PasswordPrompt` modal
    /// (title + field label). The engine has no i18n, so the app injects
    /// the translated strings; `None` falls back to plain English for
    /// headless callers.
    pw_prompt_title: Option<String>,
    pw_prompt_label: Option<String>,
    /// Parsed per-connection TOTP generator. When set, keyboard-interactive
    /// rounds whose prompts look like an OTP ask ("Verification code:",
    /// "One-time password ...") are answered automatically instead of
    /// surfacing a modal. See `autofill_kbi_round`.
    totp: Option<oryxis_core::totp::Totp>,
    /// Optional client-side keepalive: when set, russh sends a no-op
    /// SSH_MSG_GLOBAL_REQUEST every N seconds so NAT / firewall idle
    /// timeouts don't kill the session.
    keepalive_interval: Option<std::time::Duration>,
    /// Per-host SSH rekey threshold in megabytes (C5), applied to russh's
    /// `Limits::rekey_write_limit` / `rekey_read_limit`. `None` keeps
    /// russh's 1 GB default. russh caps each limit at 1 GiB (nonce-reuse
    /// guard), so the value is clamped there.
    rekey_limit_mb: Option<u32>,
    /// Outbound address-family preference (PuTTY's Auto / IPv4 / IPv6),
    /// applied wherever this engine opens a real socket: the direct
    /// dial, the proxy dial, and the first jump hop. Later jump hops
    /// ride SSH channels, where no socket (or family) exists.
    address_family: AddressFamily,
    /// Phase-by-phase timeouts. Each step of the connect ladder gets
    /// its own bound so a misbehaving server can't hang the UI on any
    /// single stage. Defaults are sane (15s/30s/10s) and the user can
    /// override via the SFTP settings panel.
    connect_timeout: std::time::Duration,
    auth_timeout: std::time::Duration,
    session_timeout: std::time::Duration,
    /// Forward the local ssh-agent socket to the remote shell. Off by
    /// default (matches OpenSSH's default `ForwardAgent no`). Enabled
    /// per-connection from the editor; relayed both to the channel-
    /// level `auth-agent-req@openssh.com` request *and* to
    /// `ClientHandler` so we only accept forward channels we asked for.
    agent_forwarding: bool,
    /// Resolved local X11 endpoint, set when the host opted into X11
    /// forwarding AND a local display was found. Resolved ONCE at
    /// builder time so the fake cookie announced in `x11-req` is the
    /// same one the per-channel bridge verifies. `None` = no `x11-req`
    /// is sent and inbound X11 channels are refused.
    x11: Option<Arc<crate::x11::X11Forwarding>>,
    /// Per-host environment variables sent via `set_env` before the shell
    /// starts. `(name, value)` pairs. Non-fatal: most `sshd` only accept
    /// `LC_*` / `LANG_*` unless `AcceptEnv` is widened.
    env_vars: Vec<(String, String)>,
    /// Per-host character encoding label (e.g. `"Big5"`, `"Shift_JIS"`).
    /// `None` or UTF-8 means no transcoding (the terminal is UTF-8); any
    /// other charset is decoded to UTF-8 on the way in and encoded back on
    /// the way out.
    encoding: Option<String>,
    /// Per-host `TERM` name sent when requesting the PTY. `None` =
    /// `xterm-256color`. See `Connection.terminal_type`.
    terminal_type: Option<String>,
    /// Per-host SSH algorithm overrides (legacy-cipher support). Each
    /// `None` keeps russh's safe `Preferred` default for that category;
    /// `Some(list)` pins exactly those wire names (unknown names dropped).
    /// See `Connection.{ciphers,kex,macs,host_key_algorithms}`.
    algo_ciphers: Option<Vec<String>>,
    algo_kex: Option<Vec<String>>,
    algo_macs: Option<Vec<String>>,
    algo_host_keys: Option<Vec<String>>,
    /// Reject unknown/changed host keys when no UI ask channel is set
    /// (used by boot auto-started port forwards). See
    /// `ClientHandler::strict_host_key`.
    strict_host_key: bool,
    /// Quick-connect auth mode: when `AuthMethod::Auto` exhausts its
    /// silent methods (publickey, agent, stored password), surface the
    /// interactive prompt instead of failing, the way OpenSSH does.
    /// Off by default so saved Auto hosts keep the documented
    /// never-prompts behavior; only ad-hoc quick connects opt in.
    auto_interactive_fallback: bool,
    /// Set only for forward connections (`connect_forward_conn`).
    /// Propagated to the handler so inbound `forwarded-tcpip` channels
    /// are routed to the `-R` rule that owns each bind. See
    /// `ClientHandler::remote_routes`.
    remote_routes: Option<RemoteRouteMap>,
    /// Sink for pre-auth banners (RFC 4252 §5.4). See
    /// `ClientHandler::banner_tx`.
    banner_tx: Option<tokio::sync::mpsc::UnboundedSender<String>>,
    /// The host's preferred agent identity (B3): the public key of the
    /// vault key this connection references. Agent auth offers a matching
    /// agent identity FIRST (then the rest, preserving the try-all
    /// fallback), so a multi-key agent doesn't burn the server's
    /// MaxAuthTries before reaching e.g. the security key.
    pinned_agent_key: Option<russh::keys::PublicKey>,
}

impl Default for SshEngine {
    fn default() -> Self {
        Self::new()
    }
}

/// Resolve the RSA signature hash to sign a publickey auth with, from the
/// server's advertised `server-sig-algs` (RFC 8308 ext-info, as returned by
/// `Handle::best_supported_rsa_hash`).
///
/// The three cases map to a single concrete choice:
/// - `Some(Some(h))` the server named a concrete rsa-sha2 hash (256/512) it
///   accepts. Use it (this also upgrades modern servers to SHA-512 when they
///   offer it, rather than always signing SHA-256).
/// - `Some(None)` the server speaks ext-info but lists only legacy `ssh-rsa`.
///   Sign with SHA-1 (`None`).
/// - `None` the server sent no `server-sig-algs` at all, i.e. it predates
///   OpenSSH 7.4. Such servers only understand `ssh-rsa` / SHA-1, so sign
///   with that (`None`) instead of insisting on rsa-sha2-256, which they
///   reject with `unsupported public key algorithm: rsa-sha2-256`.
///
/// This collapses to `best_supported.flatten()`, but the explicit mapping is
/// the contract we unit-test in `legacy_cipher_tests`.
pub(crate) fn pick_rsa_hash(best_supported: Option<Option<HashAlg>>) -> Option<HashAlg> {
    match best_supported {
        Some(Some(h)) => Some(h),
        Some(None) | None => None,
    }
}

/// Ask the connected server which RSA hash it accepts and reduce it to the
/// hash to sign with. Non-fatal on error (the handle is likely dead anyway):
/// fall back to legacy `ssh-rsa` / SHA-1, the widest-compatible choice.
async fn server_rsa_hash(handle: &client::Handle<ClientHandler>) -> Option<HashAlg> {
    pick_rsa_hash(handle.best_supported_rsa_hash().await.unwrap_or(None))
}

fn parse_addr(addr: &str) -> Result<(String, u32), SshError> {
    let parts: Vec<&str> = addr.rsplitn(2, ':').collect();
    if parts.len() != 2 {
        return Err(SshError::ConnectionFailed(format!("Invalid address: {}", addr)));
    }
    let port: u32 = parts[0]
        .parse()
        .map_err(|_| SshError::ConnectionFailed(format!("Invalid port in: {}", addr)))?;
    Ok((parts[1].to_string(), port))
}

/// Build the request bytes for an HTTP CONNECT tunnel. When `username`
/// is provided, a `Proxy-Authorization: Basic` header is added (RFC
/// 7617). `password` may be `None` or empty, the colon separator is
/// always present per the spec.
fn build_http_connect_request(
    target_host: &str,
    target_port: u16,
    username: Option<&str>,
    password: Option<&str>,
) -> String {
    use base64::Engine as _;
    let mut req = format!(
        "CONNECT {host}:{port} HTTP/1.1\r\nHost: {host}:{port}\r\n",
        host = target_host,
        port = target_port,
    );
    if let Some(user) = username {
        let creds = format!("{}:{}", user, password.unwrap_or(""));
        let encoded = base64::engine::general_purpose::STANDARD.encode(creds);
        req.push_str(&format!("Proxy-Authorization: Basic {}\r\n", encoded));
    }
    req.push_str("\r\n");
    req
}

/// Parse the status code out of an HTTP/1.x response. Returns `None`
/// if the status line can't be read (e.g. the proxy spoke garbage).
fn parse_http_status(buf: &[u8]) -> Option<u16> {
    let line_end = buf.windows(2).position(|w| w == b"\r\n").unwrap_or(buf.len());
    let line = std::str::from_utf8(&buf[..line_end]).ok()?;
    let mut parts = line.split_whitespace();
    let _version = parts.next()?;
    parts.next()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_addr_valid() {
        let (host, port) = parse_addr("192.168.1.1:22").unwrap();
        assert_eq!(host, "192.168.1.1");
        assert_eq!(port, 22);
    }

    #[test]
    fn parse_addr_hostname() {
        let (host, port) = parse_addr("server.example.com:2222").unwrap();
        assert_eq!(host, "server.example.com");
        assert_eq!(port, 2222);
    }

    #[test]
    fn parse_addr_no_port_fails() {
        assert!(parse_addr("192.168.1.1").is_err());
    }

    #[test]
    fn parse_addr_bad_port_fails() {
        assert!(parse_addr("host:abc").is_err());
    }

    #[test]
    fn identity_agent_forward_slashes_normalized() {
        let conf = "IdentityAgent //./pipe/pageant.user.0123abcd\n";
        assert_eq!(
            parse_identity_agent(conf).as_deref(),
            Some(r"\\.\pipe\pageant.user.0123abcd")
        );
    }

    #[test]
    fn identity_agent_skips_comments_and_other_keys() {
        let conf = "# pageant\nForwardAgent yes\n  IdentityAgent  \"//./pipe/pageant.abc\"  \n";
        assert_eq!(
            parse_identity_agent(conf).as_deref(),
            Some(r"\\.\pipe\pageant.abc")
        );
    }

    #[test]
    fn identity_agent_equals_spelling() {
        let conf = "IdentityAgent=//./pipe/pageant.eq\n";
        assert_eq!(
            parse_identity_agent(conf).as_deref(),
            Some(r"\\.\pipe\pageant.eq")
        );
    }

    #[test]
    fn identity_agent_absent_returns_none() {
        assert_eq!(parse_identity_agent("ForwardAgent yes\n"), None);
    }

    // ── TOTP keyboard-interactive autofill ──

    fn test_totp() -> oryxis_core::totp::Totp {
        oryxis_core::totp::Totp::parse("JBSWY3DPEHPK3PXP").unwrap()
    }

    #[test]
    fn autofill_answers_otp_prompt() {
        let totp = test_totp();
        let answers =
            autofill_kbi_round(Some(&totp), false, ["Verification code: "], None).unwrap();
        assert_eq!(answers.len(), 1);
        assert_eq!(answers[0].len(), 6);
        assert!(answers[0].chars().all(|c| c.is_ascii_digit()));
    }

    #[test]
    fn autofill_mixed_round_uses_stored_password() {
        let totp = test_totp();
        let answers = autofill_kbi_round(
            Some(&totp),
            false,
            ["Password: ", "One-time password (OATH) for `alice':"],
            Some("hunter2"),
        )
        .unwrap();
        assert_eq!(answers[0], "hunter2");
        assert_eq!(answers[1].len(), 6);
    }

    #[test]
    fn autofill_surfaces_round_without_answers() {
        let totp = test_totp();
        // Password prompt but no stored password: the user must type it.
        assert!(autofill_kbi_round(
            Some(&totp),
            false,
            ["Password: ", "Verification code: "],
            None
        )
        .is_none());
        // Unrecognized prompt: never guess.
        assert!(autofill_kbi_round(
            Some(&totp),
            false,
            ["Enter the name of your first pet: "],
            Some("hunter2")
        )
        .is_none());
        // Password-only round: TOTP autofill is not a password autofill.
        assert!(
            autofill_kbi_round(Some(&totp), false, ["Password: "], Some("hunter2"))
                .is_none()
        );
        // No TOTP configured at all.
        assert!(
            autofill_kbi_round(None, false, ["Verification code: "], Some("pw")).is_none()
        );
    }

    #[test]
    fn autofill_bitvise_round_matches_via_context() {
        // Bitvise (issue #125) carries the OTP keywords in the round's
        // name/instructions and asks a bare "(account) Enter code:".
        let totp = test_totp();
        assert!(round_context_wants_otp(
            "TOTP authentication",
            "Get a time-based one time password from your authenticator.",
        ));
        let answers =
            autofill_kbi_round(Some(&totp), true, ["(alice@host) Enter code: "], None)
                .unwrap();
        assert_eq!(answers.len(), 1);
        assert_eq!(answers[0].len(), 6);
        assert!(answers[0].chars().all(|c| c.is_ascii_digit()));
    }

    #[test]
    fn autofill_context_never_widens_without_otp_wording() {
        let totp = test_totp();
        // A bare code ask with NO OTP context stays with the user.
        assert!(
            autofill_kbi_round(Some(&totp), false, ["Enter code: "], None).is_none()
        );
        // OTP context still never answers an unrecognized prompt.
        assert!(autofill_kbi_round(
            Some(&totp),
            true,
            ["Enter the name of your first pet: "],
            None
        )
        .is_none());
        // Neutral round metadata does not claim the round.
        assert!(!round_context_wants_otp("SSH server", "Please authenticate."));
    }

    #[test]
    fn identity_agent_empty_value_keeps_scanning() {
        // A first IdentityAgent line with an empty value must not abort
        // the scan; a later valid line still wins.
        let conf = "IdentityAgent \nIdentityAgent //./pipe/pageant.late\n";
        assert_eq!(
            parse_identity_agent(conf).as_deref(),
            Some(r"\\.\pipe\pageant.late")
        );
    }

    #[test]
    fn pageant_pipe_matches_current_user() {
        let names = vec![
            "openssh-ssh-agent".to_string(),
            "pageant.alice.deadbeef".to_string(),
        ];
        assert_eq!(
            pick_pageant_pipe(&names, Some("alice")).as_deref(),
            Some(r"\\.\pipe\pageant.alice.deadbeef")
        );
    }

    #[test]
    fn pageant_pipe_match_is_case_insensitive() {
        let names = vec!["Pageant.Alice.ABCD".to_string()];
        assert_eq!(
            pick_pageant_pipe(&names, Some("alice")).as_deref(),
            // Original casing preserved in the returned path.
            Some(r"\\.\pipe\Pageant.Alice.ABCD")
        );
    }

    #[test]
    fn pageant_pipe_user_segment_boundary() {
        // `alice` must not match another user whose name starts with it.
        let names = vec!["pageant.alice2.cafe".to_string()];
        assert_eq!(pick_pageant_pipe(&names, Some("alice")), None);
    }

    #[test]
    fn pageant_pipe_ignores_non_pageant_pipes() {
        let names = vec![
            "openssh-ssh-agent".to_string(),
            "discord-ipc-0".to_string(),
        ];
        assert_eq!(pick_pageant_pipe(&names, Some("alice")), None);
    }

    #[test]
    fn pageant_pipe_unknown_user_accepts_any_pageant() {
        let names = vec!["pageant.bob.f00d".to_string()];
        assert_eq!(
            pick_pageant_pipe(&names, None).as_deref(),
            Some(r"\\.\pipe\pageant.bob.f00d")
        );
        // ...but still requires the `<user>.<guid>` shape, not a bare prefix.
        assert_eq!(pick_pageant_pipe(&["pageant.".to_string()], None), None);
        assert_eq!(
            pick_pageant_pipe(&["pageant.solo".to_string()], None),
            None
        );
    }

    #[test]
    fn pageant_pipe_empty_list_is_none() {
        assert_eq!(pick_pageant_pipe(&[], Some("alice")), None);
    }

    #[test]
    fn pageant_pipes_returns_all_matches_in_order() {
        let names = vec![
            "pageant.alice.aaa".to_string(),
            "openssh-ssh-agent".to_string(),
            "Pageant.Alice.BBB".to_string(),
            "pageant.bob.ccc".to_string(),
        ];
        assert_eq!(
            pick_pageant_pipes(&names, Some("alice")),
            vec![
                r"\\.\pipe\pageant.alice.aaa".to_string(),
                r"\\.\pipe\Pageant.Alice.BBB".to_string(),
            ],
        );
    }

    // ── Agent candidate chains (issue #98) ──

    #[test]
    fn pipe_candidates_full_chain_in_order() {
        let names = vec!["pageant.alice.deadbeef".to_string()];
        let conf = "IdentityAgent //./pipe/pageant.conf-pipe.abc\n";
        assert_eq!(
            agent::windows_agent_pipe_candidates(&names, Some("alice"), Some(conf)),
            vec![
                r"\\.\pipe\pageant.alice.deadbeef".to_string(),
                r"\\.\pipe\pageant.conf-pipe.abc".to_string(),
                r"\\.\pipe\openssh-ssh-agent".to_string(),
            ],
        );
    }

    #[test]
    fn pipe_candidates_include_every_pageant_pipe() {
        // A locked KeePassXC and PuTTY Pageant publish pageant-style
        // pipes side by side; BOTH must be dialed. A first-match-only
        // pick would let enumeration order decide which one shadows
        // the other (the issue-#98 class, nondeterministic this time).
        let names = vec![
            "pageant.alice.keepassxc".to_string(),
            "discord-ipc-0".to_string(),
            "pageant.alice.putty".to_string(),
        ];
        assert_eq!(
            agent::windows_agent_pipe_candidates(&names, Some("alice"), None),
            vec![
                r"\\.\pipe\pageant.alice.keepassxc".to_string(),
                r"\\.\pipe\pageant.alice.putty".to_string(),
                r"\\.\pipe\openssh-ssh-agent".to_string(),
            ],
        );
    }

    #[test]
    fn pipe_candidates_no_pageant_still_reach_openssh_pipe() {
        // The #98 shape: no Pageant anywhere, the OpenSSH pipe must
        // still be dialed. The Oryxis agent's own pipe is deliberately
        // absent (we don't dial ourselves).
        assert_eq!(
            agent::windows_agent_pipe_candidates(&[], Some("alice"), None),
            vec![r"\\.\pipe\openssh-ssh-agent".to_string()],
        );
    }

    #[test]
    fn pipe_candidates_dedup_is_case_insensitive() {
        // A pageant.conf naming the live pipe (or a fixed pipe) with
        // different casing must not produce a duplicate dial.
        let names = vec!["Pageant.Alice.ABCD".to_string()];
        let conf = "IdentityAgent //./pipe/pageant.alice.abcd\n";
        assert_eq!(
            agent::windows_agent_pipe_candidates(&names, Some("alice"), Some(conf)),
            vec![
                r"\\.\pipe\Pageant.Alice.ABCD".to_string(),
                r"\\.\pipe\openssh-ssh-agent".to_string(),
            ],
        );
    }

    #[test]
    fn unix_sock_candidates_use_env_only() {
        // Only SSH_AUTH_SOCK is dialed; the Oryxis agent's own socket is
        // deliberately not appended (we don't dial ourselves).
        assert_eq!(
            agent::unix_agent_sock_candidates(Some("/run/user/1000/keyring/ssh".to_string())),
            vec![std::path::PathBuf::from("/run/user/1000/keyring/ssh")],
        );
    }

    #[test]
    fn unix_sock_candidates_empty_or_unset_env_is_none() {
        // Empty env var is not a candidate, and neither is an unset one.
        assert!(agent::unix_agent_sock_candidates(Some(String::new())).is_empty());
        assert!(agent::unix_agent_sock_candidates(None).is_empty());
    }

    // ── Agent presence fingerprint (issue #101) ──

    #[test]
    fn endpoint_discovery_only_reports_live_endpoints() {
        // The candidate list always ends on the OpenSSH name; the
        // fingerprint must not, or "an agent appeared" would already
        // read as true with no agent anywhere.
        assert!(agent::agent_endpoint_names(&[], Some("alice")).is_empty());
        assert_eq!(
            agent::agent_endpoint_names(&["openssh-ssh-agent".to_string()], Some("alice")),
            vec![r"\\.\pipe\openssh-ssh-agent".to_string()],
        );
    }

    #[test]
    fn endpoint_discovery_is_enumeration_order_independent() {
        // `FindFirstFile` order is arbitrary, so the same set of pipes
        // listed in a different order must compare equal: otherwise
        // every heartbeat would look like "an agent appeared".
        let a = vec![
            "pageant.alice.aaa".to_string(),
            "discord-ipc-0".to_string(),
            "openssh-ssh-agent".to_string(),
            "pageant.alice.bbb".to_string(),
        ];
        let b = vec![
            "openssh-ssh-agent".to_string(),
            "pageant.alice.bbb".to_string(),
            "pageant.alice.aaa".to_string(),
            "discord-ipc-0".to_string(),
        ];
        assert_eq!(
            agent::agent_endpoint_names(&a, Some("alice")),
            agent::agent_endpoint_names(&b, Some("alice")),
        );
        // ...and a genuinely new agent DOES change it.
        let mut c = a.clone();
        c.push("pageant.alice.ccc".to_string());
        assert_ne!(
            agent::agent_endpoint_names(&a, Some("alice")),
            agent::agent_endpoint_names(&c, Some("alice")),
        );
    }

    #[test]
    fn census_distinguishes_empty_agent_from_no_agent() {
        // The three states must not collapse: a reachable-but-empty
        // agent (locked KeePassXC holding its pipe open) has to differ
        // from an unreachable one, or the keys landing in it later
        // would not register as a change.
        assert!(agent::census_lines("/tmp/agent.sock", None).is_empty());
        assert_eq!(
            agent::census_lines("/tmp/agent.sock", Some(Vec::new())),
            vec!["/tmp/agent.sock <empty>".to_string()],
        );
    }

    #[test]
    fn unix_endpoints_require_the_socket_to_exist() {
        // An exported-but-dead SSH_AUTH_SOCK reads as "no agent", so the
        // socket showing up later registers as a change.
        assert!(agent::unix_agent_endpoints(Some(
            "/definitely/not/here/agent.sock".to_string()
        ))
        .is_empty());
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("agent.sock");
        std::fs::write(&sock, b"").unwrap();
        assert_eq!(
            agent::unix_agent_endpoints(Some(sock.display().to_string())),
            vec![sock.display().to_string()],
        );
    }

    #[test]
    fn parse_addr_ipv6() {
        let (host, port) = parse_addr("[::1]:22").unwrap();
        assert_eq!(host, "[::1]");
        assert_eq!(port, 22);
    }

    #[test]
    fn engine_new() {
        let engine = SshEngine::new();
        assert!(engine.host_key_check.is_none());
        assert!(engine.host_key_ask_tx.is_none());
        // Fail-closed by construction: a command proxy needs an
        // approval channel, and a fresh engine has none.
        assert!(engine.proxy_cmd_ask_tx.is_none());
    }

    /// The spawn gate, from both sides.
    ///
    /// The refusal is what makes every dial site safe without each one
    /// remembering to be: connection data can arrive over sync or from
    /// an imported file, and this is the single point where it would
    /// become a local process. A caller that forgets to wire the
    /// approval channel gets a refusal, never a spawn.
    #[tokio::test]
    async fn a_command_proxy_needs_an_approval_channel() {
        // Harmless if it ever does run, because the last branch below
        // approves it and then it really does: the first two are the
        // ones asserting nothing reaches a shell.
        const CMD: &str = "echo should-never-run";
        let dial = ProxyTokens {
            host: "host.example",
            port: 22,
            user: "root",
            name: "host.example",
        };

        let no_channel = SshEngine::new();
        assert!(matches!(
            no_channel.proxy_command(CMD, &dial).await.err(),
            Some(SshError::ProxyCommandNotApproved),
        ));

        // A channel whose answer is "no" is the same outcome: the
        // decision is the user's, the default is not to run.
        let refusing = SshEngine::new()
            .with_proxy_command_ask(trusted_only_proxy_command_ask(Default::default()));
        assert!(matches!(
            refusing.proxy_command(CMD, &dial).await.err(),
            Some(SshError::ProxyCommandNotApproved),
        ));

        // Approved: the same call now spawns, which is what keeps the
        // gate from breaking every legitimate ProxyCommand host.
        //
        // This half used to be Unix-only, because the spawn was `sh -c`
        // and a stock Windows box has no `sh`. That was not a quirk of
        // the test, it was the bug: a ProxyCommand host could not
        // connect on Windows at all. `proxy_spawn` picks the local
        // shell, so the assertion now runs everywhere and this is the
        // regression test for it.
        let approved = SshEngine::new().with_proxy_command_ask(trusted_only_proxy_command_ask(
            [oryxis_core::models::connection::proxy_command_fingerprint(CMD)]
                .into_iter()
                .collect(),
        ));
        assert!(approved.proxy_command(CMD, &dial).await.is_ok());
    }

    #[test]
    fn engine_with_callback() {
        let cb: HostKeyCheckCallback = Arc::new(|_h, _p, _t, _f| HostKeyStatus::Known);
        let engine = SshEngine::new().with_host_key_check(cb);
        assert!(engine.host_key_check.is_some());
    }

    // (Personal integration test against a private SSH server was
    // removed, it had hardcoded credentials and a path that only
    // existed on the original author's machine, and didn't compile in
    // CI anyway. End-to-end SSH coverage now lives in the
    // `tests/` directory once the harness is wired.)

    #[test]
    fn http_connect_request_unauthenticated() {
        let req = build_http_connect_request("example.com", 443, None, None);
        assert!(req.starts_with("CONNECT example.com:443 HTTP/1.1\r\n"));
        assert!(req.contains("Host: example.com:443\r\n"));
        assert!(!req.contains("Proxy-Authorization"));
        assert!(req.ends_with("\r\n\r\n"));
    }

    #[test]
    fn http_connect_request_with_basic_auth() {
        // RFC 7617, "Aladdin:open sesame" → "QWxhZGRpbjpvcGVuIHNlc2FtZQ=="
        let req = build_http_connect_request("h", 22, Some("Aladdin"), Some("open sesame"));
        assert!(req.contains("Proxy-Authorization: Basic QWxhZGRpbjpvcGVuIHNlc2FtZQ==\r\n"));
    }

    #[test]
    fn http_connect_request_with_user_no_password() {
        // No password → empty after colon (per RFC 7617).
        let req = build_http_connect_request("h", 22, Some("u"), None);
        // "u:" base64 = "dTo="
        assert!(req.contains("Proxy-Authorization: Basic dTo=\r\n"));
    }

    #[test]
    fn parse_http_status_ok() {
        assert_eq!(parse_http_status(b"HTTP/1.1 200 Connection established\r\n\r\n"), Some(200));
        assert_eq!(parse_http_status(b"HTTP/1.0 407 Proxy Authentication Required\r\n"), Some(407));
        assert_eq!(parse_http_status(b"HTTP/1.1 502 Bad Gateway\r\n"), Some(502));
    }

    #[test]
    fn parse_http_status_garbage() {
        assert_eq!(parse_http_status(b""), None);
        assert_eq!(parse_http_status(b"not http"), None);
        assert_eq!(parse_http_status(b"HTTP/1.1\r\n"), None);
    }
}
