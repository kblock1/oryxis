//! The plain-text mirror of a live session recording (issue #187).
//!
//! Everything Oryxis records goes into the vault encrypted, and History
//! exports it afterwards. What this adds is the other half of the ask:
//! a file on disk that grows WHILE the session runs, for tailing from
//! another window and for handing to somebody who does not have the
//! vault.
//!
//! Two rules make it safe to offer at all. It is off by default and it
//! is NOT its own capture: the bytes come from the same flush that
//! feeds the vault, so a host set to never record produces no file
//! either, and the redaction that scrubs the stored chunk is the same
//! pass that reaches the disk. And what lands there is what a person
//! read, not the wire: the chunk goes through the linear ANSI renderer
//! (`ansi_render`, the transcript export's own pipeline), so a progress
//! bar is one line rather than a thousand escape sequences.

use std::io::Write;
use std::path::{Path, PathBuf};

use uuid::Uuid;

use crate::app::Oryxis;
use crate::state::PaneOrigin;

impl Oryxis {
    /// The folder the plain-text session logs live in: the configured
    /// setting, or `~/.oryxis/session-logs/` by default. Sibling of
    /// `command_history_dir`, and deliberately a different folder: one
    /// holds command lines, the other whole sessions.
    pub(crate) fn session_log_file_dir(&self) -> PathBuf {
        match &self.prefs.session_log_file_dir {
            Some(dir) => PathBuf::from(dir),
            None => oryxis_core::paths::oryxis_dir()
                .unwrap_or_else(|| PathBuf::from(".").join(".oryxis"))
                .join("session-logs"),
        }
    }

    /// Where THIS recording's mirror goes: `<label>-<date>-<id8>.txt`.
    ///
    /// The date is what makes a folder of these readable at a glance and
    /// the id is what keeps two sessions opened in the same second
    /// apart, so both are in the name. It is computed once per
    /// recording and parked on the pane, because a name recomputed per
    /// flush would move the file mid-session.
    pub(crate) fn session_log_file_path(&self, log_id: &Uuid, origin: &PaneOrigin) -> PathBuf {
        let label = match origin {
            PaneOrigin::Host(id) => self
                .connections
                .iter()
                .find(|c| c.id == *id)
                .map(|c| c.label.clone()),
            PaneOrigin::QuickHost(id) => {
                self.quick_connects.get(id).map(|e| e.conn.label.clone())
            }
            PaneOrigin::Local(spec) => Some(spec.label.clone()),
            PaneOrigin::Ephemeral => None,
        };
        let stem = crate::util::sanitize_file_stem(
            label.as_deref().filter(|l| !l.trim().is_empty()).unwrap_or("session"),
        );
        let id8 = log_id.simple().to_string();
        self.session_log_file_dir().join(format!(
            "{stem}-{}-{}.txt",
            chrono::Local::now().format("%Y%m%d-%H%M%S"),
            &id8[..8]
        ))
    }

    /// Append one flushed chunk to the mirror, creating the folder and
    /// the file on the first call.
    ///
    /// Owner-only (0700 / 0600) like the command log and the vault file
    /// itself: the content is plaintext by design, which is no reason to
    /// let the other accounts on the machine read a session.
    pub(crate) fn append_session_log_file(
        &self,
        path: &Path,
        data: &[u8],
    ) -> std::io::Result<()> {
        let palette = self.resolve_global_terminal_palette();
        let text: String = crate::ansi_render::render(data, &palette)
            .iter()
            .map(|s| s.text.as_str())
            .collect();
        if text.is_empty() {
            return Ok(());
        }
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
            }
        }
        let fresh = !path.exists();
        let mut opts = std::fs::OpenOptions::new();
        opts.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut file = opts.open(path)?;
        if fresh {
            // A header, once: a file found months later has to say what
            // it is a recording OF, and the plain warning belongs on the
            // artifact rather than only in the setting that made it.
            writeln!(
                file,
                "# Oryxis session log, started {}\n# Not encrypted.\n",
                chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
            )?;
        }
        file.write_all(text.as_bytes())
    }
}
