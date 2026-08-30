//! Settings -> Advanced section view: the debug-logging file toggle and
//! the environment report for GitHub issues.

use super::*;
use iced::widget::column;

impl Oryxis {
    pub(crate) fn view_settings_advanced(&self) -> Element<'_, Message> {
        // Keyboard rows are recorded in visual order.
        self.keynav_settings_reset();
        // ── Download mirror (China / blocked-network delivery) ──
        let mirror_section = self.download_mirror_section();
        // ── Debug logging ──
        let log_path = crate::logging::log_path()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "-".to_string());
        // Under `--debug-log` the sink is pinned on for the whole run, so
        // the row reports the flag instead of the usual description and
        // the toggle answers with the same sentence (see the handler).
        let forced = crate::logging::is_forced();
        let debug_desc = if forced { "debug_logging_forced" } else { "debug_logging_desc" };
        let debug_col = column![
            self.nav_toggle_row(
                t("debug_logging"),
                self.prefs.debug_logging || forced,
                Message::Settings(SettingsMessage::SettingToggleDebugLogging),
            ),
            Space::new().height(4),
            text(t(debug_desc)).size(11).color(OryxisColors::t().text_muted),
            Space::new().height(12),
            settings_row(t("debug_log_file"), log_path),
            Space::new().height(8),
            dir_row(vec![
                self.settings_nav_slot(
                    crate::keynav::RowAction::activate(Message::Settings(SettingsMessage::RevealDebugLog)),
                    6.0,
                    styled_button(
                        crate::i18n::open_in_file_manager_label(),
                        Message::Settings(SettingsMessage::RevealDebugLog),
                        OryxisColors::t().bg_selected,
                    ),
                ),
                Space::new().width(10).into(),
                self.settings_nav_slot(
                    crate::keynav::RowAction::activate(Message::Settings(SettingsMessage::ClearDebugLog)),
                    6.0,
                    styled_button(
                        t("debug_log_clear"),
                        Message::Settings(SettingsMessage::ClearDebugLog),
                        OryxisColors::t().bg_selected,
                    ),
                ),
            ]),
            // ── Performance HUD ──
            Space::new().height(16),
            self.nav_toggle_row(
                t("perf_overlay"),
                self.prefs.perf_overlay,
                Message::Settings(SettingsMessage::SettingTogglePerfOverlay),
            ),
            Space::new().height(4),
            text(t("perf_overlay_desc")).size(11).color(OryxisColors::t().text_muted),
        ];

        // ── Environment information ──
        // The report is rendered verbatim so the user sees exactly what
        // the Copy button puts on the clipboard, nothing hidden.
        let env_report = crate::logging::environment_report(self.renderer_active.as_ref());
        let report_block = container(
            text(env_report.clone())
                .size(11)
                .font(iced::Font::MONOSPACE)
                .color(OryxisColors::t().text_secondary),
        )
        .padding(12)
        .width(Length::Fill)
        .style(|_| container::Style {
            background: Some(Background::Color(OryxisColors::t().bg_selected)),
            border: Border { radius: Radius::from(6.0), ..Default::default() },
            ..Default::default()
        });
        // Key-derivation parameters (E1). Read-only: shows the vault's
        // tuned Argon2id profile, or that it uses the crate defaults.
        let kdf_line = match self.vault.as_ref().and_then(|v| v.kdf_params()) {
            Some(p) => t("kdf_params_label")
                .replacen("{mib}", &(p.m_kib / 1024).to_string(), 1)
                .replacen("{t}", &p.t.to_string(), 1),
            None => t("kdf_params_default").to_string(),
        };
        // Debug logging, the performance HUD and the environment
        // report are one diagnostics theme, so they share a card.
        let diagnostics_section = panel_section(debug_col.push(Space::new().height(16)).push(column![
            text(t("env_info")).size(13).color(OryxisColors::t().text_primary),
            Space::new().height(4),
            text(t("env_info_desc")).size(11).color(OryxisColors::t().text_muted),
            Space::new().height(10),
            text(kdf_line).size(11).color(OryxisColors::t().text_secondary),
            Space::new().height(10),
            report_block,
            Space::new().height(10),
            self.settings_nav_slot_labeled(
                t("copy_env_info"),
                crate::keynav::RowAction::activate(Message::CopyToClipboard(
                    env_report.clone(),
                )),
                6.0,
                styled_button(
                    t("copy_env_info"),
                    Message::CopyToClipboard(env_report),
                    OryxisColors::t().accent,
                ),
            ),
        ]));

        scrollable(
            container(
                column![
                    mirror_section,
                    Space::new().height(12),
                    diagnostics_section,
                    Space::new().height(24),
                ]
                .width(Length::Fill)
                .align_x(dir_align_x()),
            )
            .padding(Padding { top: 24.0, right: 24.0, bottom: 24.0, left: 24.0 }),
        )
        // Stable id so the keyboard router can keep the selected row
        // in view.
        .id(iced::widget::Id::new("settings-advanced-scroll"))
        .on_scroll(|v| Message::Settings(SettingsMessage::SectionScrolled(v.relative_offset().y)))
        .height(Length::Fill)
        .into()
    }

    /// The download-mirror block: picker (Auto / GitHub / Custom),
    /// and while Custom is selected a URL field plus a Test button
    /// running the reachability probe. Content integrity never
    /// depends on the mirror (sha256/Ed25519 gates), so the URL is
    /// user-configurable without a trust prompt.
    fn download_mirror_section(&self) -> Element<'_, Message> {
        let ui = &self.download_mirror;

        // `custom_pending` is the only case the choice can't answer:
        // the picker sits on Custom while the URL is still being typed
        // and nothing has been persisted yet.
        let selected_token = if ui.custom_pending {
            "custom"
        } else {
            ui.choice.token()
        };
        let display = |token: &String| {
            t(match token.as_str() {
                "github" => "download_mirror_github",
                "project" => "download_mirror_project",
                "custom" => "download_mirror_custom",
                _ => "download_mirror_auto",
            })
            .to_string()
        };
        let picker = self.nav_pick_row(
            t("download_mirror"),
            vec![
                "auto".into(),
                "github".into(),
                "project".into(),
                "custom".into(),
            ],
            selected_token.to_string(),
            display,
            220.0,
            |v| Message::Settings(SettingsMessage::DownloadMirrorPicked(v)),
        );

        let mut rows = column![
            picker,
            Space::new().height(4),
            text(t("download_mirror_desc")).size(11).color(OryxisColors::t().text_muted),
        ];

        // Test is offered by every mirror-first mode, since both have
        // an endpoint of their own to reach (`net_mirror::probe_target`
        // resolves which). Auto and GitHub-only dial GitHub first and
        // have nothing mirror-shaped to probe.
        let testable = matches!(selected_token, "custom" | "project");
        if testable {
            // Built where it is DRAWN, not hoisted: `settings_nav_slot`
            // records the keyboard row as a side effect of building, and
            // the ring walks in record order. Constructed before the URL
            // field, Test claimed the first slot while sitting last in
            // the row, so Tab went Test -> URL -> Save.
            let make_test_btn = |app: &Self| -> Element<'_, Message> {
                if ui.testing {
                    styled_button_opt(t("download_mirror_test_running"), None, OryxisColors::t().bg_selected)
                } else {
                    app.settings_nav_slot(
                        crate::keynav::RowAction::activate(Message::Settings(SettingsMessage::DownloadMirrorTest)),
                        6.0,
                        styled_button(
                            t("download_mirror_test"),
                            Message::Settings(SettingsMessage::DownloadMirrorTest),
                            OryxisColors::t().bg_selected,
                        ),
                    )
                }
            };
            if selected_token == "custom" {
                // Keyboard rows: the URL field (Enter commits), then
                // Save, then Test.
                let url_idx = self.settings_nav_record(crate::keynav::RowAction::input(
                    iced::widget::Id::new("set-download-mirror-url"),
                ));
                let url_field = self.settings_nav_ring_at(
                    url_idx,
                    10.0,
                    text_input(t("download_mirror_url_placeholder"), &ui.url_input)
                        .id(iced::widget::Id::new("set-download-mirror-url"))
                        .on_input(|v| Message::Settings(SettingsMessage::DownloadMirrorUrlEdited(v)))
                        .on_submit(Message::Settings(SettingsMessage::DownloadMirrorUrlCommitted))
                        .padding(10)
                        .width(360)
                        .style(crate::widgets::rounded_input_style)
                        .into(),
                );
                let save_btn = self.settings_nav_slot(
                    crate::keynav::RowAction::activate(Message::Settings(SettingsMessage::DownloadMirrorUrlCommitted)),
                    6.0,
                    styled_button(
                        t("save"),
                        Message::Settings(SettingsMessage::DownloadMirrorUrlCommitted),
                        OryxisColors::t().accent,
                    ),
                );
                let test_btn = make_test_btn(self);
                rows = rows
                    .push(Space::new().height(10))
                    .push(
                        dir_row(vec![
                            url_field,
                            Space::new().width(8).into(),
                            save_btn,
                            Space::new().width(8).into(),
                            test_btn,
                        ])
                        .align_y(iced::Alignment::Center),
                    );
                if ui.url_error {
                    rows = rows.push(Space::new().height(6)).push(
                        text(t("download_mirror_https_required"))
                            .size(11)
                            .color(OryxisColors::t().error),
                    );
                }
            } else {
                // The project mirror has no address to type: the host
                // is the app's own, so Test is the whole control.
                rows = rows
                    .push(Space::new().height(10))
                    .push(dir_row(vec![make_test_btn(self)]).align_y(iced::Alignment::Center));
            }
            match &ui.test_result {
                Some(Ok(ms)) => {
                    rows = rows.push(Space::new().height(6)).push(
                        text(format!("{} ({ms} ms)", t("download_mirror_test_ok")))
                            .size(11)
                            .color(OryxisColors::t().success),
                    );
                }
                Some(Err(cause)) => {
                    rows = rows.push(Space::new().height(6)).push(
                        text(format!("{}: {cause}", t("download_mirror_test_fail")))
                            .size(11)
                            .color(OryxisColors::t().error),
                    );
                }
                None => {}
            }
        }

        panel_section(rows)
    }
}
