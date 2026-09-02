//! Settings dispatch helpers: appearance. Tab / card / nav styling
//! arms (including the issue #79 tab-contrast family). Split out of
//! dispatch_settings/mod.rs.

use super::*;

impl Oryxis {
    /// Appearance arms: tab bar styling, host cards, nav rail and the
    /// status bar.
    pub(super) fn handle_settings_appearance(
        &mut self,
        message: SettingsMessage,
    ) -> Result<Task<Message>, SettingsMessage> {
        match message {
            SettingsMessage::FlattenHostsToggle => {
                self.flatten_hosts = !self.flatten_hosts;
                self.persist_setting(
                    "flatten_hosts",
                    if self.flatten_hosts { "true" } else { "false" },
                );
            }
            SettingsMessage::SettingToggleShowStatusBar => {
                self.prefs.show_status_bar = !self.prefs.show_status_bar;
                self.persist_setting(
                    "show_status_bar",
                    if self.prefs.show_status_bar { "true" } else { "false" },
                );
            }
            SettingsMessage::SettingToggleStatusVersion => {
                self.prefs.status_show_version = !self.prefs.status_show_version;
                self.persist_setting("status_show_version", if self.prefs.status_show_version { "true" } else { "false" });
            }
            SettingsMessage::SettingToggleStatusConnection => {
                self.prefs.status_show_connection = !self.prefs.status_show_connection;
                self.persist_setting("status_show_connection", if self.prefs.status_show_connection { "true" } else { "false" });
            }
            SettingsMessage::SettingToggleStatusLatency => {
                self.prefs.status_show_latency = !self.prefs.status_show_latency;
                self.persist_setting("status_show_latency", if self.prefs.status_show_latency { "true" } else { "false" });
            }
            SettingsMessage::SettingToggleStatusDimensions => {
                self.prefs.status_show_dimensions = !self.prefs.status_show_dimensions;
                self.persist_setting("status_show_dimensions", if self.prefs.status_show_dimensions { "true" } else { "false" });
            }
            SettingsMessage::SettingToggleStatusAlignLeft => {
                self.prefs.status_bar_align_left = !self.prefs.status_bar_align_left;
                self.persist_setting(
                    "status_bar_align_left",
                    if self.prefs.status_bar_align_left { "true" } else { "false" },
                );
            }
            SettingsMessage::SettingToggleStatusCwd => {
                self.prefs.status_show_cwd = !self.prefs.status_show_cwd;
                self.persist_setting("status_show_cwd", if self.prefs.status_show_cwd { "true" } else { "false" });
            }
            SettingsMessage::SettingToggleMonitorStatusBar => {
                self.prefs.monitor_status_bar = !self.prefs.monitor_status_bar;
                self.persist_setting(
                    "monitor_status_bar",
                    if self.prefs.monitor_status_bar { "true" } else { "false" },
                );
            }
            SettingsMessage::SettingMonitorIntervalChanged(v) => {
                // Digits only, same shaping as the other interval fields:
                // the value is validated on read, not on every keystroke.
                // Cap at an hour: anything longer is indistinguishable
                // from turning the tab off, and the field stays narrow.
                self.prefs.monitor_interval = crate::util::sanitize_uint(&v, 3_600);
                self.persist_setting(
                    "monitor_interval_seconds",
                    &self.prefs.monitor_interval.clone(),
                );
            }
            SettingsMessage::SettingToggleHostMonitoring => {
                self.prefs.host_monitoring = !self.prefs.host_monitoring;
                self.persist_setting(
                    "host_monitoring_enabled",
                    if self.prefs.host_monitoring { "true" } else { "false" },
                );
                if self.prefs.host_monitoring {
                    // First enable ever: seed the internal defaults ON so
                    // the feature shows immediate value instead of being
                    // "enabled but invisible". Guarded by a marker so a
                    // later off/on never clobbers the user's own choices.
                    if !self.prefs.host_monitoring_seeded {
                        self.prefs.host_monitoring_seeded = true;
                        self.persist_setting("host_monitoring_seeded", "true");
                        self.prefs.monitor_status_bar = true;
                        self.persist_setting("monitor_status_bar", "true");
                    }
                } else {
                    // Turning the feature off stops all probing and drops
                    // the in-memory samples; nothing monitoring-related
                    // renders anymore, so a stale ring would only leak.
                    // `monitor_reset_all` also bumps the stamp so a probe
                    // still in flight can't repopulate (or toast) after
                    // the toggle.
                    self.monitor_reset_all();
                    self.monitor_ports_open = false;
                    self.monitor_disks_open = true;
                }
            }
            SettingsMessage::SettingToggleConnectionReuse => {
                self.prefs.ssh_connection_reuse = !self.prefs.ssh_connection_reuse;
                self.persist_setting(
                    "ssh_connection_reuse",
                    if self.prefs.ssh_connection_reuse { "true" } else { "false" },
                );
                if !self.prefs.ssh_connection_reuse {
                    // Stop offering the pooled connections. Live
                    // sessions already sharing one are untouched: the
                    // setting governs what the NEXT tab does, and
                    // tearing down working tabs to honour a preference
                    // change would be the opposite of what it asks for.
                    self.ssh_transport_pool.clear();
                }
            }
            SettingsMessage::SettingToggleTmuxManager => {
                self.prefs.tmux_manager = !self.prefs.tmux_manager;
                self.persist_setting(
                    "tmux_manager_enabled",
                    if self.prefs.tmux_manager { "true" } else { "false" },
                );
                if !self.prefs.tmux_manager {
                    // Off hides every surface the feature owns, so a
                    // kept listing could only leak. Nothing here needs a
                    // stamp: `tmux_session_for_pane` re-reads the toggle,
                    // so a listing in flight lands on a pane whose entry
                    // is gone and is dropped by `Listed`.
                    self.tmux_reset_all();
                    self.hover.tmux_row = None;
                }
            }
            SettingsMessage::SettingToggleMonitorAllHosts => {
                self.prefs.monitor_all_hosts = !self.prefs.monitor_all_hosts;
                self.persist_setting(
                    "monitor_all_hosts",
                    if self.prefs.monitor_all_hosts { "true" } else { "false" },
                );
            }
            SettingsMessage::SettingToggleMonitorDashLiveOnly => {
                self.prefs.monitor_dash_live_only = !self.prefs.monitor_dash_live_only;
                self.persist_setting(
                    "monitor_dash_live_only",
                    if self.prefs.monitor_dash_live_only { "true" } else { "false" },
                );
                // Turning it on closes what the dashboard dialled for
                // itself, here rather than on the next heartbeat: that
                // heartbeat only runs while the fleet is on screen, and
                // this switch is thrown from Settings, so waiting for it
                // would leave the connections the user just asked us to
                // stop opening alive behind another view. The sweep is
                // the same one the feature toggle and the vault lock
                // use, so a dial already in flight lands on a dead
                // stamp instead of reopening what it just closed; the
                // links riding a tab's session cost nothing and are
                // rebuilt on the next entry into the view.
                if self.prefs.monitor_dash_live_only {
                    self.monitor_dash.sweep();
                }
            }
            SettingsMessage::SidebarTabSideChanged(tab, placement) => {
                if let Some(placement) = crate::state::SidebarPlacement::from_code(&placement) {
                    // Resolved BEFORE the write (the resolver applies the
                    // per-tab defaults a raw map read would miss), so the
                    // move handler knows which side the tab actually left.
                    let came_from = self.prefs.sidebar_tab_side(tab);
                    self.prefs.sidebar_tab_sides.insert(tab, placement);
                    let encoded = self.prefs.encode_sidebar_tab_sides();
                    self.persist_setting("sidebar_tab_sides", &encoded);
                    self.sidebar_tab_moved(tab, came_from, placement);
                }
            }
            SettingsMessage::SettingToggleSidebarAutoOpen => {
                self.prefs.sidebar_auto_open = !self.prefs.sidebar_auto_open;
                self.persist_setting(
                    "sidebar_auto_open",
                    if self.prefs.sidebar_auto_open { "true" } else { "false" },
                );
            }
            SettingsMessage::CycleHostViewMode => {
                // Dismiss the `…` overflow menu when cycled from there
                // (no-op for the inline toolbar button).
                self.overlay = None;
                self.prefs.host_view_mode = self.prefs.host_view_mode.next();
                self.persist_setting("host_view_mode", self.prefs.host_view_mode.code());
                // Tree mode shows every level in place, so a pending
                // drill-down cursor would only confuse the walk once
                // the user cycles back to grid/list from inside it.
                if self.prefs.host_view_mode == crate::state::HostViewMode::Tree {
                    self.active_group = None;
                }
            }
            SettingsMessage::ToggleCardAccentGlass => {
                self.prefs.card_accent_glass = !self.prefs.card_accent_glass;
                self.persist_setting(
                    "card_accent_glass",
                    if self.prefs.card_accent_glass { "true" } else { "false" },
                );
            }
            SettingsMessage::ToggleShowHostAddress => {
                self.prefs.show_host_address = !self.prefs.show_host_address;
                self.persist_setting(
                    "show_host_address",
                    if self.prefs.show_host_address { "true" } else { "false" },
                );
            }
            SettingsMessage::ToggleShowTabHostAddress => {
                self.prefs.show_tab_host_address = !self.prefs.show_tab_host_address;
                self.persist_setting(
                    "show_tab_host_address",
                    if self.prefs.show_tab_host_address { "true" } else { "false" },
                );
            }
            SettingsMessage::SettingToggleShowTabStatusDot => {
                self.prefs.show_tab_status_dot = !self.prefs.show_tab_status_dot;
                self.persist_setting(
                    "show_tab_status_dot",
                    if self.prefs.show_tab_status_dot { "true" } else { "false" },
                );
            }
            SettingsMessage::SettingToggleTabAccentLine => {
                self.prefs.tab_accent_line = !self.prefs.tab_accent_line;
                self.persist_setting(
                    "tab_accent_line",
                    if self.prefs.tab_accent_line { "true" } else { "false" },
                );
            }
            SettingsMessage::SettingToggleTabAccentWash => {
                self.prefs.tab_accent_wash = !self.prefs.tab_accent_wash;
                self.persist_setting(
                    "tab_accent_wash",
                    if self.prefs.tab_accent_wash { "true" } else { "false" },
                );
            }
            SettingsMessage::SettingToggleTabAccentText => {
                self.prefs.tab_accent_text = !self.prefs.tab_accent_text;
                self.persist_setting(
                    "tab_accent_text",
                    if self.prefs.tab_accent_text { "true" } else { "false" },
                );
            }
            SettingsMessage::SettingNavOrientationChanged(val) => {
                let normalized = match val.as_str() {
                    "vertical" => "vertical",
                    _ => "horizontal",
                };
                self.prefs.nav_orientation = normalized.into();
                self.persist_setting("nav_orientation", normalized);
            }
            SettingsMessage::ToggleNavRailExpanded => {
                self.prefs.nav_rail_expanded = !self.prefs.nav_rail_expanded;
                self.persist_setting(
                    "nav_rail_expanded",
                    if self.prefs.nav_rail_expanded { "true" } else { "false" },
                );
            }
            SettingsMessage::SettingDefaultHostIconChanged(val) => {
                let normalized = match val.as_str() {
                    "square" => "square",
                    "rounded" => "rounded",
                    "outline" => "outline",
                    "initials" => "initials",
                    _ => "circular",
                };
                self.prefs.default_host_icon = normalized.into();
                self.persist_setting("default_host_icon", normalized);
            }
            SettingsMessage::SettingTabCloseButtonSideChanged(val) => {
                // Only accept the two known values; anything else
                // collapses to the default so an unknown pick from a
                // future build can't wedge the tab bar.
                let normalized = match val.as_str() {
                    "right" => "right",
                    _ => "left",
                };
                self.prefs.tab_close_button_side = normalized.into();
                self.persist_setting("tab_close_button_side", normalized);
            }
            SettingsMessage::SettingPinnedTabStyleChanged(val) => {
                let normalized = match val.as_str() {
                    "full" => "full",
                    _ => "compact",
                };
                self.prefs.pinned_tab_style = normalized.into();
                self.persist_setting("pinned_tab_style", normalized);
            }
            SettingsMessage::SettingDuplicateTabPositionChanged(val) => {
                let normalized = match val.as_str() {
                    "end" => "end",
                    "start" => "start",
                    _ => "next",
                };
                self.prefs.duplicate_tab_position = normalized.into();
                self.persist_setting("duplicate_tab_position", normalized);
            }
            SettingsMessage::SettingTabNumberStyleChanged(val) => {
                let normalized = match val.as_str() {
                    "prefix" => "prefix",
                    "icon" => "icon",
                    _ => "off",
                };
                self.prefs.tab_number_style = normalized.into();
                self.persist_setting("tab_number_style", normalized);
            }
            SettingsMessage::SettingToggleTabSlotIncludesHome => {
                self.prefs.tab_slot_includes_home = !self.prefs.tab_slot_includes_home;
                self.persist_setting(
                    "tab_slot_includes_home",
                    if self.prefs.tab_slot_includes_home { "true" } else { "false" },
                );
            }
            SettingsMessage::SettingTabFillStyleChanged(val) => {
                let normalized = match val.as_str() {
                    "solid" => "solid",
                    _ => "gradient",
                };
                self.prefs.tab_fill_style = normalized.into();
                self.persist_setting("tab_fill_style", normalized);
            }
            SettingsMessage::SettingTabAccentColorChanged(val) => {
                let normalized = match val.as_str() {
                    "app" => "app",
                    _ => "host",
                };
                self.prefs.tab_accent_color = normalized.into();
                self.persist_setting("tab_accent_color", normalized);
            }
            SettingsMessage::SettingTabBarPositionChanged(val) => {
                let normalized = match val.as_str() {
                    "bottom" => "bottom",
                    "left" => "left",
                    "right" => "right",
                    _ => "top",
                };
                // The active-tab gradient direction lives in a process-wide
                // gate (read by `active_tab_bg` at render time, same shape
                // as the auto-title gate) so the "lit from the frame" fade
                // can flip without threading a flag through every tab
                // renderer.
                crate::views::tab_bar::set_tab_bar_pos(
                    crate::views::tab_bar::TabBarPos::from_setting(normalized),
                );
                self.prefs.tab_bar_position = normalized.into();
                self.persist_setting("tab_bar_position", normalized);
            }
            SettingsMessage::SettingInactiveTabStyleChanged(val) => {
                let normalized = match val.as_str() {
                    "border" => "border",
                    "underline" => "underline",
                    _ => "none",
                };
                // Same process-wide gate shape as `tab_bar_position`: the
                // tab renderer reads it at draw time (issue #87).
                crate::views::tab_bar::set_inactive_tab_style(
                    crate::views::tab_bar::InactiveTabStyle::from_setting(normalized),
                );
                self.prefs.inactive_tab_style = normalized.into();
                self.persist_setting("inactive_tab_style", normalized);
            }
            SettingsMessage::SettingTabWidthModeChanged(val) => {
                let normalized = if val == "uniform" { "uniform" } else { "adaptive" };
                self.prefs.tab_width_mode = normalized.into();
                self.persist_setting("tab_width_mode", normalized);
            }
            SettingsMessage::SettingTabUniformSizeChanged(val) => {
                let normalized = match val.as_str() {
                    "small" => "small",
                    "large" => "large",
                    _ => "medium",
                };
                self.prefs.tab_uniform_size = normalized.into();
                self.persist_setting("tab_uniform_size", normalized);
            }
            SettingsMessage::SettingTogglePinnedTabsTopBar => {
                self.prefs.pinned_tabs_top_bar = !self.prefs.pinned_tabs_top_bar;
                self.persist_setting(
                    "pinned_tabs_top_bar",
                    if self.prefs.pinned_tabs_top_bar { "true" } else { "false" },
                );
            }
            SettingsMessage::SettingToggleSideHideTopBar => {
                self.prefs.side_hide_top_bar = !self.prefs.side_hide_top_bar;
                self.persist_setting(
                    "side_hide_top_bar",
                    if self.prefs.side_hide_top_bar { "true" } else { "false" },
                );
            }
            SettingsMessage::SettingToggleSideFullHeight => {
                self.prefs.side_full_height = !self.prefs.side_full_height;
                self.persist_setting(
                    "side_full_height",
                    if self.prefs.side_full_height { "true" } else { "false" },
                );
            }
            m => return Err(m),
        }
        Ok(Task::none())
    }
}
