use iced::Task;

use crate::app::{Message, Oryxis};

impl Oryxis {
    /// Everything that must reach disk before this window goes away, on
    /// EVERY door that takes it away: the close verb (a real close and
    /// hide-to-tray both), the tray's Quit, the update restart and the
    /// renderer relaunch. ONE list, because four doors each carrying
    /// their own copy is how three of them drifted: the tray's Quit and
    /// both restart paths persisted the geometry and nothing else, so
    /// the tail of every recorded session and a half-typed host-editor
    /// form died with the process. Only the close verb was complete,
    /// and it is the one door close-to-tray takes away.
    ///
    /// Synchronous on purpose. The renderer relaunch exits through
    /// `std::process::exit`, which can neither await a `Task` nor run a
    /// destructor, so anything that only works as a task is a step that
    /// door cannot take (see `drain_plugins_before_exit`).
    pub(crate) fn persist_before_exit(&mut self) {
        // Buffered session-log output, trailing partial lines included:
        // the recording is going away with the pane, so there is no
        // later flush to carry the remainder.
        self.flush_session_logs_final();
        // A host-editor auto-save still inside its debounce window must
        // not die with the process. Interrupted: the window going away
        // concluded nothing about a half-typed Parent Group name, so it
        // must not become a vault group.
        self.editor_flush_interrupted();
        // Remember size + maximized/fullscreen for the next launch (also
        // on hide-to-tray: a later tray Quit exits without passing
        // through the close path again).
        self.persist_window_geometry();
    }

    /// Gracefully drain the plugin subprocesses (flush logs / close SDK
    /// clients on stdin EOF) so they aren't hard-killed with the app.
    /// Providers drain in parallel and the whole thing is time-bounded,
    /// so a wedged plugin can't hold the app open. The caller chains its
    /// own exit verb onto the returned task (`window::close` for the
    /// close and update doors, `iced::exit()` for the tray's Quit).
    ///
    /// Three doors of the four; the renderer relaunch is the exception
    /// and cannot be fixed by calling this. It leaves through
    /// `std::process::exit`, so there is no runtime left to await the
    /// drain, and blocking the UI thread on it would stall the frame
    /// that is about to spawn the replacement process. The subprocesses
    /// still see their stdin close when the parent goes, which is the
    /// same EOF this asks for, minus the wait.
    pub(crate) fn drain_plugins_before_exit(&self) -> Task<Message> {
        let providers: Vec<std::sync::Arc<crate::plugins::PluginProvider>> =
            self.plugin_providers.values().cloned().collect();
        Task::perform(
            async move {
                let drain = futures_util::future::join_all(
                    providers.iter().map(|p| p.shutdown()),
                );
                let _ = tokio::time::timeout(
                    std::time::Duration::from_millis(2000),
                    drain,
                )
                .await;
            },
            |_: ()| Message::NoOp,
        )
    }

    /// Best-effort persist a key/value pair to the vault. Logs failures
    /// instead of bubbling them up so a flaky disk doesn't take the
    /// whole settings panel down, the worst case is the user has to
    /// re-type on next boot.
    pub(crate) fn persist_setting(&self, key: &str, value: &str) {
        if let Some(vault) = &self.vault
            && let Err(e) = vault.set_setting(key, value)
        {
            tracing::warn!("failed to persist setting {key}: {e}");
        }
    }

    /// Persist the window geometry (last windowed size + outer position +
    /// the maximized / fullscreen flags) so the next launch reopens the
    /// window exactly as the user left it, on the same monitor. Every
    /// exit path reaches it through `persist_before_exit` above; it is
    /// also called on its own on the maximize / fullscreen toggles and
    /// on focus loss, as a crash-safe checkpoint. Plaintext settings
    /// rows, so this works while the vault is locked.
    pub(crate) fn persist_window_geometry(&self) {
        let w = self.window_windowed_size.width.round() as u32;
        let h = self.window_windowed_size.height.round() as u32;
        self.persist_setting("window_width", &w.to_string());
        self.persist_setting("window_height", &h.to_string());
        // No Moved event ever fired (fresh session on Wayland, or the
        // window was never dragged after a restore): keep whatever the
        // previous run stored rather than overwriting it with nothing.
        if let Some(pos) = self.window_windowed_pos {
            let x = pos.x.round() as i32;
            let y = pos.y.round() as i32;
            self.persist_setting("window_pos_x", &x.to_string());
            self.persist_setting("window_pos_y", &y.to_string());
        }
        self.persist_setting(
            "window_maximized",
            if self.window_maximized { "true" } else { "false" },
        );
        self.persist_setting(
            "window_fullscreen",
            if self.window_fullscreen { "true" } else { "false" },
        );
    }

    /// Persist the current column template (visibility + order + widths) so
    /// new panes/tabs inherit it across restarts.
    pub(crate) fn persist_sftp_columns(&self) {
        self.persist_setting("sftp_columns", &self.sftp_chrome.columns_template.visibility_storage());
        self.persist_setting("sftp_col_order", &self.sftp_chrome.columns_template.order_storage());
        self.persist_setting("sftp_col_widths", &self.sftp_chrome.columns_template.width_storage());
    }

    /// Snapshot the currently-pinned tabs (those with a reopenable spec) to
    /// the `pinned_tabs` setting so they reappear, dormant, next launch.
    /// Cloud / ephemeral pinned tabs have no spec and are skipped.
    pub(crate) fn persist_pinned_tabs(&self) {
        // De-duplicate by pin identity: a dormant placeholder and its
        // freshly-reopened live tab can briefly coexist (or a missed
        // replacement can leave both around), and persisting both
        // turns into duplicate chips on the next boot.
        let mut seen = std::collections::HashSet::new();
        // Persist in `tab_order` (the drag-reorderable display order) so the
        // restored pinned sequence matches what the user arranged, across both
        // terminal and SFTP tabs.
        let mut specs: Vec<crate::state::PinnedTabSpec> = Vec::new();
        for r in &self.tab_order {
            let spec = match r {
                crate::state::TabRef::Terminal(id) => self
                    .tabs
                    .iter()
                    .find(|t| t._id == *id)
                    .filter(|t| t.pinned)
                    .and_then(|t| t.pin_spec()),
                crate::state::TabRef::Sftp(id) => self
                    .sftp_tabs
                    .iter()
                    .position(|t| t.id == *id)
                    .filter(|&i| self.sftp_tabs[i].pinned)
                    .and_then(|i| self.sftp_pin_spec(i)),
                // Transient by design (issue #120): a restart should open
                // on real work, not on the settings screen.
                crate::state::TabRef::Panel(_) => None,
            };
            if let Some(spec) = spec
                && seen.insert(spec.dedupe_key())
            {
                specs.push(spec);
            }
        }
        let json = serde_json::to_string(&specs).unwrap_or_else(|_| "[]".into());
        self.persist_setting("pinned_tabs", &json);
    }

    /// Recreate pinned tabs as dormant placeholders at boot. They show in the
    /// strip with their saved label but hold no live session; selecting one
    /// the first time reopens it (see `reopen_dormant_tab`). Called once data
    /// is loaded so the reopen path can resolve host ids.
    pub(crate) fn restore_pinned_tabs_dormant(&mut self) {
        let json = self
            .vault
            .as_ref()
            .and_then(|v| v.get_setting("pinned_tabs").ok().flatten());
        let Some(json) = json else { return };
        let specs: Vec<crate::state::PinnedTabSpec> =
            serde_json::from_str(&json).unwrap_or_default();
        if specs.is_empty() {
            return;
        }
        // Heal any duplicates an older version persisted: one chip
        // per pin identity.
        let mut seen = std::collections::HashSet::new();
        // Pre-seed with pinned tabs already in the strip so a *re-run* of
        // `load_data_from_vault` (it fires on connection save, vault reload,
        // sync, ...) doesn't recreate dormant duplicates of live/dormant tabs
        // that already exist.
        for t in self.tabs.iter().filter(|t| t.pinned) {
            if let Some(s) = t.pin_spec() {
                seen.insert(s.dedupe_key());
            }
        }
        let existing_sftp_keys: Vec<String> = (0..self.sftp_tabs.len())
            .filter(|&i| self.sftp_tabs[i].pinned)
            .filter_map(|i| self.sftp_pin_spec(i).map(|s| s.dedupe_key()))
            .collect();
        seen.extend(existing_sftp_keys);
        for spec in specs {
            if !seen.insert(spec.dedupe_key()) {
                continue;
            }
            let label = spec.label().to_string();
            if matches!(spec, crate::state::PinnedTabSpec::Sftp { .. }) {
                // SFTP pinned tabs restore into `sftp_tabs` as dormant chips;
                // they re-mount their panes on first focus (see SelectSftpTab).
                let mut tab = crate::state::SftpTab::new_dormant(label, spec);
                tab.pinned = true;
                // Seed `tab_order` in the persisted (interleaved terminal+SFTP)
                // order so the restored strip matches what was saved, instead of
                // reconcile grouping all terminals before all SFTP tabs.
                self.tab_order.push(crate::state::TabRef::Sftp(tab.id));
                self.sftp_tabs.push(tab);
            } else {
                let tab = crate::state::TerminalTab::new_dormant_pinned(label, spec);
                self.tab_order.push(crate::state::TabRef::Terminal(tab._id));
                self.tabs.push(tab);
            }
        }
        // The tabs sit dormant in the strip; the app still boots to its
        // default view (Hosts). We deliberately do not focus a pinned tab or
        // switch to the terminal: opening always lands on Hosts, and a
        // dormant tab only connects on an explicit select.
    }
}
