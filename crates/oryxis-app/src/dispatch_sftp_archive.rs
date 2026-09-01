//! `Oryxis::handle_sftp_archive`: SFTP archive operations.
//!
//! Three families of messages:
//!
//! - Extract / compress on either pane. Remote panes shell out to the
//!   host's own tools (`tar` / `unzip` / `zip`, discovered by a
//!   once-per-mount probe) over an exec channel multiplexed on the live
//!   SSH connection, the `remove_dir_recursive` pattern. Local panes
//!   use in-process Rust codecs. Command synthesis and quoting live in
//!   `oryxis-archive` (aggressively tested there).
//! - Virtual zip browsing: the pane "enters" a `.zip` without
//!   extracting it. The central directory is read over SFTP ranged
//!   reads (KiBs of traffic regardless of archive size, zero remote
//!   tooling), the pane path takes the synthetic `<archive>!/<inner>`
//!   form, and the navigation handlers in `dispatch_sftp.rs` intercept
//!   it. Entries are mapped into the pane's normal row types so
//!   rendering / sorting / filtering / selection are untouched.
//! - Copy-out: extract an entry (or a whole folder) from the browsed
//!   archive into the OTHER pane, streaming only that entry's bytes.

#![allow(clippy::result_large_err)]

use std::sync::Arc;

use iced::Task;

use crate::app::{Message, Oryxis, SftpMessage};
use crate::sftp_helpers::{exec_checked, ExecTolerance};
use crate::state::{
    ArchiveDone, PaneState, SftpLogLevel, SftpPaneSide, ZipBrowse, ZipIndexedPayload,
};
use oryxis_archive::names::{self, ArchiveKind};
use oryxis_archive::ranged::RangedSource;
use oryxis_archive::remote::{self as remote_cmd, RemoteShell};
use oryxis_ssh::{RemoteRangedFile, SftpClient};

impl Oryxis {
    pub(crate) fn handle_sftp_archive(
        &mut self,
        message: SftpMessage,
    ) -> Result<Task<Message>, SftpMessage> {
        match message {
            SftpMessage::SftpToolsProbed(side, token, shell, tools) => {
                let pane = self.sftp.pane_mut(side);
                // A probe outliving its mount (the pane was remounted
                // to another host, or switched back to Local, while it
                // ran) is stale: drop it, the current mount spawned its
                // own probe.
                if !pane.archive_op_current(token) {
                    return Ok(Task::none());
                }
                // Stored even when empty: `Some` with no tools means
                // "probed, nothing there", which hides the remote
                // extract/compress menu items instead of re-probing.
                pane.archive_tools = Some((shell, tools));
            }
            SftpMessage::SftpZipOpen(side, path) => {
                self.sftp.row_menu = None;
                self.sftp.pane_mut(side).actions_open = false;
                {
                    let pane = self.sftp.pane(side);
                    if pane.archive_busy.is_some() || pane.zip.is_some() {
                        return Ok(Task::none());
                    }
                }
                let owner = self.current_sftp_owner();
                // Mount identity at spawn time: the completion drops
                // itself when the pane was remounted meanwhile.
                let token = self.sftp.pane(side).archive_op_token();
                let is_remote = self.sftp.pane(side).is_remote;
                let task = if is_remote {
                    let Some(client) = self.sftp.pane(side).client.clone() else {
                        return Ok(Task::none());
                    };
                    let p = path.clone();
                    Task::perform(
                        async move {
                            let file = client
                                .open_ranged(&p)
                                .await
                                .map_err(|e| e.to_string())?;
                            let file = Arc::new(file);
                            let rt = tokio::runtime::Handle::current();
                            let src = RemoteZipSource {
                                rt,
                                file: file.clone(),
                            };
                            let index = tokio::task::spawn_blocking(move || {
                                oryxis_archive::browse::read_index(src)
                            })
                            .await
                            .map_err(|e| e.to_string())?
                            .map_err(|e| e.to_string())?;
                            Ok(ZipIndexedPayload {
                                index: Arc::new(index),
                                remote_src: Some(file),
                            })
                        },
                        move |r| {
                            Message::sftp_owned(
                                owner,
                                SftpMessage::ZipIndexed(side, path.clone(), token, r),
                            )
                        },
                    )
                } else {
                    let p = std::path::PathBuf::from(&path);
                    Task::perform(
                        async move {
                            let index = tokio::task::spawn_blocking(move || {
                                let f = std::fs::File::open(&p).map_err(|e| e.to_string())?;
                                oryxis_archive::browse::read_index(f).map_err(|e| e.to_string())
                            })
                            .await
                            .map_err(|e| e.to_string())??;
                            Ok(ZipIndexedPayload {
                                index: Arc::new(index),
                                remote_src: None,
                            })
                        },
                        move |r| {
                            Message::sftp_owned(
                                owner,
                                SftpMessage::ZipIndexed(side, path.clone(), token, r),
                            )
                        },
                    )
                };
                self.sftp.pane_mut(side).archive_busy =
                    Some(crate::i18n::t("archive_reading").to_string());
                return Ok(task);
            }
            SftpMessage::ZipIndexed(side, archive_path, token, result) => {
                {
                    let pane = self.sftp.pane_mut(side);
                    // Only the op that set the busy flag may clear it:
                    // after a remount the mount reset already swept it
                    // and a NEWER op may own it now (a same-generation
                    // completion always owns it, the per-pane start
                    // guard forbids two concurrent ops on one pane).
                    if pane.archive_op_owns_busy(token) {
                        pane.archive_busy = None;
                    }
                    if !pane.archive_op_current(token) {
                        // The pane was remounted (or switched back to
                        // Local) while the index was read: installing
                        // it would put the OLD host's index and ranged
                        // handle over the new listing. Drop the result
                        // silently, returning the server-side handle
                        // when the old session still holds one (best-
                        // effort, mirroring SftpZipClose: dropping the
                        // Arc would also tear the channel down).
                        let close_task = match result
                            .ok()
                            .and_then(|p| p.remote_src)
                            .and_then(|s| Arc::try_unwrap(s).ok())
                        {
                            Some(file) => {
                                Task::future(async move { file.close().await }).discard()
                            }
                            None => Task::none(),
                        };
                        return Ok(close_task);
                    }
                }
                match result {
                    Err(e) => return Ok(Task::done(Message::Sftp(SftpMessage::SftpOpResult(side, e, true)))),
                    Ok(payload) => {
                        let name = base_name(&archive_path);
                        let skipped = payload.index.skipped_unsafe;
                        {
                            let pane = self.sftp.pane_mut(side);
                            pane.zip = Some(ZipBrowse {
                                archive_name: name.clone(),
                                inner: String::new(),
                                index: payload.index,
                                remote_src: payload.remote_src,
                                return_remote_path: pane.remote_path.clone(),
                                return_local_path: pane.local_path.clone(),
                                archive_path,
                            });
                            zip_relist(pane);
                        }
                        self.sftp.selected_rows.clear();
                        self.sftp.selection_anchor = None;
                        self.push_sftp_log(
                            SftpLogLevel::Info,
                            format!("{} {}", crate::i18n::t("archive_log_browsing"), name),
                        );
                        if skipped > 0 {
                            self.push_sftp_log(
                                SftpLogLevel::Error,
                                crate::i18n::t("archive_unsafe_skipped")
                                    .replacen("{n}", &skipped.to_string(), 1),
                            );
                        }
                    }
                }
            }
            SftpMessage::SftpZipNavigate(side, inner) => {
                let pane = self.sftp.pane_mut(side);
                if let Some(zip) = &mut pane.zip {
                    zip.inner = inner;
                    zip_relist(pane);
                    self.sftp.selected_rows.clear();
                    self.sftp.selection_anchor = None;
                }
            }
            SftpMessage::SftpZipClose(side) => {
                let pane = self.sftp.pane_mut(side);
                let Some(zip) = pane.zip.take() else {
                    return Ok(Task::none());
                };
                // Land the keyboard cursor back on the archive row.
                self.sftp.pending_focus = Some((
                    side,
                    crate::state::SftpPendingFocus::Path(zip.archive_path.clone()),
                ));
                // Return the server-side handle promptly (best-effort;
                // dropping the Arc would also tear the channel down).
                let close_task = match zip.remote_src.and_then(|s| Arc::try_unwrap(s).ok()) {
                    Some(file) => Task::future(async move { file.close().await }).discard(),
                    None => Task::none(),
                };
                let nav = if self.sftp.pane(side).is_remote {
                    Task::done(Message::Sftp(SftpMessage::SftpNavigateRemote(side, zip.return_remote_path)))
                } else {
                    Task::done(Message::Sftp(SftpMessage::SftpNavigateLocal(side, zip.return_local_path)))
                };
                return Ok(Task::batch([close_task, nav]));
            }
            SftpMessage::SftpZipCopyOut(side, inner, is_dir) => {
                self.sftp.row_menu = None;
                return Ok(self.start_zip_copy_out(side, inner, is_dir));
            }
            SftpMessage::SftpArchiveExtract(side, path) => {
                self.sftp.row_menu = None;
                return Ok(self.start_archive_extract(side, path));
            }
            SftpMessage::SftpArchiveCompress(side, kind, target) => {
                self.sftp.row_menu = None;
                return Ok(self.start_archive_compress(side, kind, target));
            }
            SftpMessage::ArchiveDone(done) => {
                let ArchiveDone {
                    side,
                    token,
                    busy_side,
                    busy_token,
                    result,
                } = done;
                {
                    // Clear exactly the pane this op marked busy (copy-
                    // out marks the SOURCE while `side` is the
                    // destination), never the other pane: a concurrent
                    // op there keeps its own guard. And only while the
                    // flag is still this op's: after a remount the
                    // mount reset swept it and a newer op may own it.
                    let busy_pane = self.sftp.pane_mut(busy_side);
                    if busy_pane.archive_op_owns_busy(busy_token) {
                        busy_pane.archive_busy = None;
                    }
                }
                if !self.sftp.pane(side).archive_op_current(token) {
                    // The affected pane was remounted (or switched back
                    // to Local) while the op ran: its outcome belongs
                    // to the previous mount. Show nothing, refresh
                    // nothing.
                    return Ok(Task::none());
                }
                match result {
                    Ok(label) => {
                        // Compressing or extracting a big tree is the
                        // same kind of wait a transfer is, and it ends
                        // the same way: with the user somewhere else.
                        // The answer is ignored here, unlike the toast
                        // sites: what this op leaves behind in-app is
                        // the log line (and, below, the pane's error),
                        // both of which PERSIST, so the notice adds to
                        // them instead of standing in for them.
                        self.notify_away(crate::i18n::t("transfer_notify_done"), &label);
                        self.push_sftp_log(SftpLogLevel::Ok, label);
                        // Refresh the affected pane so the new entries
                        // show up (skip if it meanwhile entered zip
                        // browse: its listing is virtual).
                        let pane = self.sftp.pane(side);
                        if pane.zip.is_some() {
                            return Ok(Task::none());
                        }
                        if pane.is_remote {
                            if pane.client.is_some() {
                                let path = pane.remote_path.clone();
                                return Ok(Task::done(Message::Sftp(SftpMessage::SftpNavigateRemote(side, path))));
                            }
                        } else {
                            self.refresh_sftp_local(side);
                        }
                    }
                    Err(e) => {
                        self.notify_away(crate::i18n::t("transfer_notify_failed"), &e);
                        return Ok(Task::done(Message::Sftp(SftpMessage::SftpOpResult(side, e, true))));
                    }
                }
            }
            other => return Err(other),
        }
        Ok(Task::none())
    }

    /// Uploads target the remote pane's CURRENT directory, which is a
    /// synthetic `<archive>!/...` path while zip-browsing: block them
    /// with an honest log line instead of letting the server fail on a
    /// directory that doesn't exist.
    pub(crate) fn sftp_upload_blocked_by_zip(&mut self, remote_side: SftpPaneSide) -> bool {
        if self.sftp.pane(remote_side).zip.is_some() {
            self.push_sftp_log(
                SftpLogLevel::Error,
                crate::i18n::t("archive_read_only").to_string(),
            );
            return true;
        }
        false
    }

    /// Probe the mounted host for archive tools, once per mount. Tries
    /// the POSIX probe first; when it finds nothing (a Windows OpenSSH
    /// shell answers "'sh' is not recognized" with an empty stdout),
    /// falls back to the Windows probe pair. An exec-less server (e.g.
    /// chrooted `internal-sftp`) simply yields an empty toolset, which
    /// keeps the remote archive actions hidden.
    pub(crate) fn spawn_archive_probe(&mut self, side: SftpPaneSide) -> Task<Message> {
        // Every mount funnels through here (`SftpMessage::HostMounted` calls it
        // right after resetting the pane for the new host), so this is
        // where the pane's mount generation is stamped: any archive
        // completion still in flight for the previous mount (zip
        // index, extract / compress / copy-out, this very probe) now
        // carries a stale token and drops itself on arrival.
        self.sftp.pane_mut(side).note_mounted();
        let token = self.sftp.pane(side).archive_op_token();
        let Some(client) = self.sftp.pane(side).client.clone() else {
            return Task::none();
        };
        Task::perform(
            async move {
                let run_all = |shell: RemoteShell| {
                    let client = client.clone();
                    async move {
                        let mut merged = String::new();
                        for cmd in remote_cmd::probe_commands(shell) {
                            if let Ok((_code, out, _err)) = client.exec(cmd).await {
                                merged.push_str(&out);
                                merged.push('\n');
                            }
                        }
                        remote_cmd::parse_probe_output(&merged)
                    }
                };
                let tools = run_all(RemoteShell::Posix).await;
                if tools.any() {
                    return (RemoteShell::Posix, tools);
                }
                let win = run_all(RemoteShell::Windows).await;
                if win.any() {
                    (RemoteShell::Windows, win)
                } else {
                    (RemoteShell::Posix, tools)
                }
            },
            move |(shell, tools)| Message::Sftp(SftpMessage::SftpToolsProbed(side, token, shell, tools)),
        )
    }

    /// Kick off "Extract here" for the archive at `path` (a row of the
    /// pane's CURRENT directory). The destination is a fresh directory
    /// named after the archive, deduplicated against the listing.
    fn start_archive_extract(
        &mut self,
        side: SftpPaneSide,
        path: String,
    ) -> Task<Message> {
        let name = base_name(&path);
        let Some(kind) = ArchiveKind::from_name(&name) else {
            return Task::none();
        };
        let pane = self.sftp.pane(side);
        if pane.archive_busy.is_some() || pane.zip.is_some() {
            return Task::none();
        }
        let existing = existing_names(pane);
        let dest_name = names::unique_name(
            names::archive_stem(&name),
            "",
            existing.iter().map(|s| s.as_str()),
        );
        let label = format!(
            "{} {} -> {}",
            crate::i18n::t("archive_log_extracted"),
            name,
            dest_name
        );
        let owner = self.current_sftp_owner();
        // The op changes and busies the same pane; capture its mount
        // identity so a post-remount completion drops itself.
        let token = pane.archive_op_token();
        let task = if pane.is_remote {
            let Some((shell, tools)) = pane.archive_tools else {
                return Task::done(Message::Sftp(SftpMessage::SftpOpResult(
                    side,
                    crate::i18n::t("archive_no_tools").to_string(),
                    true,
                )));
            };
            let Some(client) = pane.client.clone() else {
                return Task::none();
            };
            let dest_abs = join_remote(&pane.remote_path, &dest_name);
            let cmd = match remote_cmd::extract_command(shell, tools, kind, &path, &dest_abs) {
                Ok(c) => c,
                Err(e) => {
                    return Task::done(Message::Sftp(SftpMessage::SftpOpResult(side, e.to_string(), true)));
                }
            };
            // Only unzip's benign exit 1 is tolerated; tar failures stay
            // failures.
            let tolerate_warning = remote_cmd::extract_uses_unzip(shell, tools, kind);
            Task::perform(
                async move {
                    client
                        .create_dir(&dest_abs)
                        .await
                        .map_err(|e| e.to_string())?;
                    run_remote_archive_cmd(&client, &cmd, tolerate_warning).await
                },
                move |r| {
                    Message::sftp_owned(
                        owner,
                        SftpMessage::ArchiveDone(ArchiveDone {
                            side,
                            token,
                            busy_side: side,
                            busy_token: token,
                            result: r.map(|()| label.clone()),
                        }),
                    )
                },
            )
        } else {
            let archive = std::path::PathBuf::from(&path);
            let dest = pane.local_path.join(&dest_name);
            Task::perform(
                async move {
                    tokio::task::spawn_blocking(move || {
                        oryxis_archive::local::extract_archive(kind, &archive, &dest)
                            .map_err(|e| e.to_string())
                    })
                    .await
                    .map_err(|e| e.to_string())?
                },
                move |r| {
                    Message::sftp_owned(
                        owner,
                        SftpMessage::ArchiveDone(ArchiveDone {
                            side,
                            token,
                            busy_side: side,
                            busy_token: token,
                            result: r.map(|()| label.clone()),
                        }),
                    )
                },
            )
        };
        self.sftp.pane_mut(side).archive_busy =
            Some(crate::i18n::t("archive_extracting").to_string());
        task
    }

    /// Kick off "Compress to ..." for the clicked row (or the whole
    /// same-pane selection when the click landed inside it, the delete
    /// convention). Output lands in the pane's current directory under
    /// a deduplicated name.
    fn start_archive_compress(
        &mut self,
        side: SftpPaneSide,
        kind: ArchiveKind,
        target: String,
    ) -> Task<Message> {
        let pane = self.sftp.pane(side);
        if pane.archive_busy.is_some() || pane.zip.is_some() {
            return Task::none();
        }
        // Same-pane selection containing the clicked row -> compress
        // the selection; otherwise just the clicked row.
        let selected: Vec<String> = self
            .sftp
            .selected_rows
            .iter()
            .filter(|(s, _)| *s == side)
            .map(|(_, p)| p.clone())
            .collect();
        let items: Vec<String> = if selected.iter().any(|p| p == &target) {
            selected.iter().map(|p| base_name(p)).collect()
        } else {
            vec![base_name(&target)]
        };
        if items.is_empty() {
            return Task::none();
        }
        let existing = existing_names(pane);
        let base = if items.len() == 1 {
            let (stem, _ext) = split_name(&items[0]);
            stem.to_string()
        } else {
            // Multi-selection: name after the directory being packed.
            let cwd_name = if pane.is_remote {
                base_name(&pane.remote_path)
            } else {
                base_name(&pane.local_path.to_string_lossy())
            };
            if cwd_name.is_empty() || cwd_name == "/" {
                "archive".to_string()
            } else {
                cwd_name
            }
        };
        let out_name = names::unique_name(
            &base,
            kind.extension(),
            existing.iter().map(|s| s.as_str()),
        );
        let label = format!(
            "{} {} ({})",
            crate::i18n::t("archive_log_compressed"),
            out_name,
            items.len()
        );
        let owner = self.current_sftp_owner();
        // Same-pane op: one token covers both the refresh gate and the
        // busy-clear gate of the completion.
        let token = pane.archive_op_token();
        let task = if pane.is_remote {
            let Some((shell, tools)) = pane.archive_tools else {
                return Task::done(Message::Sftp(SftpMessage::SftpOpResult(
                    side,
                    crate::i18n::t("archive_no_tools").to_string(),
                    true,
                )));
            };
            let Some(client) = pane.client.clone() else {
                return Task::none();
            };
            let cmd = match remote_cmd::compress_command(
                shell,
                tools,
                kind,
                &pane.remote_path,
                &out_name,
                &items,
            ) {
                Ok(c) => c,
                Err(e) => {
                    return Task::done(Message::Sftp(SftpMessage::SftpOpResult(side, e.to_string(), true)));
                }
            };
            // Compression (zip/tar) reports real failure as any nonzero
            // code; only a clean exit is success.
            Task::perform(
                async move { run_remote_archive_cmd(&client, &cmd, false).await },
                move |r| {
                    Message::sftp_owned(
                        owner,
                        SftpMessage::ArchiveDone(ArchiveDone {
                            side,
                            token,
                            busy_side: side,
                            busy_token: token,
                            result: r.map(|()| label.clone()),
                        }),
                    )
                },
            )
        } else {
            let cwd = pane.local_path.clone();
            let out = cwd.join(&out_name);
            Task::perform(
                async move {
                    tokio::task::spawn_blocking(move || {
                        oryxis_archive::local::compress(kind, &cwd, &items, &out)
                            .map_err(|e| e.to_string())
                    })
                    .await
                    .map_err(|e| e.to_string())?
                },
                move |r| {
                    Message::sftp_owned(
                        owner,
                        SftpMessage::ArchiveDone(ArchiveDone {
                            side,
                            token,
                            busy_side: side,
                            busy_token: token,
                            result: r.map(|()| label.clone()),
                        }),
                    )
                },
            )
        };
        self.sftp.pane_mut(side).archive_busy =
            Some(crate::i18n::t("archive_compressing").to_string());
        task
    }

    /// Copy an entry (or folder) out of the browsed archive into the
    /// OTHER pane's current directory, streaming only the needed byte
    /// ranges. Remote destinations stage through a temp dir and upload.
    fn start_zip_copy_out(
        &mut self,
        side: SftpPaneSide,
        inner: String,
        is_dir: bool,
    ) -> Task<Message> {
        let other = other_side(side);
        {
            let src_pane = self.sftp.pane(side);
            let dst_pane = self.sftp.pane(other);
            if src_pane.zip.is_none()
                || src_pane.archive_busy.is_some()
                || dst_pane.zip.is_some()
                || (dst_pane.is_remote && dst_pane.client.is_none())
            {
                return Task::none();
            }
        }
        // Plan the job: entry indices + destination-relative paths.
        let (files, dirs, base) = {
            let pane = self.sftp.pane(side);
            let zip = pane.zip.as_ref().expect("guarded above");
            let raw_base = base_name(&inner);
            let dest_names = existing_names(self.sftp.pane(other));
            let (stem, suffix) = if is_dir {
                (raw_base.as_str(), String::new())
            } else {
                split_name(&raw_base)
            };
            let base =
                names::unique_name(stem, &suffix, dest_names.iter().map(|s| s.as_str()));
            if is_dir {
                let (fs, ds) = oryxis_archive::browse::entries_under(&zip.index, &inner);
                let files: Vec<(usize, String)> = fs
                    .into_iter()
                    .map(|(i, rel)| (i, format!("{base}/{rel}")))
                    .collect();
                let mut dirs = vec![base.clone()];
                dirs.extend(ds.into_iter().map(|d| format!("{base}/{d}")));
                (files, dirs, base)
            } else {
                let Some(entry) = zip
                    .index
                    .entries
                    .iter()
                    .find(|e| e.name == inner && !e.is_dir)
                else {
                    return Task::none();
                };
                (vec![(entry.index, base.clone())], Vec::new(), base)
            }
        };
        let source = {
            let pane = self.sftp.pane(side);
            let zip = pane.zip.as_ref().expect("guarded above");
            match &zip.remote_src {
                Some(src) => ZipSourceSpec::Remote(src.clone()),
                None => ZipSourceSpec::Local(std::path::PathBuf::from(&zip.archive_path)),
            }
        };
        let dest = {
            let pane = self.sftp.pane(other);
            if pane.is_remote {
                CopyDest::Remote(
                    pane.client.clone().expect("guarded above"),
                    pane.remote_path.clone(),
                )
            } else {
                CopyDest::Local(pane.local_path.clone())
            }
        };
        let label = format!("{} {}", crate::i18n::t("archive_log_copied"), base);
        let owner = self.current_sftp_owner();
        // Copying members OUT of an archive onto a host is an upload like
        // any other, so it obeys the same scratch-name choice.
        let temp_name = self.prefs.sftp_upload_temp_name;
        // Copy-out busies the SOURCE pane (the one browsing the
        // archive) while the DESTINATION pane receives the files, so
        // the completion carries a token per pane: the busy clear is
        // gated on the source's, the refresh on the destination's.
        let busy_token = self.sftp.pane(side).archive_op_token();
        let dest_token = self.sftp.pane(other).archive_op_token();
        self.sftp.pane_mut(side).archive_busy =
            Some(crate::i18n::t("archive_copying").to_string());
        Task::perform(
            async move { zip_copy_out_job(source, files, dirs, dest, temp_name).await },
            move |r| {
                Message::sftp_owned(
                    owner,
                    SftpMessage::ArchiveDone(ArchiveDone {
                        side: other,
                        token: dest_token,
                        busy_side: side,
                        busy_token,
                        result: r.map(|()| label.clone()),
                    }),
                )
            },
        )
    }
}

/// Refill a pane's entry rows from its zip-browse index (pure index
/// walk, no I/O) and stamp the synthetic path. Also used after inner
/// navigation.
pub(crate) fn zip_relist(pane: &mut PaneState) {
    let Some(zip) = &pane.zip else { return };
    let listing = oryxis_archive::browse::list_dir(&zip.index, &zip.inner);
    if pane.is_remote {
        let mut entries: Vec<oryxis_ssh::SftpEntry> = listing
            .into_iter()
            .map(|e| oryxis_ssh::SftpEntry {
                name: e.name,
                is_dir: e.is_dir,
                is_symlink: false,
                size: e.size,
                mtime: e.mtime_unix,
                permissions: None,
                uid: None,
                gid: None,
                // A zip central directory records no ownership at all.
                owner: None,
                group: None,
            })
            .collect();
        crate::sftp_helpers::sort_remote_entries(&mut entries, pane.sort);
        pane.remote_path = zip.synthetic_path();
        pane.remote_entries = entries;
        pane.remote_loading = false;
    } else {
        let mut entries: Vec<crate::state::LocalEntry> = listing
            .into_iter()
            .map(|e| crate::state::LocalEntry {
                name: e.name,
                is_dir: e.is_dir,
                size: e.size,
                modified: e
                    .mtime_unix
                    .map(|s| std::time::UNIX_EPOCH + std::time::Duration::from_secs(s.into())),
                mode: None,
                uid: None,
                gid: None,
            })
            .collect();
        crate::sftp_helpers::sort_local_entries(&mut entries, pane.sort);
        pane.local_path = std::path::PathBuf::from(zip.synthetic_path());
        pane.local_entries = entries;
    }
    pane.error = None;
    pane.list_scroll_y = 0.0;
}

pub(crate) fn other_side(side: SftpPaneSide) -> SftpPaneSide {
    match side {
        SftpPaneSide::Left => SftpPaneSide::Right,
        SftpPaneSide::Right => SftpPaneSide::Left,
    }
}

/// Last path component (both separators), used to turn listing paths
/// into bare names.
pub(crate) fn base_name(path: &str) -> String {
    path.rsplit(['/', '\\'])
        .find(|s| !s.is_empty())
        .unwrap_or(path)
        .to_string()
}

/// Split `photo.txt` into `("photo", ".txt")` so deduplication yields
/// `photo-1.txt` instead of `photo.txt-1`. Extensionless (and dotfile)
/// names keep the whole name as the stem.
fn split_name(name: &str) -> (&str, String) {
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => (stem, format!(".{ext}")),
        _ => (name, String::new()),
    }
}

fn join_remote(dir: &str, name: &str) -> String {
    let d = dir.trim_end_matches('/');
    if d.is_empty() {
        format!("/{name}")
    } else {
        format!("{d}/{name}")
    }
}

/// The pane's current entry names (post-listing), for output-name
/// deduplication.
fn existing_names(pane: &PaneState) -> Vec<String> {
    if pane.is_remote {
        pane.remote_entries.iter().map(|e| e.name.clone()).collect()
    } else {
        pane.local_entries.iter().map(|e| e.name.clone()).collect()
    }
}

/// Run a synthesized archive command on the exec channel, mapping the
/// exit status to a user-facing error via the shared
/// [`crate::sftp_helpers::exec_checked`]. `unzip` exits 1 for benign
/// warnings (e.g. trailing garbage) while still extracting, so 1 is
/// accepted only when the caller says this is an unzip extraction
/// (`tolerate_warning`). Keying that off the caller's operation, not a
/// substring of `cmd`, means a hostile file name containing "unzip "
/// can't turn a real tar failure into a false success.
async fn run_remote_archive_cmd(
    client: &SftpClient,
    cmd: &str,
    tolerate_warning: bool,
) -> Result<(), String> {
    let tolerance = if tolerate_warning {
        ExecTolerance::AcceptWarning
    } else {
        ExecTolerance::Strict
    };
    exec_checked(client, cmd, tolerance, |code| {
        format!("archive command exited with code {code}")
    })
    .await
    .map(|_| ())
}

/// Sync bridge: `RangedSource` over the async SFTP ranged-read handle.
/// Only ever used inside `spawn_blocking`, where `Handle::block_on` is
/// legal.
struct RemoteZipSource {
    rt: tokio::runtime::Handle,
    file: Arc<RemoteRangedFile>,
}

impl RangedSource for RemoteZipSource {
    fn len(&mut self) -> std::io::Result<u64> {
        Ok(self.file.len())
    }

    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> std::io::Result<usize> {
        let data = self
            .rt
            .block_on(self.file.read_at(offset, buf.len()))
            .map_err(std::io::Error::other)?;
        let n = data.len().min(buf.len());
        buf[..n].copy_from_slice(&data[..n]);
        Ok(n)
    }
}

enum ZipSourceSpec {
    Remote(Arc<RemoteRangedFile>),
    Local(std::path::PathBuf),
}

enum CopyDest {
    Remote(SftpClient, String),
    Local(std::path::PathBuf),
}

/// Turn a `/`-separated archive-relative path into a `PathBuf` under
/// `root`. Components are already sanitized by the index normalizer.
fn under(root: &std::path::Path, rel: &str) -> std::path::PathBuf {
    rel.split('/').fold(root.to_path_buf(), |p, c| p.join(c))
}

/// The copy-out worker: extract the planned entries (streaming from
/// ranged reads) into the destination. Remote destinations stage into a
/// temp dir, then upload and clean up.
async fn zip_copy_out_job(
    source: ZipSourceSpec,
    files: Vec<(usize, String)>,
    dirs: Vec<String>,
    dest: CopyDest,
    temp_name: bool,
) -> Result<(), String> {
    let (root, staged) = match &dest {
        CopyDest::Local(dir) => (dir.clone(), false),
        CopyDest::Remote(..) => (
            std::env::temp_dir().join(format!("oryxis-zipcopy-{}", uuid::Uuid::new_v4())),
            true,
        ),
    };
    let rt = tokio::runtime::Handle::current();
    let extract_root = root.clone();
    let extract_dirs = dirs.clone();
    let extract_files = files.clone();
    let extracted = tokio::task::spawn_blocking(move || -> Result<(), String> {
        let src: Box<dyn RangedSource> = match source {
            ZipSourceSpec::Remote(file) => Box::new(RemoteZipSource { rt, file }),
            ZipSourceSpec::Local(path) => {
                Box::new(std::fs::File::open(&path).map_err(|e| e.to_string())?)
            }
        };
        let mut reader =
            oryxis_archive::browse::ZipReader::open(src).map_err(|e| e.to_string())?;
        for d in &extract_dirs {
            std::fs::create_dir_all(under(&extract_root, d)).map_err(|e| e.to_string())?;
        }
        for (index, rel) in &extract_files {
            let target = under(&extract_root, rel);
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            let out = std::fs::File::create(&target).map_err(|e| e.to_string())?;
            reader
                .extract_to(*index, out, None)
                .map_err(|e| e.to_string())?;
        }
        Ok(())
    })
    .await
    .map_err(|e| e.to_string())?;
    if let Err(e) = extracted {
        if staged {
            let _ = std::fs::remove_dir_all(&root);
        }
        return Err(e);
    }
    if let CopyDest::Remote(client, dest_dir) = dest {
        let upload = async {
            // BTreeSet order guarantees parents before children.
            for d in &dirs {
                client
                    .create_dir(&join_remote(&dest_dir, d))
                    .await
                    .map_err(|e| e.to_string())?;
            }
            for (_, rel) in &files {
                crate::sftp_helpers::upload_one(
                    &client,
                    &under(&root, rel),
                    &join_remote(&dest_dir, rel),
                    temp_name,
                    None,
                )
                .await?;
            }
            Ok::<(), String>(())
        }
        .await;
        let _ = std::fs::remove_dir_all(&root);
        upload?;
    }
    Ok(())
}
