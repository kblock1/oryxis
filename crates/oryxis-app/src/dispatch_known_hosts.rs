//! `Oryxis::handle_known_hosts`: settings-panel-independent dispatch arms for the
//! known_hosts area, split out of dispatch.rs. Returns `Err(message)` for anything
//! it doesn't claim so the try_handler! chain falls through.
#![allow(clippy::result_large_err)]

use iced::Task;

use crate::app::{KnownHostMessage, Message, Oryxis};

impl Oryxis {
    pub(crate) fn handle_known_hosts(
        &mut self,
        message: KnownHostMessage,
    ) -> Task<Message> {
        match message {
            // -- Known hosts --
            KnownHostMessage::RequestDeleteKnownHost(idx) => {
                // Resolve the row to an id NOW, while the index is the
                // one the user clicked; the dialog it arms outlives that
                // certainty (see `KnownHostMessage::DeleteKnownHost`).
                let Some(kh) = self.known_hosts.get(idx) else {
                    return Task::none();
                };
                let (id, label) = (kh.id, format!("{}:{}", kh.hostname, kh.port));
                self.error_dialog = Some(crate::state::ErrorDialog {
                    title: crate::i18n::t("known_host_remove_confirm_title").to_string(),
                    body: format!(
                        "{label}: {}",
                        crate::i18n::t("known_host_remove_confirm_body")
                    ),
                    link: None,
                    action: Some(crate::state::ErrorDialogAction {
                        label: crate::i18n::t("remove").to_string(),
                        message: Box::new(Message::KnownHost(KnownHostMessage::DeleteKnownHost(id))),
                        danger: true,
                    }),
                });
            }
            KnownHostMessage::DeleteKnownHost(id) => {
                // An id that no longer exists is an entry that left while
                // the question was on screen: the DELETE matches nothing
                // and the reload is what the user would have asked for
                // anyway.
                if let Some(vault) = &self.vault {
                    let _ = vault.delete_known_host(&id);
                    self.load_data_from_vault();
                }
            }
            KnownHostMessage::RequestClearAllKnownHosts => {
                self.error_dialog = Some(crate::state::ErrorDialog {
                    title: crate::i18n::t("known_hosts_clear_confirm_title").to_string(),
                    body: crate::i18n::t("known_hosts_clear_confirm_body").to_string(),
                    link: None,
                    action: Some(crate::state::ErrorDialogAction {
                        label: crate::i18n::t("re_verify_all").to_string(),
                        message: Box::new(Message::KnownHost(KnownHostMessage::ClearAllKnownHosts)),
                        danger: true,
                    }),
                });
            }
            KnownHostMessage::ClearAllKnownHosts => {
                if let Some(vault) = &self.vault {
                    for kh in self.known_hosts.clone() {
                        let _ = vault.delete_known_host(&kh.id);
                    }
                    self.load_data_from_vault();
                }
            }
        }
        Task::none()
    }
}
