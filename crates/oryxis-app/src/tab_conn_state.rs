//! Derived connection state of a terminal tab: the one authority behind
//! the tab strip's status dot and the status bar's connection segment.
//!
//! The two surfaces used to answer the question independently, from
//! different signals: the strip read `session.is_some()` plus the
//! "(disconnected)" label suffix, while the bar read nothing at all and
//! claimed "connected" for any active tab. Two derivations of one fact
//! can disagree on the same frame (a split tab whose focused pane died
//! carries no label suffix, so the strip stayed green), so the
//! derivation lives here once and both callers map it to their own
//! presentation. Same one-owner rule as `mouse_binding_owner()`.

use crate::app::Oryxis;
use crate::state::{Pane, PaneOrigin, TerminalTab};

/// What a tab's FOCUSED pane can say about its transport. Presentation
/// (a dot color, a text segment) belongs to the callers; nothing here
/// knows about either.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TabConnState {
    /// A first dial for this tab is in flight.
    Connecting,
    /// The pane had a session, lost it, and an in-place reconnect is
    /// dialing.
    Reconnecting,
    /// The transport is up.
    Connected,
    /// The transport is up and OUT OF TOUCH: a mosh session that has not
    /// heard from its server, or is not being acknowledged by it.
    ///
    /// Deliberately not [`TabConnState::Lost`], and the distinction is
    /// the whole of what mosh is for. A mosh session survives its
    /// network: the address can change, the link can go away for an hour
    /// and come back, and the shell on the far side never noticed. So
    /// reading this as lost would be wrong twice over, once about the
    /// session and once about the protocol, and anything that tears down
    /// on `Lost` would tear down exactly the sessions built not to need
    /// it.
    ///
    /// It is also not [`TabConnState::Connected`], which is what it read
    /// as before: a link silent for a minute showed the same green dot
    /// as one answering instantly, so the one moment the protocol earns
    /// its keep was the one moment the interface said nothing.
    NoContact,
    /// The transport is gone: a remote pane whose session died, or a
    /// plugin-backed cloud tab whose process exited.
    Lost,
    /// Nothing to report. A local shell (its shell is not a connection)
    /// or a dormant pinned placeholder that never dialed.
    Idle,
}

impl TabConnState {
    /// The status dot's colour, `None` when there is nothing to report.
    ///
    /// One owner, for the same reason the derivation above is one: the
    /// strip's chip and a pane's own header both draw this dot, and a
    /// second inline match is how two surfaces start disagreeing about
    /// one fact. The status bar deliberately does NOT read this; it
    /// tints TEXT, and a colour that reads on a dot does not
    /// necessarily read on a word.
    pub(crate) fn dot_color(self) -> Option<iced::Color> {
        let c = crate::theme::OryxisColors::t();
        match self {
            TabConnState::Connecting
            | TabConnState::Reconnecting
            | TabConnState::NoContact => Some(c.warning),
            TabConnState::Lost => Some(c.error),
            TabConnState::Connected => Some(c.success),
            TabConnState::Idle => None,
        }
    }
}

/// The app's global connect progress, as it applies to ONE tab.
/// Resolving it is the caller's job because the progress lives on
/// `Oryxis`, not on the tab, and keeping it out of the derivation is
/// what makes that derivation testable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DialProgress {
    /// No progress card belongs to this tab.
    None,
    /// A dial for this tab is running.
    Dialing,
    /// The dial died and its card is still up, undismissed.
    Failed,
}

/// Derive `tab`'s state, which is its FOCUSED pane's state plus the
/// three signals that belong to the tab and have no per-pane answer.
pub(crate) fn derive_conn_state(
    tab: &TerminalTab,
    dial: DialProgress,
) -> TabConnState {
    let pane = tab.active();
    // A dial in flight beats every other signal: a tab that is
    // currently dialing isn't "disconnected" in the user's mental
    // model, and its pane legitimately has no session yet.
    if dial == DialProgress::Dialing {
        return TabConnState::Connecting;
    }
    if pane.connecting {
        return TabConnState::Reconnecting;
    }
    // A dead dial keeps its card (and this state) until the user
    // dismisses it. Only the initial-connect path leaves a pane with
    // no handle and no relabel, so without this the tab would read as
    // "nothing to report" while a red failure card fills the screen.
    if dial == DialProgress::Failed {
        return TabConnState::Lost;
    }
    // Plugin-backed cloud tabs (ECS Exec / SSM Session / `kubectl
    // exec`) carry no `session` handle: their transport IS the local
    // plugin process, and its only death signal is `PluginSessionEnded`
    // stamping this suffix. Checked before the session branch so the
    // SSH relabel (`dispatch_ssh::errors`) wins there too, whether or
    // not the dead handle is still attached.
    if tab.label.ends_with(" (disconnected)") {
        return TabConnState::Lost;
    }
    derive_pane_conn_state(tab, pane)
}

/// Derive ONE pane's state, for the surfaces that describe a pane
/// rather than a tab (the optional per-pane header, issue #208).
///
/// Everything the tab-wide derivation above cannot delegate is already
/// spent by the time it calls here: a dial in flight belongs to a tab
/// and carries no pane id, and the "(disconnected)" label suffix is
/// written on a tab and never on a split one. What is left is the pane
/// itself, plus `tab` for the one question a pane cannot answer alone
/// (a plugin-backed transport is a process the TAB owns).
pub(crate) fn derive_pane_conn_state(tab: &TerminalTab, pane: &Pane) -> TabConnState {
    // Reached directly by the header, and already spent when the tab
    // derivation delegates here. Both readings are the same.
    if pane.connecting {
        return TabConnState::Reconnecting;
    }
    // The verdict the pane recorded for ITSELF (issue #208) outranks
    // every guess below. It has to: a local shell that exited leaves
    // `session: None` and keeps `PaneOrigin::Local`, so the origin
    // branch would answer "nothing to report" about a pane that is
    // showing the end of its own session.
    if pane.ended {
        return TabConnState::Lost;
    }
    if let Some(session) = pane.session.as_ref() {
        // A split tab never gets the label suffix (the relabel only
        // fires for single-pane tabs, so its live siblings keep the tab
        // connected), which makes the handle the only witness that this
        // pane died.
        if !session.is_alive() {
            return TabConnState::Lost;
        }
        // A live mosh session has a second question to answer, and only
        // mosh has it: is anyone on the other end right now. Read from
        // the transport rather than passed in like `DialProgress`,
        // because it is the transport's own state and the pane already
        // reaches into `session` on the line above.
        return match session.mosh().map(|m| m.link_state()) {
            Some(oryxis_mosh::LinkState::NoContact { .. } | oryxis_mosh::LinkState::NoReply { .. }) => {
                TabConnState::NoContact
            }
            _ => TabConnState::Connected,
        };
    }
    // No handle at all. `PaneOrigin::Ephemeral` is the field's DEFAULT,
    // not a claim that the pane is remote, so it can't be read as one:
    // a live cloud tab and a dormant placeholder both land here.
    if matches!(pane.origin, PaneOrigin::Local(_)) {
        return TabConnState::Idle;
    }
    if tab.is_plugin_backed() {
        // Live by elimination: a dead plugin tab carries the suffix
        // the tab derivation handles.
        return TabConnState::Connected;
    }
    TabConnState::Idle
}

impl Oryxis {
    /// [`derive_conn_state`] for the tab at `idx`, with the global
    /// connect progress resolved against it.
    pub(crate) fn tab_conn_state(&self, idx: usize) -> TabConnState {
        let Some(tab) = self.tabs.get(idx) else {
            return TabConnState::Idle;
        };
        // Scoped by tab like `view_content`'s connect screen, so both
        // surfaces agree with what the tab is actually showing. The
        // `failed` split matters because a dead card stays in
        // `connecting` until dismissed: presence alone would keep
        // claiming a dial that is long over.
        let dial = match self.connecting.as_ref().filter(|c| c.tab_idx == idx) {
            Some(c) if c.failed => DialProgress::Failed,
            Some(_) => DialProgress::Dialing,
            None => DialProgress::None,
        };
        derive_conn_state(tab, dial)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{LocalShellSpec, TerminalState};
    use std::sync::{Arc, Mutex};

    fn tab(label: &str) -> TerminalTab {
        let term = TerminalState::new_no_pty(80, 24).unwrap();
        TerminalTab::new_single(label.into(), Arc::new(Mutex::new(term)))
    }

    /// The `session` branch needs a live transport, which no unit test
    /// can build (an `SshSession` owns a russh handle and spawned
    /// tasks). Everything reachable without one is covered here; the
    /// live-transport half is exercised by the e2e harness.
    #[test]
    fn a_dial_in_flight_wins_over_every_other_signal() {
        let mut t = tab("host (disconnected)");
        t.active_mut().connecting = true;
        assert_eq!(
            derive_conn_state(&t, DialProgress::Dialing),
            TabConnState::Connecting
        );
    }

    #[test]
    fn an_in_place_reconnect_reads_as_reconnecting() {
        let mut t = tab("host");
        t.active_mut().connecting = true;
        assert_eq!(
            derive_conn_state(&t, DialProgress::None),
            TabConnState::Reconnecting
        );
    }

    /// A failed card sits in `connecting` until dismissed, so reading
    /// the progress as a boolean would paint a dead dial "connecting"
    /// for as long as its red failure log is on screen.
    #[test]
    fn a_failed_dial_reads_as_lost_not_connecting() {
        let t = tab("host");
        assert_eq!(
            derive_conn_state(&t, DialProgress::Failed),
            TabConnState::Lost
        );
    }

    /// The suffix is the only death signal a plugin-backed cloud tab
    /// has, since its transport is a local process rather than a
    /// `session` handle.
    #[test]
    fn a_disconnected_label_reads_as_lost() {
        let mut t = tab("ECS · api (disconnected)");
        t.ssm_keepalive = true;
        assert_eq!(derive_conn_state(&t, DialProgress::None), TabConnState::Lost);
    }

    /// The regression this module exists for: a live ECS / SSM /
    /// kubectl tab has no `session` and keeps the default
    /// `PaneOrigin::Ephemeral`, which a naive "not local means remote"
    /// test paints as a dead connection for the session's whole life.
    #[test]
    fn a_live_plugin_tab_reads_as_connected() {
        let mut t = tab("ECS · api (abc12345)");
        t.ssm_keepalive = true;
        assert_eq!(derive_conn_state(&t, DialProgress::None), TabConnState::Connected);
    }

    /// A pane that recorded the end of its own session reads as lost
    /// even when nothing else about it changed. A local shell is the
    /// case that needs it: it keeps `PaneOrigin::Local` and never had a
    /// handle, so every other branch answers "nothing to report" about
    /// a pane the user is being offered a restart for.
    #[test]
    fn an_ended_pane_reads_as_lost_even_when_it_is_a_local_shell() {
        let mut t = tab("bash");
        t.active_mut().origin = PaneOrigin::Local(LocalShellSpec {
            label: "bash".into(),
            program: "/bin/bash".into(),
            args: Vec::new(),
        });
        t.active_mut().ended = true;
        let pane = t.active();
        assert_eq!(derive_pane_conn_state(&t, pane), TabConnState::Lost);
        assert_eq!(derive_conn_state(&t, DialProgress::None), TabConnState::Lost);
    }

    /// The dot is one owner for two surfaces, so the states that must
    /// show nothing and the states that must show something are pinned
    /// here rather than in either caller.
    #[test]
    fn only_idle_draws_no_status_dot() {
        assert!(TabConnState::Idle.dot_color().is_none());
        for state in [
            TabConnState::Connecting,
            TabConnState::Reconnecting,
            TabConnState::NoContact,
            TabConnState::Connected,
            TabConnState::Lost,
        ] {
            assert!(state.dot_color().is_some(), "{state:?} drew no dot");
        }
    }

    #[test]
    fn a_local_shell_reports_nothing() {
        let mut t = tab("Local Shell");
        t.active_mut().origin = PaneOrigin::Local(LocalShellSpec {
            label: "bash".into(),
            program: "/bin/bash".into(),
            args: Vec::new(),
        });
        assert_eq!(derive_conn_state(&t, DialProgress::None), TabConnState::Idle);
    }

    /// A dormant pinned placeholder never dialed: it has no handle, no
    /// plugin process and no suffix, so there is nothing to claim.
    #[test]
    fn a_dormant_placeholder_reports_nothing() {
        let t = tab("prod-db");
        assert_eq!(derive_conn_state(&t, DialProgress::None), TabConnState::Idle);
    }
}
