//! The two context menus (over a row, over the background) and the
//! properties sheet a row menu can open.
//!
//! Properties is here rather than with the file operations because
//! it only reads: one `stat` off-thread, straight into a dialog.

use super::*;

impl Oryxis {
    pub(super) fn handle_sidebar_files_menus(
        &mut self,
        message: SidebarFilesMessage,
    ) -> Task<Message> {
        match message {
            SidebarFilesMessage::ShowSidebarFilesRowMenu(path, is_dir) => {
                // Right-click semantics (Explorer / Finder, and the SFTP
                // pane's rule): right-clicking a row that is part of the
                // current selection KEEPS the selection, so the menu
                // actions apply to all of it; right-clicking an outside
                // row re-selects just that row so the menu can never
                // silently target a different set than the rows shown
                // highlighted.
                if let Some(pane) = self.active_pane_mut()
                    && !pane.files.selected.iter().any(|p| p == &path)
                {
                    pane.files.selected = vec![path.clone()];
                    pane.files.selection_anchor = Some(path.clone());
                }
                let anchor = self.keynav_take_menu_anchor();
                self.overlay = Some(crate::state::OverlayState {
                    content: crate::state::OverlayContent::SidebarFilesRow { path, is_dir },
                    x: anchor.0,
                    y: anchor.1,
                });
            }
            SidebarFilesMessage::SidebarFilesCopySelectionPaths => {
                // Bulk variant of Copy path: every selected path in the
                // browser, one per line (the SFTP pane's rule). The
                // menu is dismissed; the selection stays, copying is
                // not an action "on" the rows the way delete is.
                self.overlay = None;
                let Some(pane) = self.active_pane_mut() else {
                    return Task::none();
                };
                let paths: Vec<String> = super::selected_items(&pane.files)
                    .into_iter()
                    .map(|(p, _)| p)
                    .collect();
                if paths.is_empty() {
                    return Task::none();
                }
                return self.update(Message::CopyToClipboard(paths.join("\n")));
            }
            SidebarFilesMessage::ShowSidebarFilesBackgroundMenu => {
                // Directory-level menu for the current folder; only once
                // mounted (an unmounted browser has nothing to act on).
                let Some(dir) = self
                    .active_pane_mut()
                    .filter(|p| p.files.client.is_some() && !p.files.path.is_empty())
                    .map(|p| p.files.path.clone())
                else {
                    return Task::none();
                };
                let anchor = self.keynav_take_menu_anchor();
                self.overlay = Some(crate::state::OverlayState {
                    content: crate::state::OverlayContent::SidebarFilesBackground { dir },
                    x: anchor.0,
                    y: anchor.1,
                });
            }
            SidebarFilesMessage::SidebarFilesShowProperties(path, is_dir) => {
                self.overlay = None;
                let Some(pane) = self.active_pane_mut() else {
                    return Task::none();
                };
                // Properties applies chmod through the raw SFTP
                // channel; the local browser's menu never offers it
                // (issue #145).
                let Some(client) = pane.files.client.as_ref().and_then(|c| c.sftp().cloned())
                else {
                    return Task::none();
                };
                let stat_client = client.clone();
                let target = path.clone();
                return Task::perform(
                    async move { stat_client.stat(&target).await.map_err(|e| e.to_string()) },
                    move |result| match result {
                        Ok(stat) => {
                            let mode = stat.permissions.unwrap_or(0o644);
                            Message::Sftp(SftpMessage::SftpPropertiesLoaded(crate::state::PropertiesView {
                                // `side` is unused when a client override
                                // is present; Right is a stable filler.
                                side: crate::state::SftpPaneSide::Right,
                                client_override: Some(client.clone()),
                                from_sidebar: true,
                                path: path.clone(),
                                is_dir,
                                size: stat.size,
                                mtime: stat.mtime,
                                owner_uid: stat.uid,
                                owner_gid: stat.gid,
                                original_mode: mode,
                                bits: crate::state::PermBits::from_mode(mode),
                                mode_input: format!("{:03o}", mode & 0o777),
                                applying: false,
                                error: None,
                            }))
                        }
                        // One-shot op, not a listing: its failure surfaces
                        // as a toast (like download/upload) instead of a
                        // SidebarFilesError, whose un-bumped stamp would
                        // alias an in-flight listing's and paint an error
                        // over it / clear its loading flag.
                        Err(e) => Message::SidebarFiles(SidebarFilesMessage::SidebarFilesOpToast(e)),
                    },
                );
            }
            // The parent routed us here, so anything else is a
            // grouping mistake, not a runtime case.
            m => return crate::dispatch::unrouted(m),
        }
        Task::none()
    }
}
