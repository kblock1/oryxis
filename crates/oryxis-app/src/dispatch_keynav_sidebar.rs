//! Keyboard router for the terminal-sidebar tabs (Chat / Snippets /
//! History / Host config), iteration 3 of the focus-zone framework.
//!
//! Unlike modals and side panels, this surface coexists with a live
//! terminal that owns every plain key, so the layer is strictly
//! opt-in:
//!
//! - The FocusSidebarList hotkey opens the sidebar (when closed) and
//!   cycles every visible tab; landing engages the tab's rows.
//! - Up / Down while the mouse cursor is over a LIST tab engage the
//!   ring directly (those keys were already swallowed there, never
//!   reaching the PTY, so this upgrades a dead key into navigation).
//! - Tab / Shift+Tab walk EVERY recorded row (panel contract: input
//!   rows get real iced focus, the rest get the ring) while the ring
//!   is engaged OR the cursor is over the sidebar. With the cursor
//!   over the terminal and no ring, Tab stays a PTY `\t`.
//!
//! While engaged: Up/Down/Home/End move over non-input rows
//! (wrapping), Enter or Space activates (list rows RUN their
//! command), Shift+Enter pastes without the newline, Left/Right
//! cycle picker rows (the font-size stepper, the chat mode chips)
//! and otherwise move the ring too (the header buttons sit side by
//! side, owner QA), Delete removes (through the row's confirm), the
//! Menu key opens the row's context menu when it has one (anchored at
//! the ringed row, so a row whose extra actions live in a popover is
//! never mouse-only), Esc disengages. Everything else, typing
//! included, keeps its normal
//! routing, so the terminal (or a focused search field) still
//! receives text while the ring is up; the selection is tagged by
//! sidebar tab and clamped against each frame's recording, so
//! filtering while ringed just clamps.
//!
//! Delete has a second, RING-LESS reading, and only on Files: a click
//! there selects a row and deliberately drops the ring, so the key
//! falls back to the mouse selection
//! (`sidebar_files_selected_entries`, the whole multi-selection), which
//! is the select-then-Del pair the SFTP pane offers. That fallback is
//! what makes the inline-edit guard in that helper load bearing: this
//! layer engages on the cursor alone, so a Delete typed into a Files
//! input must not reach a file.

use iced::keyboard;
use iced::Task;

use crate::app::{AiMessage, Message, Oryxis, SidebarFilesMessage};
use crate::keynav::movement::index_move;
use crate::keynav::SidebarRow;
use crate::state::TerminalSidebarTab;

/// Blur every focusable (focusing a nonexistent id): moving the ring
/// onto a non-input row takes the keyboard away from whatever input
/// had focus (same trick as the side-panel router).
fn blur_task() -> Task<Message> {
    iced::widget::operation::focus(iced::widget::Id::new("__keynav_blur__"))
}

impl Oryxis {
    /// The sidebar tab actually shown by `side`'s region for the
    /// active terminal tab, or `None` when no terminal tab is active,
    /// that region is closed / empty, or the tab is in Files mode
    /// (which replaces the whole tab content, sidebar included).
    /// Mirrors the resolution in `view_terminal_sidebar`.
    pub(crate) fn effective_sidebar_tab(
        &self,
        side: crate::state::SidebarSide,
    ) -> Option<TerminalSidebarTab> {
        if !self.active_sidebar_shown(side) {
            return None;
        }
        self.sidebar_region_tab(side)
    }

    /// The region the keyboard belongs to right now, with its shown
    /// tab: a live ring pins its own region (as long as that region
    /// still shows the ring's tab); otherwise the region under the
    /// cursor. `None` = the sidebar layer owns nothing this keypress.
    fn engaged_sidebar_context(
        &self,
    ) -> Option<(crate::state::SidebarSide, TerminalSidebarTab)> {
        if let Some((tab, _)) = self.keynav.sidebar_selected
            && let Some(side) = self.prefs.sidebar_tab_side(tab)
            && self.effective_sidebar_tab(side) == Some(tab)
        {
            return Some((side, tab));
        }
        let side = self.cursor_over_sidebar_side()?;
        Some((side, self.sidebar_region_tab(side)?))
    }

    /// The (open) sidebar region the mouse cursor currently sits
    /// over, if any. Keys there never reach the PTY (the chat-sidebar
    /// swallow gate), so promoting them to navigation costs nothing.
    pub(crate) fn cursor_over_sidebar_side(&self) -> Option<crate::state::SidebarSide> {
        use crate::state::SidebarSide;
        let tab = self.active_tab.and_then(|i| self.tabs.get(i))?;
        // Each region hugs its physical edge, shifted inward by a
        // side-docked tab strip (issue #87): with both on the same
        // edge the region starts AFTER the strip, and classifying the
        // strip band as "sidebar" would let arrow keys over the strip
        // engage the ring while the region's inner edge leaked keys
        // into the PTY.
        let strip_left = self.side_strip_left_offset();
        let strip_right = self.side_strip_reserve() - strip_left;
        let x = self.mouse_position.x;
        if self.sidebar_region_shown(tab, SidebarSide::Left)
            && x > strip_left
            && x < strip_left + self.chat_ui.sidebar_width[SidebarSide::Left.idx()]
        {
            return Some(SidebarSide::Left);
        }
        if self.sidebar_region_shown(tab, SidebarSide::Right) {
            let right_edge = self.window_size.width - strip_right;
            if x > right_edge - self.chat_ui.sidebar_width[SidebarSide::Right.idx()]
                && x < right_edge
            {
                return Some(SidebarSide::Right);
            }
        }
        None
    }

    /// Whether the cursor is over ANY open sidebar region.
    pub(crate) fn cursor_over_sidebar(&self) -> bool {
        self.cursor_over_sidebar_side().is_some()
    }

    /// Close a pending Files path edit and its history dropdown, if
    /// any. Moving the keyboard (or the mouse, or the sidebar tab) off
    /// the path is its blur, and while editing the header hides the
    /// action icons so the input can take the whole width; the blur
    /// must snap that back (owner ask). No-op on every other state.
    pub(crate) fn close_files_path_edit(&mut self) {
        if let Some(idx) = self.active_tab
            && let Some(tab) = self.tabs.get_mut(idx)
        {
            let files = &mut tab.active_mut().files;
            files.path_editing = None;
            files.path_history_open = false;
        }
    }

    /// Whether the recorded row at `idx` of `tab`'s region is an
    /// input row (Tab focuses it instead of ringing it).
    fn sidebar_row_is_input(&self, tab: TerminalSidebarTab, idx: usize) -> bool {
        self.sidebar_items_for(tab)
            .borrow()
            .get(idx)
            .is_some_and(|r| r.action.focus.is_some())
    }

    /// The recorded row list of `tab`'s region (issue #102: one list
    /// per region; the tab names its region via its dock side). A
    /// hidden tab records nowhere; the Right fallback only keeps a
    /// misuse harmless (callers reach here via shown tabs only).
    fn sidebar_items_for(
        &self,
        tab: TerminalSidebarTab,
    ) -> &std::cell::RefCell<Vec<SidebarRow>> {
        let side = self
            .prefs
            .sidebar_tab_side(tab)
            .unwrap_or(crate::state::SidebarSide::Right);
        &self.keynav.sidebar_items[side.idx()]
    }

    /// The Files browser's mouse selection as delete targets
    /// (`(full path, is_dir)` pairs, in listing order). `None` on any
    /// other tab, with no selection, or when the selection no longer
    /// matches the listing (a refresh raced the key): a stale
    /// selection must never guess `is_dir`, so the key simply isn't
    /// consumed. Also `None` while an inline edit (path / rename /
    /// new entry) owns the keyboard, which is the SFTP pane's
    /// `editing` guard under another name. This layer engages on the
    /// CURSOR being over the sidebar, and the ring goes quiet on an
    /// input row, so without it a Delete meant to erase a character
    /// forward inside the rename field would raise the destructive
    /// confirm on the rows the mouse selected earlier, with the
    /// removal as its default button: the next Enter, the one that
    /// was going to commit the rename, would delete the files
    /// instead. The refusal lives HERE rather than in the Delete arm
    /// so a later ring-less caller (the Menu key is the obvious one)
    /// inherits it.
    fn sidebar_files_selected_entries(
        &self,
        tab: TerminalSidebarTab,
    ) -> Option<Vec<(String, bool)>> {
        if tab != TerminalSidebarTab::Files {
            return None;
        }
        let pane = self.tabs.get(self.active_tab?)?.active();
        if pane.files.path_editing.is_some()
            || pane.files.rename.is_some()
            || pane.files.new_entry.is_some()
        {
            return None;
        }
        let selected = crate::dispatch_sidebar_files::selected_items(&pane.files);
        (!selected.is_empty()).then_some(selected)
    }

    /// Keep the selected row visible; same best-effort relative snap
    /// as the side-panel router (iced exposes no row bounds). The
    /// scrollable id is per TAB (`keynav::sidebar_scroll_id`), since
    /// both regions can mount a list at once; tabs without that
    /// scrollable no-op.
    fn sidebar_nav_scroll(&self, tab: TerminalSidebarTab, idx: usize) -> Task<Message> {
        let len = self.sidebar_items_for(tab).borrow().len();
        let denom = len.saturating_sub(1).max(1);
        iced::widget::operation::snap_to(
            crate::keynav::sidebar_scroll_id(tab),
            iced::widget::operation::RelativeOffset {
                x: None,
                y: Some(idx as f32 / denom as f32),
            },
        )
    }

    /// Tab / Shift+Tab over the recorded sidebar rows (panel
    /// contract): input rows receive real iced focus, non-input rows
    /// show the ring and blur whatever input had the keyboard.
    fn sidebar_nav_tab(&mut self, tab: TerminalSidebarTab, forward: bool) -> Option<Task<Message>> {
        let len = self.sidebar_items_for(tab).borrow().len();
        if len == 0 {
            return None;
        }
        let cur = match self.keynav.sidebar_selected {
            Some((tag, idx)) if tag == tab => Some(idx.min(len - 1)),
            _ => None,
        };
        let next = match cur {
            Some(_) => index_move(len, cur, forward)?,
            // A fresh forward walk starts on the list BODY (the
            // mouse-selected anchor when one exists, else the first
            // list row), NOT on the strip's own chrome: starting there
            // puts Enter one keypress away from closing the panel
            // while the user meant "go to the rows below" (live QA on
            // the Hosts tree, whose search holds real focus with no
            // ring). Same landing rule as arrow entry; the chrome pair
            // stays reachable by walking on (the walk wraps).
            // Backward keeps the end-of-list start.
            None if forward => {
                let items = self.sidebar_items_for(tab).borrow();
                items
                    .iter()
                    .position(|r| r.anchor)
                    .or_else(|| items.iter().position(|r| r.list))
                    // No list rows this frame (empty tab body): the
                    // first NON-CHROME row, so an empty Snippets list
                    // lands on "+ SNIPPET" and Chat lands on its mode
                    // picker. Never an index guess: the chrome count
                    // varies per tab (Chat records Reset before
                    // Close), which is how a fresh walk into Chat once
                    // ringed the CLOSE button.
                    .or_else(|| items.iter().position(|r| !r.chrome))
                    .unwrap_or(0)
            }
            None => len - 1,
        };
        self.keynav.sidebar_selected = Some((tab, next));
        let action = self.sidebar_items_for(tab).borrow().get(next)?.action.clone();
        let step = match action.focus {
            Some(id) => crate::widgets::focus_input(id),
            None => {
                // Walking onto a non-input row blurs whatever input had
                // the keyboard; that blur also closes a pending Files
                // path edit (which was holding the whole header width).
                self.close_files_path_edit();
                blur_task()
            }
        };
        Some(Task::batch([step, self.sidebar_nav_scroll(tab, next)]))
    }

    /// Entry point, called from the `KeyboardEvent` arm right after
    /// the vault-area router. Returns `Some(task)` when consumed.
    pub(crate) fn handle_sidebar_nav_key(
        &mut self,
        event: &keyboard::Event,
    ) -> Option<Task<Message>> {
        let (_side, tab) = self.engaged_sidebar_context()?;
        if self.any_modal_blocks_input() || self.panels.host_panel {
            return None;
        }
        let keyboard::Event::KeyPressed { key, modifiers, .. } = event else {
            return None;
        };
        let len = self.sidebar_items_for(tab).borrow().len();
        // Selection engaged on the visible tab, clamped against this
        // frame's recording (a search filter can shrink it).
        let selected = match self.keynav.sidebar_selected {
            Some((tag, idx)) if tag == tab && len > 0 => Some(idx.min(len - 1)),
            _ => None,
        };
        // Ctrl+F with the keyboard in the sidebar (owner ask): open /
        // focus the active tab's search field instead of sending the
        // readline forward-char to the PTY. Same ownership gate as the
        // Tab walk; tabs without a search decline so nothing surprising
        // gets consumed.
        if modifiers.control()
            && !modifiers.alt()
            && !modifiers.logo()
            && !modifiers.shift()
            && matches!(key, keyboard::Key::Character(c) if c.as_str().eq_ignore_ascii_case("f"))
            && (selected.is_some() || self.cursor_over_sidebar())
        {
            match tab {
                TerminalSidebarTab::Snippets => {
                    // The input's focused border takes over as the
                    // keyboard affordance; a ring left behind on some
                    // other row would read as stuck.
                    self.keynav.sidebar_selected = None;
                    if self.sidebar_search_open {
                        return Some(crate::widgets::focus_input(iced::widget::Id::new(
                            "sidebar-snippet-search",
                        )));
                    }
                    // Opens the field and focuses it (the handler does).
                    return Some(self.update(Message::Ai(AiMessage::ToggleSidebarSearch)));
                }
                TerminalSidebarTab::History => {
                    self.keynav.sidebar_selected = None;
                    return Some(crate::widgets::focus_input(iced::widget::Id::new(
                        "sidebar-history-search",
                    )));
                }
                TerminalSidebarTab::HostsTree => {
                    self.keynav.sidebar_selected = None;
                    return Some(crate::widgets::focus_input(iced::widget::Id::new(
                        "sidebar-hosts-search",
                    )));
                }
                // No search on Chat / Host config.
                _ => return None,
            }
        }
        // Ctrl+A / Cmd+A on the Files tab: select every visible row,
        // anchored on the first so a follow-up shift-click keeps
        // well-defined semantics (the SFTP pane's rule). An inline
        // edit (path / rename / new entry) owns the key: there it is
        // the input's own select-all. Gated on the same sidebar
        // engagement as everything here, so a Ctrl+A meant for the
        // terminal (readline line-start) is never stolen.
        if tab == TerminalSidebarTab::Files
            && (modifiers.control() || modifiers.command())
            && !modifiers.alt()
            && matches!(key, keyboard::Key::Character(c) if c.as_str().eq_ignore_ascii_case("a"))
        {
            let idx = self.active_tab?;
            let files = &mut self.tabs.get_mut(idx)?.active_mut().files;
            // DECLINE rather than consume while an inline edit owns the
            // keyboard: there Ctrl+A is the text input's own select-all,
            // and swallowing it here would leave the field with a key
            // that does nothing. Same reason the ring-less Delete
            // refuses in `sidebar_files_selected_entries`, and the same
            // shape: not acting is not the same as consuming.
            if files.path_editing.is_some()
                || files.rename.is_some()
                || files.new_entry.is_some()
            {
                return None;
            }
            let paths = crate::dispatch_sidebar_files::visible_entry_paths(files);
            if paths.is_empty() {
                return None;
            }
            files.selection_anchor = Some(paths[0].clone());
            files.selected = paths;
            return Some(Task::none());
        }
        if modifiers.control() || modifiers.alt() || modifiers.logo() {
            return None;
        }
        // The ring is "active" only on non-input rows; while the
        // selection points at an input row the real focus owns the
        // keys (typing, caret, the chat editor's own Enter binding).
        let ring = selected.filter(|&i| !self.sidebar_row_is_input(tab, i));

        // Space presses the ringed control, matching the desktop
        // convention (owner ask). Only while ringed: an idle Space
        // belongs to the PTY (and typing there drops the ring anyway).
        let is_space = matches!(key, keyboard::Key::Named(keyboard::key::Named::Space))
            || matches!(key, keyboard::Key::Character(c) if c.as_str() == " ");
        if is_space {
            let idx = ring?;
            let row: SidebarRow = self.sidebar_items_for(tab).borrow().get(idx).cloned()?;
            let msg = row.action.activate.or(row.paste)?;
            return Some(self.update(msg));
        }

        let keyboard::Key::Named(named) = key else {
            return None;
        };
        use keyboard::key::Named;
        match named {
            // Tab walks the rows while the sidebar owns the keyboard
            // (ring engaged or cursor over it); otherwise the PTY
            // keeps its literal \t.
            Named::Tab => {
                if selected.is_none() && !self.cursor_over_sidebar() {
                    return None;
                }
                self.sidebar_nav_tab(tab, !modifiers.shift())
            }
            Named::ArrowUp | Named::ArrowDown => {
                if modifiers.shift() || len == 0 {
                    return None;
                }
                let forward = matches!(named, Named::ArrowDown);
                let cur = match selected {
                    Some(cur) => {
                        // Arrows only move from a ringed row; a focused
                        // input keeps its native caret/history keys.
                        ring?;
                        cur
                    }
                    // Not engaged: only the hover gate over a LIST tab
                    // (Snippets / History / Files / Hosts) turns a dead
                    // (already swallowed) arrow into an entry point;
                    // Chat / Host config need the hotkey or Tab.
                    None if self.cursor_over_sidebar()
                        && matches!(
                            tab,
                            TerminalSidebarTab::Snippets
                                | TerminalSidebarTab::History
                                | TerminalSidebarTab::Files
                                | TerminalSidebarTab::HostsTree
                        ) =>
                    {
                        // Land straight on the LIST body: first list row
                        // going down, last going up. Header chrome (path,
                        // search, sort, Close) stays reachable by walking
                        // on from there, but entry must never ring it: a
                        // ring popping up on the Files path row reads as a
                        // plain focused text input, not as navigation
                        // (live QA), and ringing Close would put Enter one
                        // keypress away from closing the sidebar.
                        let target = {
                            let items = self.sidebar_items_for(tab).borrow();
                            // A mouse-selected row (the Files tab's
                            // click-select) anchors the entry: the ring
                            // picks up where the mouse left off instead
                            // of starting at the list edge.
                            items.iter().position(|r| r.anchor).or_else(|| {
                                if forward {
                                    items.iter().position(|r| r.list)
                                } else {
                                    items.iter().rposition(|r| r.list)
                                }
                            })
                        };
                        if let Some(t) = target {
                            self.keynav.sidebar_selected = Some((tab, t));
                            self.close_files_path_edit();
                            return Some(Task::batch([
                                blur_task(),
                                self.sidebar_nav_scroll(tab, t),
                            ]));
                        }
                        // No list rows this frame (empty dir / group view):
                        // start on Close so the move below lands on the
                        // first body row going down / wraps to the last
                        // going up.
                        0
                    }
                    None => return None,
                };
                // Hop over input rows (panel contract): arrows are the
                // quick jump between actionable rows, Tab is the full
                // walk.
                let mut next = cur;
                for _ in 0..len {
                    next = index_move(len, Some(next), forward)?;
                    if !self.sidebar_row_is_input(tab, next) {
                        break;
                    }
                }
                if self.sidebar_row_is_input(tab, next) {
                    return Some(Task::none());
                }
                self.keynav.sidebar_selected = Some((tab, next));
                self.close_files_path_edit();
                Some(Task::batch([blur_task(), self.sidebar_nav_scroll(tab, next)]))
            }
            Named::Home | Named::End => {
                ring?;
                if len == 0 {
                    return Some(Task::none());
                }
                let mut idx = if matches!(named, Named::Home) { 0 } else { len - 1 };
                let step_forward = matches!(named, Named::Home);
                for _ in 0..len {
                    if !self.sidebar_row_is_input(tab, idx) {
                        break;
                    }
                    idx = index_move(len, Some(idx), step_forward)?;
                }
                if self.sidebar_row_is_input(tab, idx) {
                    return Some(Task::none());
                }
                self.keynav.sidebar_selected = Some((tab, idx));
                Some(self.sidebar_nav_scroll(tab, idx))
            }
            Named::Enter => {
                let idx = ring?;
                let row: SidebarRow = self.sidebar_items_for(tab).borrow().get(idx).cloned()?;
                // Shift+Enter = paste without the newline; plain Enter
                // = the row's primary (list rows RUN their command,
                // buttons/toggles/cards activate). Picker rows consume
                // as a no-op so the key can't leak into the PTY while
                // ringed.
                let msg = if modifiers.shift() {
                    row.paste.or(row.action.activate)
                } else {
                    row.action.activate.or(row.paste)
                };
                Some(match msg {
                    Some(msg) => self.update(msg),
                    None => Task::none(),
                })
            }
            Named::ArrowLeft | Named::ArrowRight => {
                let idx = ring?;
                let action = self.sidebar_items_for(tab).borrow().get(idx)?.action.clone();
                let rtl = crate::i18n::is_rtl_layout();
                let forward = matches!(named, Named::ArrowRight) != rtl;
                if action.prev.is_some() || action.next.is_some() {
                    // Picker row: cycle the value in place.
                    let msg = if forward { action.next } else { action.prev };
                    return Some(match msg {
                        Some(msg) => self.update(msg),
                        None => Task::none(),
                    });
                }
                // Non-picker row: move the ring along the recording,
                // hopping over inputs. Owner QA: the sort / search
                // header buttons sit side by side, so switching
                // between them must answer to the horizontal arrows
                // too, not only Up/Down.
                let mut next = idx;
                for _ in 0..len {
                    next = index_move(len, Some(next), forward)?;
                    if !self.sidebar_row_is_input(tab, next) {
                        break;
                    }
                }
                if self.sidebar_row_is_input(tab, next) {
                    return Some(Task::none());
                }
                self.keynav.sidebar_selected = Some((tab, next));
                self.close_files_path_edit();
                Some(Task::batch([blur_task(), self.sidebar_nav_scroll(tab, next)]))
            }
            // Shift+F10 is the same gesture for keyboards without a
            // dedicated Menu key, exactly as the vault router pairs them.
            Named::ContextMenu | Named::F10
                if *named == Named::ContextMenu || modifiers.shift() =>
            {
                // Keyboard half of the row's right-click. Anchored at
                // the ringed row's own rect (reported by
                // `sidebar_nav_slot`), never at a mouse the keyboard
                // user hasn't touched; the modality gate is armed so
                // the menu's default row shows its ring immediately,
                // exactly like `keynav_open_context_menu` does for
                // vault cards.
                let idx = ring?;
                let row = self.sidebar_items_for(tab).borrow().get(idx).cloned()?;
                let msg = row.menu?;
                self.keynav.modal.kbd.set(true);
                let rect = self.keynav.ring_bounds.get();
                if rect.width > 0.0 {
                    let x = if crate::i18n::is_rtl_layout() {
                        rect.x
                    } else {
                        rect.x + rect.width
                    };
                    self.keynav.menu_anchor = Some((x, rect.y + rect.height / 2.0));
                }
                Some(self.update(msg))
            }
            Named::Delete => {
                if let Some(idx) = ring {
                    let row = self.sidebar_items_for(tab).borrow().get(idx).cloned()?;
                    let msg = row.delete?;
                    // The recording shrinks next frame; the selection is
                    // clamped on the next key, so the ring lands on the
                    // neighbor instead of vanishing.
                    return Some(self.update(msg));
                }
                // Files: a click selects a row AND deliberately drops the
                // ring (`SidebarFilesSelectRow`), so a ring-less Del acts
                // on what the mouse selected: the whole multi-selection,
                // the select-then-Del pair the SFTP pane offers.
                let selected = self.sidebar_files_selected_entries(tab)?;
                let msg = if selected.len() == 1 {
                    let (path, is_dir) = selected.into_iter().next().expect("len checked");
                    SidebarFilesMessage::SidebarFilesDelete(path, is_dir)
                } else {
                    SidebarFilesMessage::SidebarFilesDeleteSelection(selected)
                };
                Some(self.update(Message::SidebarFiles(msg)))
            }
            Named::Escape => {
                // Esc is the "give me the terminal back" key: drop the
                // ring AND blur whatever sidebar input the walk focused
                // (the terminal never holds iced focus, so no focused
                // input means keys route to the PTY again). Also fires
                // with the cursor over the sidebar, where Esc was
                // previously swallowed by the hover gate and did
                // nothing at all.
                if selected.is_some() || self.cursor_over_sidebar() {
                    self.keynav.sidebar_selected = None;
                    // A half-typed Files edit (path / rename / new
                    // entry) cancels with the disengage (mirrors the
                    // SFTP pane's Esc).
                    if let Some(idx) = self.active_tab
                        && let Some(tab) = self.tabs.get_mut(idx)
                    {
                        let files = &mut tab.active_mut().files;
                        files.path_editing = None;
                        files.rename = None;
                        files.new_entry = None;
                        files.path_history_open = false;
                    }
                    return Some(blur_task());
                }
                None
            }
            _ => None,
        }
    }

    /// FocusSidebarList hotkey: bring the keyboard to the sidebar.
    /// Opens it when closed (landing on the tab it already shows);
    /// pressed again it cycles EVERY available tab across BOTH
    /// regions, left region first then right, in strip order (issue
    /// #102), opening the target's region as it lands there. Landing
    /// focuses the tab's natural entry point: Chat the message
    /// editor, History / Hosts their search field, the rest their
    /// first row. No-op outside a terminal tab.
    pub(crate) fn focus_sidebar_list(&mut self) -> Task<Message> {
        use crate::state::SidebarSide;
        let Some(idx) = self.active_tab else {
            return Task::none();
        };
        // The availability gates (AI toggle, SSH transport for Files /
        // Monitor / Tmux, feature toggles) live in
        // `sidebar_region_tabs`, exactly as the strips render them.
        let mut order = self.sidebar_region_tabs(SidebarSide::Left);
        order.extend(self.sidebar_region_tabs(SidebarSide::Right));
        if order.is_empty() {
            return Task::none();
        }

        // What the keyboard considers "current": the engaged ring's
        // tab, else the shown tab of an open region (right first, the
        // historical single-region bias), else the remembered tab the
        // first open will land on.
        let shown_left = self.effective_sidebar_tab(SidebarSide::Left);
        let shown_right = self.effective_sidebar_tab(SidebarSide::Right);
        let was_open = shown_left.is_some() || shown_right.is_some();
        let current = self
            .keynav
            .sidebar_selected
            .map(|(t, _)| t)
            .filter(|t| Some(*t) == shown_left || Some(*t) == shown_right)
            .or(shown_right)
            .or(shown_left)
            .or_else(|| {
                self.sidebar_region_tab(SidebarSide::Right)
                    .or_else(|| self.sidebar_region_tab(SidebarSide::Left))
            })
            .unwrap_or(TerminalSidebarTab::Snippets);
        // First press lands on what's already showing; repeats advance.
        let target = if was_open {
            let cur_pos = order.iter().position(|t| *t == current).unwrap_or(0);
            order[(cur_pos + 1) % order.len()]
        } else {
            current
        };
        // Open the target's region (a no-op when already open) and
        // make the target its shown tab. Targets come from
        // `sidebar_region_tabs`, so they always have a side.
        let Some(target_side) = self.prefs.sidebar_tab_side(target) else {
            return Task::none();
        };
        if let Some(tab) = self.tabs.get_mut(idx) {
            tab.sidebar_open[target_side.idx()] = true;
        }
        tracing::debug!(?current, ?target, was_open, "FocusSidebarList");
        self.set_sidebar_region_tab(target);
        match target {
            TerminalSidebarTab::Chat => {
                self.keynav.sidebar_selected = None;
                crate::widgets::focus_input(iced::widget::Id::new("chat-input"))
            }
            TerminalSidebarTab::History => {
                self.refresh_command_history();
                // Owner call: entering History goes straight to its
                // search field (real focus; Tab walks on from there).
                self.keynav.sidebar_selected = None;
                crate::widgets::focus_input(iced::widget::Id::new(
                    "sidebar-history-search",
                ))
            }
            TerminalSidebarTab::HostsTree => {
                // Same owner-approved pattern as History: land in the
                // tree's search field (real focus; Tab walks on from
                // there into the rows).
                self.keynav.sidebar_selected = None;
                crate::widgets::focus_input(iced::widget::Id::new("sidebar-hosts-search"))
            }
            TerminalSidebarTab::Files => {
                // Same first-body-row landing as Snippets/HostConfig,
                // batched with the mount / follow sync so the browser
                // is live (or catching up to the shell) by the time
                // the ring shows.
                self.keynav.sidebar_selected = Some((target, 1));
                Task::batch([self.sidebar_nav_scroll(target, 1), self.sidebar_files_sync()])
            }
            TerminalSidebarTab::Monitor => {
                // Gauges are informational; the only navigable row is the
                // opt-in button (when the host hasn't enabled monitoring
                // yet), which the body records first.
                self.keynav.sidebar_selected = Some((target, 1));
                self.sidebar_nav_scroll(target, 1)
            }
            TerminalSidebarTab::Tmux => {
                // Land on the first body row (Refresh) and list in the
                // same breath, so the rows are there by the time the
                // ring lands on them.
                self.keynav.sidebar_selected = Some((target, 1));
                Task::batch([self.sidebar_nav_scroll(target, 1), self.tmux_sync()])
            }
            TerminalSidebarTab::Snippets | TerminalSidebarTab::HostConfig => {
                // Land on the first row of the tab BODY. Index 0 is the
                // header's Close button (the strip records first, on
                // these tabs exactly one action; Chat's extra Reset
                // never applies here), and landing there would put
                // Enter one keypress away from closing the sidebar.
                // Next frame's recording; the slot only draws the ring
                // on non-input rows, and Enter/Tab dive into inputs.
                self.keynav.sidebar_selected = Some((target, 1));
                self.sidebar_nav_scroll(target, 1)
            }
        }
    }
}
