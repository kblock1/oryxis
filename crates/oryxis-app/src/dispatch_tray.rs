//! `Oryxis::handle_tray`: settings-panel-independent dispatch arms for the
//! tray area, split out of dispatch.rs. Returns `Err(message)` for anything
//! it doesn't claim so the try_handler! chain falls through.
#![allow(clippy::result_large_err)]

use iced::Task;

use crate::app::{TabsMessage, SshMessage, TrayMessage, Message, Oryxis};

impl Oryxis {
    pub(crate) fn handle_tray(
        &mut self,
        message: TrayMessage,
    ) -> Task<Message> {
        match message {
            // A `oryxis://` URL forwarded by a launcher process and
            // claimed by this window's deep-link subscription. Not
            // tray-related beyond sharing the instance-IPC plumbing
            // (tray_ipc owns the inbox), hence its home here.
            TrayMessage::DeepLink(url) => {
                return self.handle_deep_link_url(&url);
            }
            // `oryxis user@host` run while this window was already up.
            // Dials, unlike the deep-link route: the target came from
            // the user's own shell, not from a page they clicked.
            TrayMessage::ConnectTarget(target) => {
                let route = self.handle_connect_target(&target);
                #[cfg(target_os = "windows")]
                {
                    return Task::batch([Task::done(Message::Tray(TrayMessage::Show)), route]);
                }
                #[cfg(not(target_os = "windows"))]
                {
                    return route;
                }
            }
            // -- System tray --
            TrayMessage::Poll => {
                // A native minimize verb (taskbar button, Win+Down,
                // Alt+Space menu) is swallowed by the Win32 subclass
                // in `tray`, which hides the window from inside the
                // window procedure and leaves this flag behind. It
                // can't touch app state from there, so the same
                // bookkeeping the chrome-button path does inline
                // happens here instead. Drained BEFORE the signature
                // below so the menu rebuild + `set_visible` see the
                // new hidden state on this very tick.
                if crate::tray::take_native_hide() {
                    self.is_window_hidden = true;
                    crate::tray::set_visible(true);
                    self.broadcast_ipc_state_if_child();
                }
                // Windows JumpList refresh rides this unconditional-Windows
                // tick (it is NOT gated on the tray setting; if TrayPoll
                // ever becomes tray-gated, move this to its own timer).
                // Cheap: hashes the top-10 recent hosts and only touches
                // the shell when that set changed.
                self.refresh_jumplist();
                // 500 ms multi-window IPC heartbeat. Rebuild the dynamic
                // submenu (Active sessions + Recent hosts) when the state
                // behind it changed, plus the child IPC command drain and
                // promotion below. Menu / icon clicks no longer come
                // through here, they arrive event-driven via
                // `TrayMenuEvent` / `TrayIconDoubleClick`, so this timer
                // only carries the housekeeping that genuinely needs a
                // tick. The signature is a hash of the tab count +
                // connection last_used times; the hash is cheap but the
                // IPC registry scan behind it stats files, which the
                // 500 ms cadence keeps infrequent.
                {
                    use std::collections::hash_map::DefaultHasher;
                    use std::hash::{Hash, Hasher};

                    // Labels handed to the OS menu go through the same
                    // Privacy Mode redactor as the tab strip / status
                    // bar (issue #78): the tray outlives the app's own
                    // chrome, so raw host labels must never leave the
                    // process while privacy is on (a no-op when it's
                    // off). The menu lists are built every tick and
                    // the signature is taken over what actually
                    // renders, so a privacy toggle or per-host
                    // override flip refreshes the menu like any label
                    // change. One terms pass per tick, never per row.
                    let privacy_terms = self.privacy_terms();
                    // `&` is the Windows menu accelerator prefix:
                    // a host named "R&D" would render as "RD" with
                    // D underlined. Doubling the `&` escapes it.
                    // Capped at 20: a user with 50+ open tabs gets
                    // an unwieldy submenu otherwise; recent-hosts
                    // submenu already had a `.take(10)` for the
                    // same reason. Redaction keys on the automatic
                    // label (volatile OSC titles stay out on purpose:
                    // hashing them would churn the menu rebuild) so a
                    // custom rename keeps the per-host override.
                    let active: Vec<(String, String)> = self
                        .tabs
                        .iter()
                        .enumerate()
                        .take(20)
                        .map(|(i, t)| {
                            let shown = t.custom_name.as_deref().unwrap_or(&t.label);
                            let shown =
                                self.privacy_display_label(&t.label, shown, &privacy_terms);
                            (shown.replace('&', "&&"), i.to_string())
                        })
                        .collect();
                    // Recent hosts: top 10 by last_used desc.
                    // Hosts that were never connected drop to
                    // the bottom and get sliced off, so the
                    // menu only lists hosts the user actually
                    // touched.
                    let mut recent_pairs: Vec<&oryxis_core::models::connection::Connection> =
                        self.connections.iter().filter(|c| c.last_used.is_some()).collect();
                    recent_pairs.sort_by_key(|c| std::cmp::Reverse(c.last_used));
                    let recent: Vec<(String, String)> = recent_pairs
                        .iter()
                        .take(10)
                        .map(|c| {
                            let shown =
                                self.privacy_display_label(&c.label, &c.label, &privacy_terms);
                            (shown.replace('&', "&&"), c.id.to_string())
                        })
                        .collect();
                    // Unified "Windows" list: every window the
                    // user owns that's currently hidden, primary
                    // first (when the primary itself is hidden)
                    // then each hidden child via the IPC registry.
                    // The id-suffix is the owning process's PID;
                    // the menu click dispatcher checks self_pid
                    // to decide between local TrayShow and an
                    // IPC send_command. Cheap: list_instances does
                    // one dir scan + PID liveness check per entry,
                    // which on a typical setup means <5 file reads.
                    let mut hidden: Vec<(String, String)> = Vec::new();
                    if self.is_window_hidden {
                        let primary_label = self
                            .active_tab
                            .and_then(|i| self.tabs.get(i))
                            .map(|t| {
                                let shown = t.custom_name.as_deref().unwrap_or(&t.label);
                                self.privacy_display_label(&t.label, shown, &privacy_terms)
                            })
                            .unwrap_or_else(|| crate::i18n::t("tray_main_window").to_string());
                        hidden.push((
                            primary_label.replace('&', "&&"),
                            std::process::id().to_string(),
                        ));
                    }
                    for inst in crate::tray_ipc::Primary::list_instances() {
                        if !inst.is_hidden {
                            continue;
                        }
                        // A child's title is a host label from another
                        // window over the same vault; redact it with
                        // this process's terms like our own labels
                        // (per-host overrides resolve by label lookup,
                        // global setting is the fallback).
                        let label = if inst.title.is_empty() || inst.title == "Oryxis" {
                            format!("{} (PID {})", crate::i18n::t("tray_main_window"), inst.pid)
                        } else {
                            self.privacy_display_label(&inst.title, &inst.title, &privacy_terms)
                        };
                        hidden.push((label.replace('&', "&&"), inst.pid.to_string()));
                    }
                    // Signature over the rendered lists plus our OWN
                    // hidden state, which drives both the "Windows"
                    // section and `set_visible` below, so it has to be
                    // part of the signature. Leaving it out meant a
                    // primary that hid itself produced an unchanged
                    // signature, skipped the whole block, and never
                    // called `set_visible(true)`: the window was gone
                    // and no tray icon ever appeared to bring it back.
                    let mut h = DefaultHasher::new();
                    self.is_window_hidden.hash(&mut h);
                    active.hash(&mut h);
                    recent.hash(&mut h);
                    hidden.hash(&mut h);
                    let sig = h.finish();
                    if sig != self.tray_menu_signature {
                        self.tray_menu_signature = sig;
                        if let Err(e) = crate::tray::rebuild_menu(&active, &recent, &hidden) {
                            tracing::warn!("tray menu rebuild failed: {e}");
                        }
                        // Tray icon is only visible when at least
                        // one window (primary's own or any child's)
                        // is currently hidden. The "1 tray to rule
                        // them all" UX the user asked for: when
                        // everything's visible on screen there's no
                        // reason to clutter the notification area
                        // with a redundant icon.
                        let any_hidden = self.is_window_hidden || !hidden.is_empty();
                        crate::tray::set_visible(any_hidden);
                    }
                }
                // Drain whatever the tray-icon crate's event threads
                // queued since the last poll. Each menu id resolves
                // to a real Message via Task::batch so we can emit
                // more than one event per tick if the user spam-
                // clicked. On non-Windows targets both polls return
                // None immediately, so this is harmless overhead.
                let mut follow_ups: Vec<Task<Message>> = Vec::new();

                // One-shot: tag the main window with the JumpList AUMID so
                // its taskbar button adopts the identity the list is filed
                // under. Needs the raw HWND, so it hops through
                // `iced::window::run` on the UI thread. No-op off Windows.
                if !self.jumplist_window_tagged {
                    self.jumplist_window_tagged = true;
                    follow_ups.push(
                        iced::window::oldest()
                            .and_then(|id| {
                                iced::window::run(id, |window| {
                                    crate::jumplist::tag_window(window);
                                })
                            })
                            .discard(),
                    );
                }

                // One-shot: subclass the window so the OS minimize
                // verbs honour minimize-to-tray. Needs the raw HWND,
                // so it hops through `iced::window::run` like the
                // JumpList tag above. Retried each tick until it
                // lands (the window may not exist yet on the first
                // ticks); the static flag stops it afterwards. Every
                // process does this for its own window, children
                // included: they have no tray icon, but the primary's
                // menu lists their hidden windows via the IPC
                // registry. No-op off Windows.
                if !crate::tray::minimize_hook_installed() {
                    follow_ups.push(
                        iced::window::oldest()
                            .and_then(|id| {
                                iced::window::run(id, |window| {
                                    crate::tray::install_minimize_hook(window);
                                })
                            })
                            .discard(),
                    );
                }

                // Push our state into the tray_ipc registry so the
                // primary's "Hidden windows" menu reflects any tab
                // label edits / new sessions / etc. between explicit
                // hide/show events. No-op for the primary itself.
                self.broadcast_ipc_state_if_child();

                // Drain whatever command the primary queued for us
                // (a Show or Quit from a click in its tray menu).
                // No-op for the primary process (it never has its
                // own command file because we skip self_pid in
                // Primary::list_instances).
                let is_primary = crate::app::APP_IS_PRIMARY
                    .load(std::sync::atomic::Ordering::Relaxed);
                if !is_primary {
                    while let Some(cmd) = crate::tray_ipc::Child::poll_command() {
                        match cmd {
                            crate::tray_ipc::Command::Show => {
                                follow_ups.push(Task::done(Message::Tray(TrayMessage::Show)));
                            }
                            crate::tray_ipc::Command::Quit => {
                                follow_ups.push(Task::done(Message::Tray(TrayMessage::Quit)));
                            }
                        }
                    }

                    // Promotion check: if the primary process
                    // exited (mutex released) one of the surviving
                    // children needs to take over so the user
                    // doesn't end up with orphaned hidden windows
                    // and no tray to surface them. try_acquire_mutex
                    // succeeds when nobody else owns the mutex; the
                    // first child to win the race becomes the new
                    // primary, installs the tray, and unregisters
                    // its own IPC row.
                    if crate::tray::try_acquire_mutex() {
                        tracing::info!("tray IPC: promoting to primary (old primary gone)");
                        crate::app::APP_IS_PRIMARY
                            .store(true, std::sync::atomic::Ordering::Relaxed);
                        if let Err(e) = crate::tray::install() {
                            tracing::warn!("tray install on promotion: {e}");
                        }
                        crate::tray_ipc::Child::unregister();
                    }
                }

                // Menu / icon clicks are NOT drained here anymore: they
                // arrive event-driven as `TrayMenuEvent` /
                // `TrayIconDoubleClick` (see `subscription::tray_event_stream`),
                // so a click wakes the UI on its own instead of this
                // timer polling for one 10x/s. Only the child IPC
                // commands collected above remain.
                if !follow_ups.is_empty() {
                    return Task::batch(follow_ups);
                }
            }
            TrayMessage::MenuEvent(id) => {
                // A tray menu item was clicked (delivered event-driven).
                // Resolve its id to a concrete action; unknown ids and
                // the cross-process "show that PID's window" case (which
                // only sends an IPC command) resolve to no local Message.
                let msg = match id.as_str() {
                    crate::tray::MENU_ID_SHOW => Some(Message::Tray(TrayMessage::Show)),
                    crate::tray::MENU_ID_HIDE => Some(Message::Tray(TrayMessage::Hide)),
                    crate::tray::MENU_ID_QUIT => Some(Message::Tray(TrayMessage::Quit)),
                    s if s.starts_with(crate::tray::MENU_PREFIX_SESSION) => {
                        // "oryxis-tray-session:<idx>" -> activate that open
                        // tab. The dispatcher already has TabSelect plumbed
                        // through every path that switches the active pane.
                        let suffix = &s[crate::tray::MENU_PREFIX_SESSION.len()..];
                        suffix.parse::<usize>().ok().and_then(|idx| {
                            if idx < self.tabs.len() {
                                Some(Message::Tray(TrayMessage::ActivateSession(idx)))
                            } else {
                                None
                            }
                        })
                    }
                    s if s.starts_with(crate::tray::MENU_PREFIX_HOST) => {
                        // "oryxis-tray-host:<uuid>" -> open a new tab
                        // against that saved connection.
                        let suffix = &s[crate::tray::MENU_PREFIX_HOST.len()..];
                        uuid::Uuid::parse_str(suffix).ok().map(|v| Message::Tray(TrayMessage::OpenHost(v)))
                    }
                    s if s.starts_with(crate::tray::MENU_PREFIX_HIDDEN) => {
                        // "oryxis-tray-hidden:<pid>". Our own pid -> show
                        // the primary's hidden window locally. Otherwise
                        // queue an IPC Show command for that child, whose
                        // own heartbeat routes it into TrayShow.
                        let suffix = &s[crate::tray::MENU_PREFIX_HIDDEN.len()..];
                        if let Ok(pid) = suffix.parse::<u32>() {
                            if pid == std::process::id() {
                                Some(Message::Tray(TrayMessage::Show))
                            } else {
                                crate::tray_ipc::Primary::send_command(
                                    pid,
                                    crate::tray_ipc::Command::Show,
                                );
                                None
                            }
                        } else {
                            None
                        }
                    }
                    _ => None,
                };
                if let Some(m) = msg {
                    return Task::done(m);
                }
            }
            TrayMessage::IconDoubleClick => {
                // Double-click on the icon body restores the window.
                return Task::done(Message::Tray(TrayMessage::Show));
            }
            TrayMessage::Show => {
                // Hop through iced::window::oldest -> window::run so
                // we get the raw window handle on the UI thread. The
                // tray hide/show helpers swallow non-Windows targets
                // (stubs return false), so this is a no-op outside
                // Windows even though the code compiles everywhere.
                // `.discard()` drops the `()` return so the chain
                // matches the dispatcher's `Task<Message>` shape.
                self.is_window_hidden = false;
                self.broadcast_ipc_state_if_child();
                return iced::window::oldest()
                    .and_then(|id| {
                        iced::window::run(id, |window| {
                            crate::tray::show_window(window);
                        })
                    })
                    .discard();
            }
            TrayMessage::Hide => {
                self.is_window_hidden = true;
                self.broadcast_ipc_state_if_child();
                return iced::window::oldest()
                    .and_then(|id| {
                        iced::window::run(id, |window| {
                            crate::tray::hide_window(window);
                        })
                    })
                    .discard();
            }
            TrayMessage::Quit => {
                // The same opt-in guard the tab closes and the window's
                // X use. The tray menu is the only exit verb once the
                // window is hidden to the tray, so leaving it unguarded
                // would let the path that always drops every live
                // session skip the very protection the user asked for.
                if self.prefs.confirm_close_session_tab {
                    let live = self.live_session_tab_count();
                    if live > 0 {
                        // Show the window first: a dialog on a hidden
                        // window is invisible, and a user who then
                        // kills the process from Task Manager loses
                        // the sessions unasked anyway.
                        self.overlay = None;
                        self.error_dialog = Some(crate::state::ErrorDialog {
                            title: crate::i18n::t("close_window_title").to_string(),
                            body: crate::i18n::t("close_window_body")
                                .replacen("{n}", &live.to_string(), 1),
                            link: None,
                            action: Some(crate::state::ErrorDialogAction {
                                label: crate::i18n::t("close_window_confirm").to_string(),
                                message: Box::new(Message::Tray(TrayMessage::ConfirmQuit)),
                                danger: true,
                            }),
                        });
                        return Task::done(Message::Tray(TrayMessage::Show));
                    }
                }
                return self.tray_quit_now();
            }
            TrayMessage::ConfirmQuit => {
                return self.tray_quit_now();
            }
            TrayMessage::ActivateSession(idx) => {
                // Show first (window may be hidden) then re-emit
                // SelectTab via Task::done. Bundled together so the
                // user sees the tab swap and the window pop in the
                // same frame.
                if idx < self.tabs.len() {
                    return Task::batch(vec![
                        Task::done(Message::Tray(TrayMessage::Show)),
                        Task::done(Message::Tabs(TabsMessage::SelectTab(idx))),
                    ]);
                }
            }
            TrayMessage::OpenHost(uuid) => {
                if let Some(idx) =
                    self.connections.iter().position(|c| c.id == uuid)
                {
                    return Task::batch(vec![
                        Task::done(Message::Tray(TrayMessage::Show)),
                        Task::done(Message::Ssh(SshMessage::ConnectSsh(idx))),
                    ]);
                }
            }
        }
        Task::none()
    }

    /// The tray's exit itself, with no prompt. Reached directly when
    /// the live-session guard is off or nothing was live, and from
    /// `ConfirmQuit` once the ask is answered.
    ///
    /// With close-to-tray on this is the app's ONLY exit verb (the
    /// close verb hides instead), which is why it takes the same
    /// teardown the close path does rather than a shorter one of its
    /// own: it used to persist the geometry and nothing else, so the
    /// tail of every recorded session and a half-typed host-editor form
    /// died with the process.
    fn tray_quit_now(&mut self) -> Task<Message> {
        tracing::info!("tray: quit requested");
        // The window may have been shown and resized / maximized since
        // the hide-to-tray persisted geometry, so this writes the final
        // state along with the two flushes.
        self.persist_before_exit();
        self.drain_plugins_before_exit().then(|_| iced::exit())
    }

    /// Rebuild the Windows taskbar JumpList's recent-hosts category when
    /// the set changed. Same list as the tray "Recent hosts" submenu (top
    /// 10 saved connections by `last_used`, never-connected ones filtered
    /// out). Gated on its own signature so the shell is only touched on a
    /// real change; a full no-op off Windows.
    pub(crate) fn refresh_jumplist(&mut self) {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let mut recent: Vec<&oryxis_core::models::connection::Connection> = self
            .connections
            .iter()
            .filter(|c| c.last_used.is_some())
            .collect();
        recent.sort_by_key(|c| std::cmp::Reverse(c.last_used));
        // JumpList titles are persisted by the shell, so they go
        // through the same Privacy Mode redactor as the tab strip
        // (issue #78); the entry still launches by uuid, so a
        // redacted title stays clickable. A no-op when privacy is
        // off. One terms pass per refresh, never per row.
        let privacy_terms = self.privacy_terms();
        let entries: Vec<(String, uuid::Uuid)> = recent
            .iter()
            .take(10)
            .map(|c| {
                (
                    self.privacy_display_label(&c.label, &c.label, &privacy_terms),
                    c.id,
                )
            })
            .collect();

        // Signature covers label + id so a rename or reorder refreshes;
        // it hashes the redacted label, so a privacy toggle refreshes
        // the list too instead of leaving raw titles behind.
        let mut h = DefaultHasher::new();
        for (label, id) in &entries {
            label.hash(&mut h);
            id.hash(&mut h);
        }
        let sig = h.finish();
        if sig == self.jumplist_signature {
            return;
        }
        self.jumplist_signature = sig;

        let Ok(exe) = std::env::current_exe() else {
            return;
        };
        crate::jumplist::set_recent(&exe, crate::i18n::t("tray_recent_hosts"), &entries);
    }
}
