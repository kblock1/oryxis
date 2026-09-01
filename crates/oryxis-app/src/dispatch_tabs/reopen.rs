//! Reopen the last closed tab (issue #186): the capture side that every
//! user-driven close calls, and the `ReopenClosedTab` handler that brings
//! one back.
//!
//! Closing a tab is one click on a small target sitting between two other
//! small targets, and the session behind it is not recoverable by
//! undoing anything else. The browsers' answer is a stack of recently
//! closed tabs, and it works here because the app already has a
//! serializable "how do I recreate this tab" value: the pin spec. So a
//! reopen is not a second restore mechanism, it is the pin's resolution
//! (`spec_open_message`) reached from a different door.

use iced::Task;

use crate::app::{Message, Oryxis, SftpMessage, SshMessage};
use crate::state::{ClosedTab, ClosedTabSpec};

/// How many closed tabs the stack keeps.
///
/// Deep enough to cover the mistake this exists for (a misclick noticed a
/// few tabs later) and the "close others" that drops a screenful at once,
/// shallow enough that it stays a recent-history list rather than a
/// session archive nobody prunes.
const CLOSED_TABS_MAX: usize = 10;

impl Oryxis {
    /// Remember the terminal tab at `idx` so it can be reopened.
    ///
    /// Called from the paths where the USER closed a tab, never from
    /// `teardown_tab_at`: the reconnect rebuild tears a tab down to put an
    /// equivalent one back in the same breath, and an entry minted there
    /// would offer the user a tab they never closed. The three sites are
    /// `close_tab_now`, `CloseOtherTabs` and `CloseAllTabs`.
    pub(crate) fn remember_closed_tab(&mut self, idx: usize) {
        let Some(tab) = self.tabs.get(idx) else { return };
        // `pin_spec` answers first, because everything it can describe
        // reopens through the pin's own resolution. It reads the FOCUSED
        // pane, so a split tab comes back as a single pane on that host.
        // Same answer pinning gives a split, and the honest one: the
        // panes are separate live sessions, not a layout that can be
        // re-dialled as a unit.
        let spec = match tab.pin_spec() {
            Some(spec) => ClosedTabSpec::Pinned(spec),
            // What it declines is quick-connect and SSM, and only the
            // first of those is recoverable: an ad-hoc host is a
            // `Connection` living in `quick_connects`, which
            // `prune_quick_connects` is about to drop, so the snapshot
            // is taken here and owned by the stack. SSM has nothing to
            // snapshot (`relaunch: None`).
            None => match &tab.active().origin {
                crate::state::PaneOrigin::QuickHost(qid) => {
                    let Some(entry) = self.quick_connects.get(qid) else { return };
                    ClosedTabSpec::QuickHost(Box::new(quick_snapshot(entry)))
                }
                _ => return,
            },
        };
        let id = tab._id;
        let after_id = self.chip_to_the_left_of(id);
        self.push_closed_tab(ClosedTab { spec, after_id });
    }

    /// The SFTP half. Same rule, different storage vec: called from the
    /// user close paths in `dispatch_sftp::tabs`, not from
    /// `close_sftp_tab` itself, because the "Open terminal" morph closes
    /// the SFTP tab as its last step and that tab did not die, it became
    /// the terminal tab beside it.
    pub(crate) fn remember_closed_sftp_tab(&mut self, idx: usize) {
        let Some(spec) = self.sftp_pin_spec(idx) else { return };
        let Some(id) = self.sftp_tabs.get(idx).map(|t| t.id) else { return };
        let after_id = self.chip_to_the_left_of(id);
        self.push_closed_tab(ClosedTab {
            spec: ClosedTabSpec::Pinned(spec),
            after_id,
        });
    }

    /// Strip id of the chip immediately before `id`, or `None` when it is
    /// the first one (or not in the strip at all, which reads the same:
    /// nothing to come back next to).
    fn chip_to_the_left_of(&self, id: uuid::Uuid) -> Option<uuid::Uuid> {
        let at = self.tab_order.iter().position(|r| r.strip_id() == id)?;
        self.tab_order.get(at.checked_sub(1)?).map(|r| r.strip_id())
    }

    fn push_closed_tab(&mut self, entry: ClosedTab) {
        push_closed(&mut self.closed_tabs, entry);
    }

    /// Bring back the most recently closed tab.
    ///
    /// Pops until one resolves rather than failing on the newest: a host
    /// deleted since it was closed can never be reopened, and stopping
    /// there would wedge the whole stack behind a dead entry with nothing
    /// on screen saying why. An empty stack is a no-op, like every
    /// browser's.
    pub(super) fn handle_reopen_closed_tab(&mut self) -> Task<Message> {
        use crate::state::PinnedTabSpec;
        // Four menus reach this now (the tab chip's, the SFTP chip's,
        // the strip's own and the `+` popover), and the row that fires
        // it is the last thing they have to say. The hotkey path clears
        // nothing that was open.
        self.overlay = None;
        while let Some(entry) = self.closed_tabs.pop() {
            let spec = match entry.spec {
                ClosedTabSpec::Pinned(spec) => spec,
                // An ad-hoc host reopens by putting its `Connection`
                // back in `quick_connects` and firing the message the
                // quick-connect card fires, which is also the switch
                // that routes Telnet / Raw / Serial / Local / remote
                // desktop to their own connect paths. A dial site of its
                // own here would be a second one to keep correct.
                //
                // `QuickConnect` reuses a live entry for the same id
                // when one exists, so reopening a tab whose twin is
                // still open dials with the credentials that twin is
                // already using instead of asking again.
                ClosedTabSpec::QuickHost(conn) => {
                    self.arm_reopen_placement(entry.after_id);
                    let entry = crate::state::QuickConnectEntry::bare(*conn);
                    return self.update(Message::Ssh(SshMessage::QuickConnect(Box::new(entry))));
                }
            };
            // An SFTP tab is recreated here rather than through
            // `spec_open_message`: it lives in `sftp_tabs`, and its
            // reopen IS the dormant-pin path, a chip that re-mounts its
            // panes when it is first selected.
            //
            // This test has to stay ABOVE the `spec_open_message` call,
            // whose `Sftp` arm answers `None` (that path only ever
            // produces terminal tabs). Below it, every SFTP entry would
            // fall into the `continue` meant for a deleted host and be
            // eaten without a chip to show for it.
            if matches!(spec, PinnedTabSpec::Sftp { .. }) {
                return self.reopen_closed_sftp_tab(spec, entry.after_id);
            }
            // The async flag is the dormant path's business (it has a
            // placeholder chip to hold open while a plugin answers);
            // here the placement below covers both kinds.
            let (open, _async_spawn) = self.spec_open_message(&spec);
            let Some(open) = open else { continue };
            // Where the chip goes back, through the one door every new
            // tab walks (`place_new_tab_ref`). It resolves the neighbour
            // fresh, so a neighbour that closed in the meantime degrades
            // to appending instead of guessing a stale index, and it is
            // the only placement that also works for the cloud specs,
            // whose tab arrives several updates later.
            self.arm_reopen_placement(entry.after_id);
            return self.update(open);
        }
        Task::none()
    }

    fn reopen_closed_sftp_tab(
        &mut self,
        spec: crate::state::PinnedTabSpec,
        after_id: Option<uuid::Uuid>,
    ) -> Task<Message> {
        // Dormant, exactly like a pinned SFTP tab restored at boot: the
        // panes re-mount on the first select rather than here, so one
        // path owns the mount and the reopen cannot drift from it.
        let label = spec.label().to_string();
        let tab = crate::state::SftpTab::new_dormant(label, spec);
        let id = tab.id;
        self.sftp_tabs.push(tab);
        let idx = self.sftp_tabs.len() - 1;
        // Placed by hand: `reconcile_tab_order` appends SFTP refs, and
        // only terminal tabs go through the placement door. Same three
        // answers that door gives, so the two kinds come back to the same
        // place.
        let at = match after_id {
            // It was the first chip, so it comes back as the first chip.
            None => 0,
            Some(a) => self
                .tab_order
                .iter()
                .position(|r| r.strip_id() == a)
                .map(|p| p + 1)
                // The neighbour closed in the meantime: the end is where
                // a new tab would have gone anyway.
                .unwrap_or(self.tab_order.len()),
        };
        self.tab_order
            .insert(at.min(self.tab_order.len()), crate::state::TabRef::Sftp(id));
        self.update(Message::Sftp(SftpMessage::SelectSftpTab(idx)))
    }

    /// Arm the strip placement for the tab a reopen is about to spawn.
    ///
    /// Not `arm_tab_placement`, which reads the `duplicate_tab_position`
    /// setting: a reopen has an answer of its own, the slot the tab held
    /// when it left.
    fn arm_reopen_placement(&mut self, after: Option<uuid::Uuid>) {
        use crate::state::{PendingTabPlacement, TabPlacement};
        let placement = match after {
            Some(_) => TabPlacement::NextToOriginal,
            // It was the first chip, so it comes back as the first chip.
            None => TabPlacement::Start,
        };
        self.pending_tab_placement = Some(PendingTabPlacement {
            // Unread under `Start`, which needs no anchor.
            source_id: after.unwrap_or_default(),
            placement,
            armed_at: std::time::Instant::now(),
        });
    }
}

/// The `Connection` a quick-connect tab leaves behind when it closes.
///
/// A copy of the entry's own, with one correction. The credentials stay
/// behind (they never lived in `conn`), and an ad-hoc host set to
/// `Password` has none without them: the engine reads that method as "a
/// password was supplied", so it would fail the auth outright instead of
/// asking for one. `PasswordPrompt` is that same method WITH the asking,
/// which is what a reopen has to do anyway, and it is the method that
/// deliberately stores nothing.
///
/// Only when the password was typed into this entry. A `Password` host
/// pointing at a saved identity hydrates from the vault on the way back,
/// and turning it into a prompt would ask the user for something the app
/// already has.
fn quick_snapshot(
    entry: &crate::state::QuickConnectEntry,
) -> oryxis_core::models::Connection {
    use oryxis_core::models::connection::AuthMethod;
    let mut conn = entry.conn.clone();
    if entry.password.is_some() && conn.auth_method == AuthMethod::Password {
        conn.auth_method = AuthMethod::PasswordPrompt;
    }
    conn
}

/// Pure half of [`Oryxis::push_closed_tab`], so the cap is testable
/// without an `Oryxis`.
fn push_closed(stack: &mut Vec<ClosedTab>, entry: ClosedTab) {
    stack.push(entry);
    // Oldest first out, so the cap never costs the user the tab they just
    // closed.
    if stack.len() > CLOSED_TABS_MAX {
        stack.remove(0);
    }
}

#[cfg(test)]
mod tests {
    use super::{push_closed, quick_snapshot, CLOSED_TABS_MAX};
    use crate::state::{ClosedTab, ClosedTabSpec, PinnedTabSpec, QuickConnectEntry};
    use oryxis_core::models::connection::{AuthMethod, Connection};

    fn entry(n: u128) -> ClosedTab {
        ClosedTab {
            spec: ClosedTabSpec::Pinned(PinnedTabSpec::Host {
                id: uuid::Uuid::from_u128(n),
                label: format!("host-{n}"),
            }),
            after_id: None,
        }
    }

    fn quick(auth: AuthMethod, password: Option<&str>) -> QuickConnectEntry {
        let mut conn = Connection::new("ad-hoc", "example.test");
        conn.auth_method = auth;
        QuickConnectEntry {
            conn,
            password: password.map(str::to_string),
            totp_secret: None,
            proxy_password: None,
        }
    }

    /// The snapshot carries no secret, whatever the entry held. This is
    /// the whole reason a quick-connect tab may sit in a stack that a pin
    /// refuses it.
    #[test]
    fn the_quick_snapshot_leaves_every_credential_behind() {
        let entry = quick(AuthMethod::Password, Some("hunter2"));
        let snapshot = format!("{:?}", quick_snapshot(&entry));

        assert!(!snapshot.contains("hunter2"), "password in the snapshot");
    }

    /// A typed password becomes a prompt, because the engine would
    /// otherwise fail `Password` auth outright rather than ask for the
    /// password the reopen deliberately did not keep.
    #[test]
    fn a_typed_password_reopens_as_a_prompt() {
        let snap = quick_snapshot(&quick(AuthMethod::Password, Some("hunter2")));
        assert_eq!(snap.auth_method, AuthMethod::PasswordPrompt);
    }

    /// ...and a `Password` host that never had one typed keeps the
    /// method: its password comes from the linked identity in the vault,
    /// and prompting would ask for what the app already has.
    #[test]
    fn a_password_host_with_no_typed_password_is_left_alone() {
        let snap = quick_snapshot(&quick(AuthMethod::Password, None));
        assert_eq!(snap.auth_method, AuthMethod::Password);
    }

    /// Every other method is left alone too: `Auto` reaches the
    /// quick-connect interactive fallback on its own, and `Key` / `Agent`
    /// authenticate with something that was never in the entry.
    #[test]
    fn other_auth_methods_survive_the_snapshot_verbatim() {
        for auth in [AuthMethod::Auto, AuthMethod::Key, AuthMethod::Agent] {
            let snap = quick_snapshot(&quick(auth.clone(), Some("hunter2")));
            assert_eq!(snap.auth_method, auth);
        }
    }

    /// The cap drops the OLDEST entry. Closing eleven tabs must not cost
    /// the eleventh, which is the one the user is about to ask back.
    #[test]
    fn the_cap_drops_the_oldest_not_the_newest() {
        let mut stack: Vec<ClosedTab> = Vec::new();
        for n in 0..(CLOSED_TABS_MAX as u128 + 1) {
            push_closed(&mut stack, entry(n));
        }

        assert_eq!(stack.len(), CLOSED_TABS_MAX);
        let host_id = |e: &ClosedTab| match &e.spec {
            ClosedTabSpec::Pinned(PinnedTabSpec::Host { id, .. }) => *id,
            _ => unreachable!(),
        };
        let newest = host_id(stack.last().unwrap());
        assert_eq!(newest, uuid::Uuid::from_u128(CLOSED_TABS_MAX as u128));
        let oldest = host_id(stack.first().unwrap());
        assert_eq!(oldest, uuid::Uuid::from_u128(1), "entry 0 was dropped");
    }
}
