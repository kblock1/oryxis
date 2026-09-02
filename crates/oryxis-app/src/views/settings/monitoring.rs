//! Settings -> Monitoring section view (issue #83). Consolidates the
//! host-monitoring config that used to be scattered (interval in
//! Connection, status-bar toggle in Interface) plus the new "Enable for
//! all hosts" control. Only listed while the monitoring feature is
//! enabled (Features & Plugins), so the whole section appears/disappears
//! with the master toggle, mirroring the SFTP section.

use super::*;
use iced::widget::column;

impl Oryxis {
    pub(crate) fn view_settings_monitoring(&self) -> Element<'_, Message> {
        // Keyboard rows recorded in visual order.
        self.keynav_settings_reset();

        // "Enable for all hosts": when on, every host with a live session
        // is monitored and the per-host editor toggle locks on.
        let all_hosts_section = panel_section(column![
            self.nav_toggle_row(
                t("monitor_all_hosts"),
                self.prefs.monitor_all_hosts,
                Message::Settings(SettingsMessage::SettingToggleMonitorAllHosts),
            ),
            Space::new().height(4),
            text(t("monitor_all_hosts_desc"))
                .size(11)
                .color(OryxisColors::t().text_muted),
        ]);

        // "Only hosts with a live session" (issue #197): sits right
        // under "Enable for all hosts" because it answers the question
        // that toggle raises, which of the hosts it swept in the fleet
        // view should actually be reached.
        let live_only_section = panel_section(column![
            self.nav_toggle_row(
                t("monitor_dash_live_only"),
                self.prefs.monitor_dash_live_only,
                Message::Settings(SettingsMessage::SettingToggleMonitorDashLiveOnly),
            ),
            Space::new().height(4),
            text(t("monitor_dash_live_only_desc"))
                .size(11)
                .color(OryxisColors::t().text_muted),
        ]);

        // Probe interval (moved here from Connection).
        let interval_section = panel_section(column![
            text(t("monitor_interval")).size(13).color(OryxisColors::t().text_primary),
            Space::new().height(4),
            text(t("monitor_interval_desc"))
                .size(11)
                .color(OryxisColors::t().text_muted),
            Space::new().height(8),
            self.settings_nav_slot_labeled(
                t("monitor_interval"),
                crate::keynav::RowAction::input(iced::widget::Id::new("set-monitor-interval")),
                10.0,
                text_input("5", &self.prefs.monitor_interval)
                    .id(iced::widget::Id::new("set-monitor-interval"))
                    .on_input(|v| Message::Settings(SettingsMessage::SettingMonitorIntervalChanged(v)))
                    .padding(10)
                    .width(240)
                    .style(crate::widgets::rounded_input_style)
                    .align_x(dir_align_x())
                    .into(),
            ),
        ]);

        // Status-bar segment (moved here from Interface).
        let status_bar_section = panel_section(column![
            self.nav_toggle_row(
                t("monitor_status_bar"),
                self.prefs.monitor_status_bar,
                Message::Settings(SettingsMessage::SettingToggleMonitorStatusBar),
            ),
            Space::new().height(4),
            text(t("monitor_status_bar_desc"))
                .size(11)
                .color(OryxisColors::t().text_muted),
        ]);

        let content = column![
            all_hosts_section,
            Space::new().height(12),
            live_only_section,
            Space::new().height(12),
            interval_section,
            Space::new().height(12),
            status_bar_section,
            Space::new().height(24),
        ]
        .width(Length::Fill)
        .align_x(dir_align_x());

        scrollable(
            container(content)
                .padding(Padding { top: 24.0, right: 24.0, bottom: 24.0, left: 24.0 }),
        )
        .id(iced::widget::Id::new("settings-monitoring-scroll"))
        .on_scroll(|v| Message::Settings(SettingsMessage::SectionScrolled(v.relative_offset().y)))
        .height(Length::Fill)
        .into()
    }
}
