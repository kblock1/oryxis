//! Minimal `~/.ssh/config` parser + Connection mapper.
//!
//! Handles the directives we actually use today: Host (block start),
//! HostName, Port, User, IdentityFile, ProxyJump, ProxyCommand,
//! ForwardAgent. Everything else is ignored. Wildcard host blocks
//! (`Host *`, `Host *.example.com`) are skipped on import, they're
//! templates, not concrete servers.

use std::path::PathBuf;

use oryxis_core::models::connection::{AuthMethod, Connection, ProxyConfig, ProxyType};

/// One parsed `Host` block from an SSH config file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SshConfigHost {
    /// The literal alias from the `Host` line, used as the connection
    /// label and as the fallback hostname when `HostName` is omitted.
    pub alias: String,
    pub hostname: Option<String>,
    pub port: Option<u16>,
    pub user: Option<String>,
    /// Every `IdentityFile` line in the block, in file order. OpenSSH
    /// ACCUMULATES them and offers each in turn, so this is a list and
    /// not the last one seen (which is what a single field made of a
    /// two-key block). `to_connection` keeps the first and records the
    /// rest, since a Connection names one key.
    pub identity_files: Vec<PathBuf>,
    /// First alias from `ProxyJump host[,host2,...]`. Only the first hop
    /// is recorded, multi-hop chains aren't supported on import yet
    /// because they'd require resolving multiple aliases at link time.
    pub proxy_jump: Option<String>,
    /// Verbatim `ProxyCommand` line. The four tokens `ssh_config(5)`
    /// lists for one (`%h` / `%n` / `%p` / `%r`) are kept as written: no
    /// shell expands those, OpenSSH resolves them itself and so does the
    /// engine, at spawn time and against the host being dialed (see
    /// `ProxyType::Command` and `proxy_spawn`). `%n` resolves to the
    /// connection label, which for an imported host is this block's
    /// `Host` alias, the same thing OpenSSH puts there.
    pub proxy_command: Option<String>,
    /// `ForwardAgent` directive, only `yes` flips it on; missing /
    /// `no` / anything else stays off, matching OpenSSH's default.
    pub forward_agent: bool,
    /// `ForwardX11` / `ForwardX11Trusted` directives. Either one set to
    /// `yes` flips it on: we only implement trusted forwarding, so a host
    /// asking for plain `ForwardX11` gets the mode that actually runs the
    /// GUI toolkits people forward X11 for.
    pub forward_x11: bool,
    /// `AddressFamily` directive: `inet` -> IPv4-only, `inet6` ->
    /// IPv6-only; `any`, missing, or anything else stays Auto
    /// (OpenSSH's own default is `any`).
    pub address_family: oryxis_core::models::connection::AddressFamily,
}

/// Parse the contents of an `ssh_config` file into a list of concrete
/// host blocks. Wildcards and the universal `*` block are dropped
/// they're config templates, not importable servers.
pub fn parse(text: &str) -> Vec<SshConfigHost> {
    let mut hosts: Vec<SshConfigHost> = Vec::new();
    let mut current: Option<SshConfigHost> = None;
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // Tolerate `key value`, `key = value`, and quoted values. Split
        // on first run of whitespace or `=`, strip surrounding quotes.
        let (key, value) = match split_key_value(line) {
            Some(parts) => parts,
            None => continue,
        };
        if key.eq_ignore_ascii_case("Host") {
            // First host name on the line wins, `Host alias1 alias2`
            // creates one block referenced by either alias, but for
            // import we just use the first.
            if let Some(prev) = current.take()
                && !is_wildcard(&prev.alias)
            {
                hosts.push(prev);
            }
            let alias = value.split_whitespace().next().unwrap_or("").to_string();
            current = Some(SshConfigHost {
                alias,
                ..Default::default()
            });
            continue;
        }
        let Some(host) = current.as_mut() else {
            continue;
        };
        match key.to_ascii_lowercase().as_str() {
            "hostname" => host.hostname = Some(value.to_string()),
            "port" => host.port = value.parse().ok(),
            "user" => host.user = Some(value.to_string()),
            "identityfile" => host.identity_files.push(expand_tilde(value)),
            "proxyjump" => {
                // OpenSSH allows `ProxyJump host1,host2,...`, keep only
                // the first hop; multi-hop linking on import is more
                // alias-resolution than we want to handle for v1.
                let first = value.split(',').next().unwrap_or("").trim();
                if !first.is_empty() {
                    host.proxy_jump = Some(first.to_string());
                }
            }
            "proxycommand" => host.proxy_command = Some(value.to_string()),
            "forwardagent" => host.forward_agent = value.eq_ignore_ascii_case("yes"),
            // `|=`: whichever of the two directives says `yes` wins, in
            // any order, so `ForwardX11 yes` + `ForwardX11Trusted no`
            // still imports as enabled.
            "forwardx11" | "forwardx11trusted" => {
                host.forward_x11 |= value.eq_ignore_ascii_case("yes")
            }
            "addressfamily" => {
                use oryxis_core::models::connection::AddressFamily;
                host.address_family = if value.eq_ignore_ascii_case("inet") {
                    AddressFamily::V4
                } else if value.eq_ignore_ascii_case("inet6") {
                    AddressFamily::V6
                } else {
                    // `any` and unknown values both mean no filter.
                    AddressFamily::Auto
                };
            }
            _ => {}
        }
    }
    if let Some(prev) = current.take()
        && !is_wildcard(&prev.alias)
    {
        hosts.push(prev);
    }
    hosts
}

/// Map a parsed entry onto an Oryxis `Connection`.
///
/// `IdentityFile` is carried over as the host's disk key
/// (`use_disk_key` + `identity_file`) rather than resolved to a vault
/// key id: the file is what the user's config names, it is right there
/// on disk, and importing it would be a second decision (with a
/// passphrase prompt) made silently on their behalf during an import.
/// The Keychain's own import is where a key becomes a vault key.
pub fn to_connection(host: &SshConfigHost) -> Connection {
    let hostname = host
        .hostname
        .clone()
        .unwrap_or_else(|| host.alias.clone());
    let mut conn = Connection::new(host.alias.clone(), hostname);
    if let Some(port) = host.port {
        conn.port = port;
    }
    if let Some(user) = &host.user {
        conn.username = Some(user.clone());
    }
    // An explicit IdentityFile becomes this host's disk key: the file
    // the config already names is the one that connects, with no import
    // step in between. The FIRST is kept because a Connection names one
    // key; the rest land in `notes` below, the same place an unresolved
    // ProxyJump alias goes, so nothing the config said is dropped in
    // silence.
    if let Some(first) = host.identity_files.first() {
        conn.use_disk_key = true;
        conn.identity_file = Some(first.display().to_string());
    }
    // A named IdentityFile still means Key rather than Auto: the config
    // picked a credential, so sweeping the agent's whole roster after it
    // is refused would spend the server's `MaxAuthTries` on keys the
    // user did not name. Without one, Auto handles whatever's available
    // (key, agent, password) at connect time.
    conn.auth_method = if conn.identity_file.is_some() {
        AuthMethod::Key
    } else {
        AuthMethod::Auto
    };
    conn.agent_forwarding = host.forward_agent;
    conn.x11_forwarding = host.forward_x11;
    conn.address_family = host.address_family;
    // ProxyCommand maps directly to our typed `Command(cmd)` proxy.
    // Linking a ProxyJump alias to an actual jump-host UUID happens in
    // a second pass (see `link_proxy_jumps`) once every block has its
    // own connection id assigned.
    if let Some(cmd) = &host.proxy_command {
        conn.proxy = Some(ProxyConfig {
            proxy_type: ProxyType::Command(cmd.clone()),
            host: String::new(),
            port: 0,
            username: None,
            password: None,
        });
    }
    // Drop the import provenance into notes so the user can find the
    // origin later, useful when reconciling with a manual edit. The
    // IdentityFile lines beyond the first ride along for the same
    // reason: the host can only offer one, and a line that vanished
    // without a trace is what makes an import untrustworthy.
    let mut notes = format!("Imported from ssh_config (alias `{}`)", host.alias);
    if host.identity_files.len() > 1 {
        let extra: Vec<String> = host.identity_files[1..]
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        notes.push_str(&format!(
            "\nAlso listed IdentityFile: {}",
            extra.join(", ")
        ));
    }
    conn.notes = Some(notes);
    // `HostName` is a dedicated directive, so it rarely carries a user,
    // but the ALIAS falls back into the same field when the block omits
    // it, and an alias is free text. Runs after `User` / `Port` are
    // mapped, so the directives keep winning over the host string
    // (issue #171).
    crate::importers::split_host_field(&mut conn);
    conn
}

/// Link `ProxyJump` aliases to their target Connection ids in a
/// second pass. Each `parsed[i]` line up 1-1 with `connections[i]` and
/// the parsed `proxy_jump` is an alias name, we look it up among the
/// imported aliases and append the matching id to `jump_chain`. An
/// unresolved alias (no `Host` block matches) is recorded in `notes`
/// so the user can fix it manually instead of having the import fail.
pub fn link_proxy_jumps(parsed: &[SshConfigHost], connections: &mut [Connection]) {
    use std::collections::HashMap;
    let alias_to_id: HashMap<&str, uuid::Uuid> = parsed
        .iter()
        .zip(connections.iter())
        .map(|(p, c)| (p.alias.as_str(), c.id))
        .collect();

    for (parsed_host, conn) in parsed.iter().zip(connections.iter_mut()) {
        let Some(target_alias) = parsed_host.proxy_jump.as_deref() else {
            continue;
        };
        match alias_to_id.get(target_alias) {
            Some(target_id) if *target_id != conn.id => {
                conn.jump_chain.push(*target_id);
            }
            Some(_) => {
                // Self-referential ProxyJump, pathological but possible
                // in malformed configs. Record and skip.
                let warn = format!(
                    "ProxyJump '{target_alias}' refers to this host itself, ignored",
                );
                conn.notes = Some(merge_note(conn.notes.take(), &warn));
            }
            None => {
                // Alias not present in the imported set. Could be a
                // template host (skipped), a typo, or a host the user
                // hasn't imported yet. Don't fail; tag for manual fix.
                let warn =
                    format!("ProxyJump alias '{target_alias}' not resolved, link manually");
                conn.notes = Some(merge_note(conn.notes.take(), &warn));
            }
        }
    }
}

fn merge_note(existing: Option<String>, addition: &str) -> String {
    match existing {
        Some(prev) if !prev.is_empty() => format!("{prev}\n{addition}"),
        _ => addition.to_string(),
    }
}

fn split_key_value(line: &str) -> Option<(&str, &str)> {
    // Recognise `key value`, `key=value`, or `key = value`. The split
    // happens on the first whitespace or `=`, whichever comes first.
    let split_at = line
        .char_indices()
        .find(|(_, c)| c.is_whitespace() || *c == '=')?
        .0;
    let key = line[..split_at].trim();
    let value = line[split_at..]
        .trim_start_matches(|c: char| c.is_whitespace() || c == '=')
        .trim();
    if key.is_empty() {
        return None;
    }
    let value = value.trim_matches('"');
    Some((key, value))
}

fn is_wildcard(alias: &str) -> bool {
    alias.contains('*') || alias.contains('?')
}

fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = home_dir()
    {
        return home.join(rest);
    }
    if path == "~"
        && let Some(home) = home_dir()
    {
        return home;
    }
    PathBuf::from(path)
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

/// Default location of the user's SSH config file. The import flow
/// uses this as the file picker's starting path.
pub fn default_config_path() -> Option<PathBuf> {
    home_dir().map(|h| h.join(".ssh").join("config"))
}

#[cfg(test)]
#[path = "ssh_config_tests.rs"]
mod tests;
