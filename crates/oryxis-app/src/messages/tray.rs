//! System-tray messages (Windows runtime), wrapped by [`crate::messages::Message::Tray`]. Handled by `Oryxis::handle_tray`.


#[derive(Debug, Clone)]
pub enum TrayMessage {
    /// A tray menu item was clicked (carries the raw menu id). Delivered
    /// by the event-driven tray subscription so a click wakes the UI
    /// only when it happens, instead of the old 100 ms poll that
    /// re-rendered the whole app 10x/s on Windows. Only constructed on
    /// Windows (the subscription is only mounted there).
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    MenuEvent(String),
    /// The tray icon was double-clicked (restore the window). Same
    /// event-driven delivery as [`TrayMenuEvent`].
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    IconDoubleClick,
    /// 100 ms ticker emitted by the iced subscription. The handler
    /// drains the tray-icon crate's crossbeam event channels and
    /// re-emits real `TrayShow / TrayHide / TrayQuit` messages.
    /// Polling here is acceptable noise (~10 ticks/sec, each a
    /// non-blocking `try_recv`) and avoids wiring a custom
    /// Subscription stream that bridges crossbeam into iced.
    /// The ticker only mounts on Windows (the tray lives there),
    /// hence the cfg'd allow.
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    Poll,
    /// A `oryxis://` URL claimed from the cross-process deep-link
    /// inbox (`tray_ipc::take_deeplinks`) by this window's
    /// `deep_link_stream` subscription. All platforms, unlike the
    /// rest of this enum's delivery paths: the inbox is how a scheme
    /// launch reaches an already-running instance. Carries the raw
    /// URL; `handle_deep_link_url` parses and routes it.
    DeepLink(String),
    /// `oryxis user@host` target forwarded by a CLI launcher process
    /// and claimed from the connect inbox. Separate from `DeepLink`
    /// because it dials instead of prefilling a confirm surface.
    ConnectTarget(String),
    /// User clicked "Show Oryxis" in the tray menu, or left-clicked
    /// the tray icon. Bring the main window back from hidden state
    /// and pull it to the foreground.
    Show,
    /// User clicked "Hide to tray". Hide the main window (true
    /// hide via Win32 ShowWindow, not just minimize) and leave
    /// only the tray icon present.
    Hide,
    /// User clicked "Quit" in the tray menu. Tear down the tray
    /// icon and exit the process. Guarded: when the live-session
    /// confirm pref is on and some tab still holds a session, this
    /// asks first (the tray's Quit is the only exit verb once the
    /// window is hidden to the tray, so a mis-click there drops
    /// everything exactly like a mis-click on a tab's X).
    Quit,
    /// Second step of the tray "Quit" guard: the confirmation said
    /// yes, so exit without asking again. Only the dialog can
    /// produce this message.
    ConfirmQuit,
    /// User clicked an entry in the tray menu's "Active sessions"
    /// section. Payload is the tab index from `Oryxis::tabs`. The
    /// handler shows the window (in case it was hidden) and selects
    /// the tab.
    ActivateSession(usize),
    /// User clicked an entry in the tray menu's "Recent hosts"
    /// section. Payload is the connection UUID. The handler shows
    /// the window and opens a new tab against that connection.
    OpenHost(uuid::Uuid),
}
