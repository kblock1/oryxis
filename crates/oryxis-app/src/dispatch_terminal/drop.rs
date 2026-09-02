//! OS drag-and-drop onto the terminal (#106): buffering, routing, and
//! the SFTP half of the upload.
//!
//! `FileDropped` is a global window event that `subscription.rs` funnels
//! into `SftpMessage::SftpFileDropped`; the SFTP surface keeps first
//! claim on it, and this module is the fallback when that surface is not
//! visible (`sftp_surface_visible()` false, or no SFTP tab open at all).
//! A hybrid Files tab therefore keeps the full dual-pane drop behavior:
//! it IS the SFTP surface while shown.
//!
//! Routing, per target pane:
//!
//! | Transport      | Shell cwd (OSC 7)  | Files       | Folders        |
//! |----------------|--------------------|-------------|----------------|
//! | SSH            | exact              | SFTP to cwd | SFTP to cwd    |
//! | SSH            | unknown/heuristic  | ZMODEM      | toast, skipped |
//! | Telnet/Serial  | n/a                | ZMODEM      | toast, skipped |
//! | local shell    | n/a                | paste path  | paste path     |
//!
//! A host's `zmodem_drops` flag forces the ZMODEM row for SSH, whatever
//! the cwd story: the interactive shell runs inside a container there,
//! where SFTP (host-bound by construction) cannot follow it, and where
//! the host shell's OSC 133 busy signal froze at the handover, so the
//! typed-`rz` guard reads the alternate screen instead of it.
//!
//! The OSC 7 gate is strict on purpose: the title-parse cwd fallback is
//! a heuristic and often `~`-relative, and a guessed directory means
//! uploading to the wrong place. ZMODEM's `rz` lands in the shell's real
//! cwd without us knowing it, which is exactly why it stays the fallback
//! for files. Folders can't ride ZMODEM at all (the protocol streams
//! file contents, it cannot create directory trees).
//!
//! The ZMODEM half is intentionally thin here: stash the sources on the
//! pane, type `rz -y`, and let the existing detector flow
//! (`begin_zmodem_transfer`) consume them instead of opening the file
//! picker. The transfer only ever starts when the detector has SEEN the
//! remote receiver start, so the detect-timeout can never abort a
//! running transfer; it merely clears the stash when `rz` demonstrably
//! never ran (#106's fixed 1-second watchdog aborted every upload that
//! outlived it, and mislabeled the abort as "lrzsz not installed").

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use iced::Task;
use iced::futures::SinkExt;
use iced::futures::future::BoxFuture;
use uuid::Uuid;

use crate::app::{Message, Oryxis, TerminalMessage, ZmodemMessage};
use crate::state::{DropProgress, DropUploadPane, PromptState};

/// Debounce that coalesces a multi-file OS drop (delivered as one
/// `FileDropped` per file) into a single flush, mirroring the SFTP
/// panel's own burst window.
const DROP_FLUSH_DEBOUNCE_MS: u64 = 150;

/// How long the ZMODEM detector gets to see the remote `rz` start after
/// the app types `rz -y`. This bounds one command round trip (echo +
/// spawn), never any transfer, so a generous value costs nothing: the
/// only thing that happens at expiry is clearing the stash and telling
/// the user why.
const RZ_DETECT_TIMEOUT_SECS: u64 = 4;

impl Oryxis {
    /// Buffer one dropped path; the first file of the gesture arms the
    /// debounce flush. Called from the SFTP drop handler's fallback
    /// paths, so it only ever sees drops the SFTP surface declined.
    pub(crate) fn buffer_terminal_drop(&mut self, path: PathBuf) -> Task<Message> {
        let first = self.pending_terminal_drops.is_empty();
        self.pending_terminal_drops.push(path);
        if !first {
            return Task::none();
        }
        Task::perform(
            async {
                tokio::time::sleep(std::time::Duration::from_millis(DROP_FLUSH_DEBOUNCE_MS)).await;
            },
            |_| Message::Terminal(TerminalMessage::TerminalDropFlush),
        )
    }

    /// Route the buffered drop to the pane under the cursor. See the
    /// module docs for the routing table.
    pub(crate) fn handle_terminal_drop_flush(&mut self) -> Task<Message> {
        let paths = std::mem::take(&mut self.pending_terminal_drops);
        if paths.is_empty() {
            return Task::none();
        }
        // Only a visible terminal accepts drops: with the vault (or any
        // other view) on screen there is no pane under the cursor, and
        // uploading into a BACKGROUND tab is the #106 behavior this
        // replaces. The SFTP surfaces already claimed their drops
        // upstream. "Visible" mirrors `view_content`: a focused session
        // tab renders the terminal under ANY view tag (closing a chip,
        // Close Others or Reconnect focus a tab without touching
        // `active_view`), so the gate is the same expression the
        // keyboard / mouse / palette routers use, not a per-view check.
        let terminal_on_screen = self.active_view == crate::state::View::Terminal
            || self.active_tab.is_some();
        if !terminal_on_screen {
            return Task::none();
        }
        // The connect progress paints over the focused tab's panes
        // (`view_content` gates on the same `connecting_here`), so a
        // drop while it is up has no visible pane to land in; without
        // this check it would type into (or SFTP toward) a surface the
        // user cannot see.
        let connecting_here = self
            .connecting
            .as_ref()
            .is_some_and(|cp| Some(cp.tab_idx) == self.active_tab);
        if connecting_here {
            return Task::none();
        }
        let Some(tab_idx) = self.active_tab else {
            return Task::none();
        };
        let Some(tab) = self.tabs.get(tab_idx) else {
            return Task::none();
        };
        let tab_id = tab._id;
        // The pane whose last-drawn canvas rect contains the cursor,
        // falling back to the focused pane: `FileDropped` carries no
        // position, and some platforms stop delivering CursorMoved while
        // the OS owns the drag, so the last known position can be stale.
        // The fallback is never worse than what #106 shipped (it always
        // used the focused pane).
        // Hit-tested against the pane's WHOLE rect, header included: a
        // reported rect is the body only, so without this a drop on the
        // header strip would fall through to the focused pane instead
        // of the one it was aimed at.
        let headers = tab.panes_have_headers(self.prefs.pane_headers);
        let rects: Vec<(Uuid, iced::Rectangle)> = tab
            .pane_grid
            .panes
            .values()
            .map(|p| (p.id, crate::state::TerminalTab::pane_hit_bounds(p, headers)))
            .collect();
        let pane_id =
            pane_under(self.mouse_position, &rects).unwrap_or_else(|| tab.active().id);
        let Some(pane) = tab.pane_grid.panes.values().find(|p| p.id == pane_id) else {
            return Task::none();
        };

        // One transfer per pane at a time. The running one's card is on
        // screen, so the state is visible rather than silently dropped.
        if pane.zmodem.is_some() || pane.drop_upload.is_some() || !pane.pending_drop_sources.is_empty() {
            return Task::none();
        }

        // Broken symlinks and unreadable entries ride the FILE branch on
        // purpose: the transport's own open() error is the honest report,
        // where #106's `is_file()` partition silently misfiled them as
        // folders.
        let (dirs, files): (Vec<PathBuf>, Vec<PathBuf>) =
            paths.into_iter().partition(|p| p.is_dir());

        let Some(session) = pane.session.as_ref() else {
            // Local shell: paste the quoted paths, the convention every
            // terminal follows for drops. Riding the paste path keeps
            // bracketed paste and the paste guards.
            let line = typed_drop_line(&files, &dirs);
            if !line.is_empty() {
                self.paste_text_into_tab(tab_id, &line);
            }
            return Task::none();
        };

        // Sidebar Files hybrid: with the browser on screen, its current
        // directory is what the user is LOOKING at (manual navigation
        // included), and the mounted client is proof SFTP works, so it
        // outranks the shell cwd. Focused pane only: the sidebar renders
        // the FOCUSED pane's browser, so for any other pane of a split
        // that directory is not the one on screen.
        let sidebar_dir = (pane_id == tab.active().id
            && self.sidebar_tab_shown(crate::state::TerminalSidebarTab::Files)
            && pane.files.client.is_some()
            && !pane.files.path.is_empty())
        .then(|| pane.files.path.clone());

        // Per-host opt-out of the SFTP path: a host whose interactive
        // shell runs INSIDE a container (the startup command enters one)
        // sees its SFTP channel reach the host filesystem, not the
        // container's. The user's say-so is the connection flag; with it
        // set, drops fall through to the ZMODEM branch below, whose `rz`
        // runs where the shell runs and lands in the container's own
        // working directory. It outranks the sidebar directory too: that
        // browser rides the same host-bound SFTP.
        let zmodem_drops = pane
            .saved_conn_id()
            .and_then(|id| self.connections.iter().find(|c| c.id == id))
            .is_some_and(|c| c.zmodem_drops);

        // SSH with a visible browser directory or an exact shell cwd:
        // SFTP, files and folders alike, on a subsystem channel of the
        // live handle. Out-of-band: nothing is typed, so this is safe
        // even with a full-screen program up.
        if !zmodem_drops
            && let Some(ssh) = session.ssh()
            && let Some(dest) = sidebar_dir
                .or_else(|| pane.cwd_from_osc7.then(|| pane.cwd.clone()).flatten())
        {
            let ssh = Arc::clone(ssh);
            return self.begin_drop_sftp_upload(pane_id, ssh, dest, files, dirs);
        }

        // ZMODEM fallback (SSH without exact cwd, Telnet, serial).
        let mut toast: Option<&'static str> = None;
        if !dirs.is_empty() {
            // rz cannot create directory trees; say so instead of
            // vanishing (the files of a mixed drop still go).
            toast = Some("terminal_drop_folder_no_cwd");
        }
        if files.is_empty() {
            if let Some(key) = toast {
                self.set_toast(crate::i18n::t(key).to_string());
            }
            return Task::none();
        }
        // `rz -y` is a typed command: refuse while a command is known to
        // be running, or the line lands inside vim/less/htop. NoIntegration
        // stays permissive: without OSC 133 there is no busy signal, and
        // refusing would break every host without shell integration.
        //
        // A `zmodem_drops` host gets the alternate-screen test instead:
        // the flag says the interactive shell runs inside a container,
        // where the host shell's OSC 133 marks stop the moment the
        // startup command enters it, leaving `Busy` frozen at the
        // handover forever (the refusal fired from an idle container
        // prompt). The alternate screen is still live there — vim /
        // less / htop raise it wherever they run — so the full-screen
        // protection stays without the stale false positive.
        let busy = if zmodem_drops {
            pane.terminal
                .lock()
                .map(|t| t.is_alt_screen())
                .unwrap_or(false)
        } else {
            matches!(pane.prompt, PromptState::Busy)
        };
        if busy {
            self.set_toast(crate::i18n::t("terminal_drop_busy").to_string());
            return Task::none();
        }
        let wire = session.write_sender();
        if let Some(key) = toast {
            self.set_toast(crate::i18n::t(key).to_string());
        }
        if let Some(pane) = self.pane_by_id_mut(pane_id) {
            pane.pending_drop_sources = files;
        }
        // Straight to the transport's write sender, NOT
        // `write_input_to_tab`: broadcast input fans that out to every
        // pane in the tab, which would start rz on hosts nobody dropped
        // onto.
        let _ = wire.send(b"rz -y\r".to_vec());
        Task::perform(
            async {
                tokio::time::sleep(std::time::Duration::from_secs(RZ_DETECT_TIMEOUT_SECS)).await;
            },
            move |_| Message::Zmodem(ZmodemMessage::ZmodemDropRzTimeout(pane_id)),
        )
    }

    /// Start the SFTP half: pre-walk the sources (so the total is known
    /// up front), then upload sequentially on a fresh subsystem channel,
    /// streaming progress back as messages. Top-level folders get
    /// collision-free names via `unique_name_in_remote_dir` (a tree
    /// cannot merge safely); top-level files keep their own name, and a
    /// pre-existing destination pauses the upload to ask the user via
    /// the overwrite modal instead of clobbering or silently renaming.
    fn begin_drop_sftp_upload(
        &mut self,
        pane_id: Uuid,
        ssh: Arc<oryxis_ssh::SshSession>,
        dest_dir: String,
        files: Vec<PathBuf>,
        dirs: Vec<PathBuf>,
    ) -> Task<Message> {
        // Dropping files onto a terminal is still an upload, so it obeys
        // the same scratch-name choice as every other one.
        let temp_name = self.prefs.sftp_upload_temp_name;
        let abort = Arc::new(AtomicBool::new(false));
        if let Some(pane) = self.pane_by_id_mut(pane_id) {
            pane.drop_upload = Some(DropUploadPane {
                file_name: None,
                batch: None,
                transferred: 0,
                total: None,
                abort: Arc::clone(&abort),
                dest_dir: dest_dir.clone(),
                paused: None,
            });
        } else {
            return Task::none();
        }

        let stream = iced::stream::channel::<Message>(
            64,
            move |out: iced::futures::channel::mpsc::Sender<Message>| async move {
                // Every exit path reports exactly one terminal event;
                // the handler clears the card and toasts from it.
                let send = |p: DropProgress| -> BoxFuture<'static, bool> {
                    let msg = Message::Terminal(TerminalMessage::TerminalDropProgress(pane_id, p));
                    let mut out = out.clone();
                    Box::pin(async move { out.send(msg).await.is_ok() })
                };

                let client = match ssh.open_sftp().await {
                    Ok(c) => c,
                    Err(e) => {
                        send(DropProgress::Failed(e.to_string())).await;
                        return;
                    }
                };

                // Top-level plan: folders keep a non-colliding remote name
                // (a tree cannot merge safely), files keep their own so a
                // pre-existing destination asks the user instead of being
                // silently renamed or overwritten. The run loop below does
                // that per-file conflict check.
                let mut plans: Vec<(String, Vec<crate::state::TransferItem>)> = Vec::new();
                let mut total: u64 = 0;
                for (src, is_dir) in files
                    .iter()
                    .map(|f| (f.clone(), false))
                    .chain(dirs.iter().map(|d| (d.clone(), true)))
                {
                    let Some(base) = src.file_name().map(|s| s.to_string_lossy().into_owned())
                    else {
                        continue;
                    };
                    if is_dir {
                        let unique = match crate::sftp_helpers::unique_name_in_remote_dir(
                            &client, &dest_dir, &base,
                        )
                        .await
                        {
                            Ok(u) => u,
                            Err(e) => {
                                send(DropProgress::Failed(e)).await;
                                return;
                            }
                        };
                        let dst = crate::sftp_helpers::remote_join(&dest_dir, &unique);
                        let name = dst.rsplit('/').next().unwrap_or(&dst).to_string();
                        let mut items = Vec::new();
                        items.push(crate::state::TransferItem {
                            src: src.to_string_lossy().into_owned(),
                            dst: dst.clone(),
                            is_dir: true,
                            size: None,
                        });
                        let mut queue = std::collections::VecDeque::new();
                        if let Err(e) =
                            crate::sftp_helpers::walk_local_for_upload(&src, &dst, &mut queue)
                        {
                            send(DropProgress::Failed(e)).await;
                            return;
                        }
                        total += queue.iter().filter_map(|i| i.size).sum::<u64>();
                        items.extend(queue);
                        plans.push((name, items));
                    } else {
                        let dst = crate::sftp_helpers::remote_join(&dest_dir, &base);
                        let size = tokio::fs::metadata(&src).await.map(|m| m.len()).ok();
                        total += size.unwrap_or(0);
                        plans.push((
                            base,
                            vec![crate::state::TransferItem {
                                src: src.to_string_lossy().into_owned(),
                                dst,
                                is_dir: false,
                                size,
                            }],
                        ));
                    }
                }
                if plans.is_empty() {
                    send(DropProgress::Done).await;
                    return;
                }
                if !send(DropProgress::Plan { total }).await {
                    return;
                }

                let of = plans.len();
                run_drop_upload_plans(
                    &send,
                    &client,
                    temp_name,
                    abort,
                    pane_id,
                    plans,
                    0,
                    of,
                    0,
                    dest_dir,
                    None,
                )
                .await;
            },
        );
        Task::stream(stream)
    }

    /// Apply the user's overwrite answer to a paused drop upload and
    /// resume it from where it stopped. Called by the SFTP conflict
    /// handler when the modal's prompt carried `drop_upload_pane`.
    pub(crate) fn resolve_terminal_drop_conflict(
        &mut self,
        pane_id: Uuid,
        action: crate::state::OverwriteAction,
        apply_to_all: bool,
    ) -> Task<Message> {
        let Some(pane) = self.pane_by_id_mut(pane_id) else {
            return Task::none();
        };
        let Some(up) = pane.drop_upload.as_mut() else {
            return Task::none();
        };
        let Some(paused) = up.paused.take() else {
            return Task::none();
        };
        let abort = up.abort.clone();
        let Some(ssh) = pane.session.as_ref().and_then(|t| t.ssh()) else {
            return Task::none();
        };
        let ssh = Arc::clone(ssh);
        let crate::state::DropUploadPaused {
            mut plans,
            completed,
            index,
            of,
            dest_dir,
            temp_name,
        } = paused;
        let entry_start = index;
        // Cancel with "apply to all" drops the rest of the batch, the
        // panel's own sticky-cancel semantics (`handle_sftp_conflict`
        // clears the queue): the user asked to stop the drop, not to
        // keep uploading around every conflict. One terminal event
        // still clears the card and toasts.
        if apply_to_all && matches!(action, crate::state::OverwriteAction::Cancel) {
            return Task::done(Message::Terminal(TerminalMessage::TerminalDropProgress(
                pane_id,
                DropProgress::Cancelled,
            )));
        }
        // Sticky answer when the user ticked "apply to all": every later
        // conflict applies it instead of asking again. Resume is
        // deliberately never sticky, same rule as the panel handler: the
        // engine can check that THIS destination's tail belongs to THIS
        // source, which says nothing about the next pair, and a sticky
        // "continue" would be guessing at whether to splice two files it
        // has not seen.
        let sticky = (apply_to_all
            && !matches!(action, crate::state::OverwriteAction::Resume))
        .then_some(action);
        // The first remaining item is the conflicted file; pop it so the
        // answer can be applied before the loop resumes.
        let conflict = plans
            .first_mut()
            .and_then(|(_, items)| if items.is_empty() { None } else { Some(items.remove(0)) });

        let stream = iced::stream::channel::<Message>(
            64,
            move |out: iced::futures::channel::mpsc::Sender<Message>| async move {
                let send = |p: DropProgress| -> BoxFuture<'static, bool> {
                    let msg = Message::Terminal(TerminalMessage::TerminalDropProgress(pane_id, p));
                    let mut out = out.clone();
                    Box::pin(async move { out.send(msg).await.is_ok() })
                };

                // Cancelled via the overlay while the modal was up:
                // honor it instead of starting the paused upload.
                if abort.load(Ordering::Relaxed) {
                    send(DropProgress::Cancelled).await;
                    return;
                }

                let client = match ssh.open_sftp().await {
                    Ok(c) => c,
                    Err(e) => {
                        send(DropProgress::Failed(e.to_string())).await;
                        return;
                    }
                };

                let mut completed = completed;
                if let Some(item) = conflict {
                    if matches!(action, crate::state::OverwriteAction::Cancel) {
                        // Skipped, not transferred: count its planned size
                        // anyway so the bar still sums to the total.
                        completed += item.size.unwrap_or(0);
                    } else {
                        let counter = Arc::new(AtomicU64::new(0));
                        match watched_apply_overwrite(
                            &send, &client, &item, action, temp_name, &abort, completed, &counter,
                        )
                        .await
                        {
                            Ok(WatchedApply::Finished) => {}
                            Ok(WatchedApply::Aborted) => {
                                send(DropProgress::Cancelled).await;
                                return;
                            }
                            Ok(WatchedApply::ChannelClosed) => return,
                            Err(e) => {
                                send(DropProgress::Failed(e)).await;
                                return;
                            }
                        }
                        // Replace-if-different may skip (equal sizes) and
                        // Resume moves fewer bytes than the planned size:
                        // count the larger of planned and moved so the bar
                        // still sums to the total.
                        completed += item
                            .size
                            .unwrap_or(0)
                            .max(counter.load(Ordering::Relaxed));
                    }
                    if !send(DropProgress::Advanced { transferred: completed }).await {
                        return;
                    }
                }
                run_drop_upload_plans(
                    &send,
                    &client,
                    temp_name,
                    abort,
                    pane_id,
                    plans,
                    completed,
                    of,
                    entry_start,
                    dest_dir,
                    sticky,
                )
                .await;
            },
        );
        Task::stream(stream)
    }

    /// Apply a streamed drop-upload event to the pane's card; terminal
    /// events clear it and toast the outcome.
    pub(crate) fn handle_terminal_drop_progress(
        &mut self,
        pane_id: Uuid,
        progress: DropProgress,
    ) -> Task<Message> {
        let mut toast: Option<String> = None;
        let mut refresh: Option<Task<Message>> = None;
        // Title + body for the OS notice on a terminal event, taken
        // while the card is still here (the arms below consume it).
        let mut finished: Option<(String, String)> = None;
        // A `Conflict` parks the paused context on the pane and raises
        // the overwrite modal; the modal write happens after the pane
        // borrow below ends.
        let mut park_prompt: Option<crate::state::OverwritePrompt> = None;
        if let Some(pane) = self.pane_by_id_mut(pane_id) {
            let file_name = pane
                .drop_upload
                .as_ref()
                .and_then(|up| up.file_name.clone())
                .unwrap_or_default();
            match progress {
                DropProgress::Plan { total } => {
                    if let Some(up) = pane.drop_upload.as_mut() {
                        up.total = Some(total);
                    }
                }
                DropProgress::Entry { name, index, of } => {
                    if let Some(up) = pane.drop_upload.as_mut() {
                        up.file_name = Some(name);
                        // A single-entry drop needs no "(1/1)" noise,
                        // matching the ZMODEM card's single-file rule.
                        up.batch = (of > 1).then_some((index, of));
                    }
                }
                DropProgress::Advanced { transferred } => {
                    if let Some(up) = pane.drop_upload.as_mut() {
                        up.transferred = transferred;
                    }
                }
                DropProgress::Conflict { prompt, item, paused } => {
                    if let Some(up) = pane.drop_upload.as_mut() {
                        up.paused = Some(paused);
                        up.file_name = Some(crate::sftp_helpers::transfer_item_label(&item));
                    }
                    park_prompt = Some(*prompt);
                }
                DropProgress::Done => {
                    let up = pane.drop_upload.take();
                    toast = Some(crate::i18n::t("transfer_notify_done").to_string());
                    finished = Some((
                        crate::i18n::t("transfer_notify_done").to_string(),
                        file_name,
                    ));
                    // Sidebar Files showing the directory we just landed
                    // in: re-list it so the new entries appear without a
                    // manual refresh. Pinned to this pane's browser (not
                    // `SidebarFilesRefresh`, which acts on the ACTIVE
                    // pane: focus can have moved during a long upload).
                    if let Some(up) = up
                        && let Some(client) = pane.files.client.clone()
                        && pane.files.path == up.dest_dir
                    {
                        pane.files.loading = true;
                        let seq = pane.files.next_req();
                        refresh = Some(crate::dispatch_sidebar_files::list_dir_task(
                            client,
                            up.dest_dir,
                            pane_id,
                            seq,
                        ));
                    }
                }
                DropProgress::Cancelled => {
                    pane.drop_upload = None;
                    toast = Some(crate::i18n::t("transfer_notify_cancelled").to_string());
                    finished = Some((
                        crate::i18n::t("transfer_notify_cancelled").to_string(),
                        file_name,
                    ));
                }
                DropProgress::Failed(e) => {
                    pane.drop_upload = None;
                    toast = Some(format!("{}: {e}", crate::i18n::t("transfer_notify_failed")));
                    finished = Some((crate::i18n::t("transfer_notify_failed").to_string(), e));
                }
            }
        }
        // A destination conflict paused the drop upload: raise the
        // overwrite modal. The pane keeps the paused context; answering
        // the modal resumes the upload through
        // `resolve_terminal_drop_conflict`. Last-writer-wins like the
        // queue runners' own raise site (`SftpTransferConflict`): a
        // prompt already up for another surface would be replaced here
        // and its transfer left parked. Both raisers share that
        // limitation; lifting it takes a prompt queue across surfaces.
        if let Some(prompt) = park_prompt {
            self.sftp.overwrite_prompt = Some(prompt);
        }
        // Dropping files onto the terminal is an upload like any other,
        // and the one most likely to be left running: the user dropped
        // a folder and went back to what they were doing. Away from the
        // window it gets the OS notice, and the toast stays for the
        // times that notice wasn't shown.
        let notified = finished
            .as_ref()
            .is_some_and(|(title, body)| self.notify_away(title, body));
        if let Some(t) = toast
            && !notified
        {
            self.set_toast(t);
        }
        refresh.unwrap_or_else(Task::none)
    }
}

/// Drive the upload loop shared by the initial drop
/// (`begin_drop_sftp_upload`) and a paused-then-resumed one
/// (`resolve_terminal_drop_conflict`).
///
/// Every top-level FILE checks its destination first: a pre-existing file
/// with no sticky default pauses the whole drop (emitting `Conflict`,
/// which the update loop turns into the overwrite modal) instead of
/// overwriting silently. Folder plans upload under a fresh collision-free
/// root, so their children skip the check entirely; the directories
/// themselves merge tolerantly, mirroring the panel uploader.
#[allow(clippy::too_many_arguments)]
async fn run_drop_upload_plans(
    send: &(dyn Fn(DropProgress) -> BoxFuture<'static, bool> + Send + Sync),
    client: &oryxis_ssh::SftpClient,
    temp_name: bool,
    abort: Arc<AtomicBool>,
    pane_id: Uuid,
    plans: Vec<(String, Vec<crate::state::TransferItem>)>,
    mut completed: u64,
    of: usize,
    entry_start: usize,
    dest_dir: String,
    overwrite_default: Option<crate::state::OverwriteAction>,
) {
    for (plan_idx, (name, plan_items)) in plans.iter().enumerate() {
        let index = entry_start + plan_idx + 1;
        if !send(DropProgress::Entry {
            name: name.clone(),
            index,
            of,
        })
        .await
        {
            return;
        }
        let item_count = plan_items.len();
        // A folder plan uploads under a fresh collision-free root
        // (`unique_name_in_remote_dir`), so its children cannot conflict
        // by construction; only top-level file plans probe the
        // destination. That keeps the cost at one `list_dir` per dropped
        // file, never one per child of a dropped tree.
        let checks_conflict = plan_items.first().is_some_and(|i| !i.is_dir);
        for item_idx in 0..item_count {
            let item = &plan_items[item_idx];
            if abort.load(Ordering::Relaxed) {
                send(DropProgress::Cancelled).await;
                return;
            }
            if item.is_dir {
                // Merge-tolerant like the panel's uploader: a racing mkdir
                // is fine, anything else poisons every child below it, so
                // surface it now.
                if let Err(e) = client.create_dir(&item.dst).await
                    && client.list_dir(&item.dst).await.is_err()
                {
                    send(DropProgress::Failed(e.to_string())).await;
                    return;
                }
                continue;
            }
            // Conflict gate: a pre-existing destination asks instead of
            // silently overwriting (or silently renaming, the old drop
            // behavior). Mirrors the panel queue's per-item check.
            if checks_conflict {
                let parent = crate::sftp_helpers::parent_path(&item.dst);
                let basename = item
                    .dst
                    .rsplit('/')
                    .find(|s| !s.is_empty())
                    .unwrap_or(&item.dst)
                    .to_string();
                let entries = match client.list_dir(&parent).await {
                    Ok(e) => e,
                    Err(e) => {
                        send(DropProgress::Failed(e.to_string())).await;
                        return;
                    }
                };
                if let Some(existing) = entries.iter().find(|e| e.name == basename) {
                    if let Some(action) = overwrite_default {
                        let counter = Arc::new(AtomicU64::new(0));
                        match watched_apply_overwrite(
                            send, client, item, action, temp_name, &abort, completed, &counter,
                        )
                        .await
                        {
                            Ok(WatchedApply::Finished) => {}
                            Ok(WatchedApply::Aborted) => {
                                send(DropProgress::Cancelled).await;
                                return;
                            }
                            Ok(WatchedApply::ChannelClosed) => return,
                            Err(e) => {
                                send(DropProgress::Failed(e)).await;
                                return;
                            }
                        }
                        // Same accounting as the resolve handler: count
                        // the larger of planned and moved so a skipping
                        // replace-if-different still advances the bar.
                        completed += item
                            .size
                            .unwrap_or(0)
                            .max(counter.load(Ordering::Relaxed));
                        if !send(DropProgress::Advanced { transferred: completed }).await {
                            return;
                        }
                        continue;
                    }
                    let src_size = tokio::fs::metadata(&item.src)
                        .await
                        .map(|m| m.len())
                        .unwrap_or(0);
                    let prompt = crate::state::OverwritePrompt {
                        dst_dir: parent,
                        basename,
                        src_size,
                        dst_size: existing.size,
                        direction: crate::state::OverwriteDirection::Upload,
                        multi: of > 1,
                        apply_to_all: false,
                        owner: None,
                        drop_upload_pane: Some(pane_id),
                    };
                    // Everything still to do, this conflicted item first, so
                    // the resolve handler can apply the answer and continue.
                    let mut rest: Vec<(String, Vec<crate::state::TransferItem>)> = Vec::new();
                    rest.push((name.clone(), plan_items[item_idx..].to_vec()));
                    for (pname, pitems) in plans.iter().skip(plan_idx + 1) {
                        rest.push((pname.clone(), pitems.clone()));
                    }
                    send(DropProgress::Conflict {
                        prompt: Box::new(prompt),
                        item: item.clone(),
                        paused: crate::state::DropUploadPaused {
                            plans: rest,
                            completed,
                            // 0-based global entry position; the resume uses
                            // it as its `entry_start` so the displayed
                            // `(k, n)` batch position does not advance twice.
                            index: entry_start + plan_idx,
                            of,
                            dest_dir: dest_dir.clone(),
                            temp_name,
                        },
                    })
                    .await;
                    return;
                }
            }
            // No conflict: upload with progress ticks.
            let counter = Arc::new(AtomicU64::new(0));
            let src = PathBuf::from(&item.src);
            let mut up = Box::pin(crate::sftp_helpers::upload_one(
                client,
                &src,
                &item.dst,
                temp_name,
                Some(Arc::clone(&counter)),
            ));
            let mut tick = tokio::time::interval(std::time::Duration::from_millis(120));
            let finished = loop {
                tokio::select! {
                    r = &mut up => break r,
                    _ = tick.tick() => {
                        if abort.load(Ordering::Relaxed) {
                            // Kill the in-flight write and sweep the
                            // partial so a cancel never masquerades as a
                            // complete file. Which path holds the bytes
                            // depends on the scratch-name setting: with it
                            // on they went to `<dst>.oryxis-part`, so
                            // removing `dst` would miss the scratch AND
                            // could delete a pre-existing real file this
                            // upload never touched.
                            drop(up);
                            if temp_name {
                                client.discard_upload_scratch(&item.dst).await;
                            } else {
                                let _ = client.remove_file(&item.dst).await;
                            }
                            send(DropProgress::Cancelled).await;
                            return;
                        }
                        if !send(DropProgress::Advanced {
                            transferred: completed + counter.load(Ordering::Relaxed),
                        })
                        .await
                        {
                            return;
                        }
                    }
                }
            };
            if let Err(e) = finished {
                send(DropProgress::Failed(format!("{}: {e}", item.dst))).await;
                return;
            }
            completed += item.size.unwrap_or_else(|| counter.load(Ordering::Relaxed));
            if !send(DropProgress::Advanced { transferred: completed }).await {
                return;
            }
        }
    }
    send(DropProgress::Done).await;
}

/// Outcome of `watched_apply_overwrite`. `ChannelClosed` means a progress
/// send failed: the UI is gone, so the caller just ends the stream
/// without another event.
enum WatchedApply {
    Finished,
    Aborted,
    ChannelClosed,
}

/// Run an overwrite apply under the same 120 ms watch loop as a plain
/// upload, so the card keeps advancing and the overlay cancel is not
/// deaf for the whole file. Mid-flight cancel only sweeps where the
/// sweep is provably safe; everything else cancels at the next file
/// boundary instead (the outer loop's per-item abort check), which
/// never leaves a spliced resume or an orphan half-duplicate behind.
#[allow(clippy::too_many_arguments)]
async fn watched_apply_overwrite(
    send: &(dyn Fn(DropProgress) -> BoxFuture<'static, bool> + Send + Sync),
    client: &oryxis_ssh::SftpClient,
    item: &crate::state::TransferItem,
    action: crate::state::OverwriteAction,
    temp_name: bool,
    abort: &Arc<AtomicBool>,
    completed: u64,
    counter: &Arc<AtomicU64>,
) -> Result<WatchedApply, String> {
    let sweepable = if temp_name {
        // Scratch mode: the bytes went to `<dst>.oryxis-part`, so the
        // sweep can only ever remove our own partial, never the file
        // being replaced.
        matches!(
            action,
            crate::state::OverwriteAction::Replace
                | crate::state::OverwriteAction::ReplaceIfDifferent
        )
    } else {
        // Direct mode truncates `dst` on open, so a cancelled Replace
        // must sweep the damaged file (the user chose to forfeit it).
        // Replace-if-different may finish WITHOUT touching `dst` at all
        // (equal sizes): sweeping could delete the intact file the
        // action just decided to keep, so it is boundary-only here.
        // Resume appends into the file the user chose to continue and
        // Duplicate writes under a name minted inside the apply, so
        // neither has anything addressable to sweep in either mode.
        matches!(action, crate::state::OverwriteAction::Replace)
    };
    let mut apply = Box::pin(crate::sftp_helpers::apply_overwrite_for_item(
        client.clone(),
        item.clone(),
        action,
        temp_name,
        Some(Arc::clone(counter)),
    ));
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(120));
    let finished = loop {
        tokio::select! {
            r = &mut apply => break r,
            _ = tick.tick() => {
                if sweepable && abort.load(Ordering::Relaxed) {
                    // Same sweep rules as the plain upload path: which
                    // side holds the bytes depends on the scratch-name
                    // setting.
                    drop(apply);
                    if temp_name {
                        client.discard_upload_scratch(&item.dst).await;
                    } else {
                        let _ = client.remove_file(&item.dst).await;
                    }
                    return Ok(WatchedApply::Aborted);
                }
                if !send(DropProgress::Advanced {
                    transferred: completed + counter.load(Ordering::Relaxed),
                })
                .await
                {
                    return Ok(WatchedApply::ChannelClosed);
                }
            }
        }
    };
    finished.map(|()| WatchedApply::Finished)
}

/// The pane whose last-drawn rect contains `point`. `None` when the
/// point misses every pane (stale cursor position, drop outside the
/// grid), which the caller resolves to the focused pane.
fn pane_under(point: iced::Point, rects: &[(Uuid, iced::Rectangle)]) -> Option<Uuid> {
    rects
        .iter()
        .find(|(_, r)| r.contains(point))
        .map(|(id, _)| *id)
}

/// The line a local-shell drop pastes: every path shell-quoted, space
/// separated, with a trailing space so the user can keep typing. Paths
/// come from the OS, so quoting here is convenience (spaces, quotes),
/// not a security boundary; a path whose name defeats quoting (a line
/// break, on POSIX) is skipped rather than typed broken.
fn typed_drop_line(files: &[PathBuf], dirs: &[PathBuf]) -> String {
    let mut line = String::new();
    for p in files.iter().chain(dirs.iter()) {
        let raw = p.display().to_string();
        #[cfg(windows)]
        let quoted = if raw.contains(' ') {
            // Windows paths cannot contain `"`, so plain wrapping is
            // exact for any real path.
            Some(format!("\"{raw}\""))
        } else {
            Some(raw)
        };
        #[cfg(not(windows))]
        let quoted = if raw
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '-' | '_' | '~' | '+'))
        {
            Some(raw)
        } else {
            oryxis_archive::quote::sh_quote(&raw).ok()
        };
        if let Some(q) = quoted {
            line.push_str(&q);
            line.push(' ');
        }
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: f32, y: f32, w: f32, h: f32) -> iced::Rectangle {
        iced::Rectangle::new(iced::Point::new(x, y), iced::Size::new(w, h))
    }

    #[test]
    fn pane_under_picks_the_containing_rect() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let rects = vec![(a, rect(0.0, 0.0, 100.0, 100.0)), (b, rect(100.0, 0.0, 100.0, 100.0))];
        assert_eq!(pane_under(iced::Point::new(50.0, 50.0), &rects), Some(a));
        assert_eq!(pane_under(iced::Point::new(150.0, 50.0), &rects), Some(b));
        // A miss (stale cursor, drop on the chrome) resolves to None so
        // the caller falls back to the focused pane.
        assert_eq!(pane_under(iced::Point::new(250.0, 50.0), &rects), None);
    }

    #[cfg(not(windows))]
    #[test]
    fn typed_drop_line_quotes_what_needs_it() {
        let plain = PathBuf::from("/tmp/plain-1.txt");
        let spaced = PathBuf::from("/tmp/with space.txt");
        let quoted = PathBuf::from("/tmp/it's.txt");
        let line = typed_drop_line(&[plain, spaced, quoted], &[]);
        assert_eq!(
            line,
            "/tmp/plain-1.txt '/tmp/with space.txt' '/tmp/it'\\''s.txt' "
        );
        // A newline defeats POSIX single quotes in csh-family shells;
        // the path is skipped, never typed broken.
        let broken = PathBuf::from("/tmp/a\nb");
        assert_eq!(typed_drop_line(&[broken], &[]), "");
    }
}
