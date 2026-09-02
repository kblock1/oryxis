//! Creating, renaming and deleting entries: the inline editors and
//! the commits that reach the remote.
//!
//! Delete always goes through its confirm (`SidebarFilesDelete`
//! arms it, `SidebarFilesDeleteConfirmed` performs it); a trash
//! click must never be one gesture away from a recursive remove.

use super::*;

impl Oryxis {
    pub(super) fn handle_sidebar_files_entries(
        &mut self,
        message: SidebarFilesMessage,
    ) -> Task<Message> {
        match message {
            SidebarFilesMessage::SidebarFilesStartRename(path) => {
                self.overlay = None;
                let Some(pane) = self.active_pane_mut() else {
                    return Task::none();
                };
                let basename = files_basename(&path);
                pane.files.new_entry = None;
                pane.files.rename = Some((path, basename));
                return crate::widgets::focus_input(iced::widget::Id::new(
                    "sidebar-files-rename",
                ));
            }
            SidebarFilesMessage::SidebarFilesRenameInput(s) => {
                if let Some(pane) = self.active_pane_mut()
                    && let Some((_, input)) = pane.files.rename.as_mut()
                {
                    *input = s;
                }
            }
            SidebarFilesMessage::SidebarFilesRenameCommit => {
                let Some(pane) = self.active_pane_mut() else {
                    return Task::none();
                };
                let Some((original, input)) = pane.files.rename.take() else {
                    return Task::none();
                };
                let name = input.trim();
                // Same guard as the SFTP pane's rename: one plain path
                // component (rejects empty, ".", ".." and separators).
                if !crate::sftp_helpers::is_safe_remote_entry_name(name) {
                    return Task::none();
                }
                let parent = files_parent_dir(&original).unwrap_or_else(|| "/".to_string());
                // (windows-aware: `files_parent_dir` / `files_join`
                // detect the local browser's `C:\` paths, issue #145)
                let target = files_join(&parent, name);
                if target == original {
                    return Task::none();
                }
                let Some(client) = pane.files.client.clone() else {
                    return Task::none();
                };
                let list_path = pane.files.path.clone();
                let pane_id = pane.id;
                pane.files.loading = true;
                pane.files.error = None;
                let seq = pane.files.next_req();
                return op_then_list(
                    client.clone(),
                    list_path,
                    pane_id,
                    seq,
                    async move { client.rename(&original, &target).await },
                );
            }
            SidebarFilesMessage::SidebarFilesStartNewEntry(kind) => {
                self.overlay = None;
                let Some(pane) = self.active_pane_mut() else {
                    return Task::none();
                };
                if pane.files.client.is_none() {
                    return Task::none();
                }
                pane.files.rename = None;
                pane.files.new_entry = Some((kind, String::new()));
                return crate::widgets::focus_input(iced::widget::Id::new(
                    "sidebar-files-new",
                ));
            }
            SidebarFilesMessage::SidebarFilesNewEntryInput(s) => {
                if let Some(pane) = self.active_pane_mut()
                    && let Some((_, input)) = pane.files.new_entry.as_mut()
                {
                    *input = s;
                }
            }
            SidebarFilesMessage::SidebarFilesNewEntryCommit => {
                let Some(pane) = self.active_pane_mut() else {
                    return Task::none();
                };
                let Some((kind, input)) = pane.files.new_entry.take() else {
                    return Task::none();
                };
                let name = input.trim();
                // One plain path component (rejects empty, ".", ".." and
                // separators), same guard as the rename commit.
                if !crate::sftp_helpers::is_safe_remote_entry_name(name) {
                    return Task::none();
                }
                let Some(client) = pane.files.client.clone() else {
                    return Task::none();
                };
                let target = files_join(&pane.files.path, name);
                let list_path = pane.files.path.clone();
                let pane_id = pane.id;
                pane.files.loading = true;
                pane.files.error = None;
                let seq = pane.files.next_req();
                let exists_msg = crate::i18n::t("files_entry_exists")
                    .replacen("{name}", name, 1);
                return op_then_list(client.clone(), list_path, pane_id, seq, async move {
                    match kind {
                        crate::state::SftpEntryKind::Folder => client.create_dir(&target).await,
                        // Exclusive create: colliding with an existing name
                        // must error out, not silently truncate it to zero
                        // bytes (which a plain CREATE|TRUNCATE write does).
                        // The stat pre-check turns the server's opaque
                        // EXCL failure into a readable message; EXCL still
                        // closes the check-to-create race.
                        crate::state::SftpEntryKind::File => {
                            if client.stat(&target).await.is_ok() {
                                return Err(oryxis_ssh::SshError::Channel(exists_msg));
                            }
                            client.create_file_exclusive(&target).await
                        }
                    }
                });
            }
            SidebarFilesMessage::SidebarFilesDelete(path, is_dir) => {
                self.overlay = None;
                let name = files_basename(&path);
                // Shared destructive-confirm dialog (Enter confirms via
                // the modal keynav router).
                self.confirm_remove(
                    name,
                    Message::SidebarFiles(SidebarFilesMessage::SidebarFilesDeleteConfirmed(path, is_dir)),
                );
            }
            SidebarFilesMessage::SidebarFilesDeleteSelection(targets) => {
                // Bulk delete from a multi-selection: same shared confirm
                // dialog, with the SFTP modal's count-aware copy (folder /
                // file breakdown) instead of a single name.
                self.overlay = None;
                if targets.is_empty() {
                    return Task::none();
                }
                let owned: Vec<(&str, bool)> = targets
                    .iter()
                    .map(|(p, d)| (p.as_str(), *d))
                    .collect();
                let (title, body) = crate::sftp_helpers::delete_confirm_copy(&owned);
                drop(owned);
                self.error_dialog = Some(crate::state::ErrorDialog {
                    title,
                    body,
                    link: None,
                    action: Some(crate::state::ErrorDialogAction {
                        label: crate::i18n::t("delete").to_string(),
                        message: Box::new(Message::SidebarFiles(
                            SidebarFilesMessage::SidebarFilesDeleteConfirmedSelection(
                                targets,
                            ),
                        )),
                        danger: true,
                    }),
                });
            }
            SidebarFilesMessage::SidebarFilesDeleteConfirmed(path, is_dir) => {
                let Some(pane) = self.active_pane_mut() else {
                    return Task::none();
                };
                let Some(client) = pane.files.client.clone() else {
                    return Task::none();
                };
                let list_path = pane.files.path.clone();
                let pane_id = pane.id;
                pane.files.loading = true;
                pane.files.error = None;
                let seq = pane.files.next_req();
                return op_then_list(client.clone(), list_path, pane_id, seq, async move {
                    if is_dir {
                        client.remove_dir_recursive(&path).await
                    } else {
                        client.remove_file(&path).await
                    }
                });
            }
            SidebarFilesMessage::SidebarFilesDeleteConfirmedSelection(targets) => {
                let Some(pane) = self.active_pane_mut() else {
                    return Task::none();
                };
                let Some(client) = pane.files.client.clone() else {
                    return Task::none();
                };
                let list_path = pane.files.path.clone();
                let pane_id = pane.id;
                pane.files.loading = true;
                pane.files.error = None;
                let seq = pane.files.next_req();
                return bulk_delete_then_list(client, list_path, pane_id, seq, targets);
            }
            // The parent routed us here, so anything else is a
            // grouping mistake, not a runtime case.
            m => return crate::dispatch::unrouted(m),
        }
        Task::none()
    }
}
