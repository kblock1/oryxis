//! Settings -> MCP Server section view: the server enable toggle, the
//! setup guide button, and the expandable info panel with the config
//! snippet, token management and the vault-password embed flow.

use super::*;
use iced::widget::column;

use crate::mcp::{mcp_config_json_display, token_mask};

impl Oryxis {
    /// Standalone MCP Server settings section. Was nested inside the
    /// Security panel in 0.6 when MCP shipped with the installer; in
    /// 0.7 it lives in its own Settings sidebar entry because the
    /// plugin distribution + setup-guide affordances deserve room
    /// without competing with the Security toggles.
    pub(super) fn view_settings_mcp(&self) -> Element<'_, Message> {
        // Keyboard rows are recorded in visual order: the guide button
        // is defined here but only recorded (slot-wrapped) at its use
        // site below, after the server toggle.
        self.keynav_settings_reset();
        // The MCP plugin is managed (installed / updated) from the
        // Plugins screen; the server's own on/off lives here.
        let mcp_guide_btn = button(
            container(text(crate::i18n::t("mcp_setup_guide")).size(12).color(OryxisColors::t().accent))
                .padding(Padding { top: 6.0, right: 16.0, bottom: 6.0, left: 16.0 }),
        )
        .on_press(if self.mcp.show_info { Message::Mcp(McpMessage::HideMcpInfo) } else { Message::Mcp(McpMessage::ShowMcpInfo) })
        .style(|_, status| {
            let bg = match status {
                BtnStatus::Hovered => Color { a: 0.1, ..OryxisColors::t().accent },
                _ => Color::TRANSPARENT,
            };
            button::Style {
                background: Some(Background::Color(bg)),
                border: Border { radius: Radius::from(6.0), color: OryxisColors::t().accent, width: 1.0 },
                ..Default::default()
            }
        });
        let mut mcp_col = column![
            self.nav_toggle_row(
                crate::i18n::t("mcp_server"),
                self.mcp.server_enabled,
                Message::Mcp(McpMessage::ToggleMcpServer),
            ),
            Space::new().height(12),
            dir_row(vec![
                text(crate::i18n::t("mcp_server_desc")).size(11).color(OryxisColors::t().text_muted).into(),
                Space::new().width(Length::Fill).into(),
                self.settings_nav_slot_labeled(
                    crate::i18n::t("mcp_setup_guide"),
                    crate::keynav::RowAction::activate(if self.mcp.show_info {
                        Message::Mcp(McpMessage::HideMcpInfo)
                    } else {
                        Message::Mcp(McpMessage::ShowMcpInfo)
                    }),
                    6.0,
                    mcp_guide_btn.into(),
                ),
            ]).align_y(iced::Alignment::Center),
        ];
        if self.mcp.show_info {
            mcp_col = mcp_col
                .push(Space::new().height(12))
                .push(mcp_info_panel(self));
        }

        scrollable(
            container(
                column![
                    panel_section(mcp_col),
                    Space::new().height(24),
                ]
                .width(Length::Fill)
                .align_x(dir_align_x()),
            )
            .padding(Padding { top: 24.0, right: 24.0, bottom: 24.0, left: 24.0 }),
        )
        // Stable id so the keyboard router can keep the selected row
        // in view.
        .id(iced::widget::Id::new("settings-mcp-scroll"))
        .on_scroll(|v| Message::Settings(SettingsMessage::SectionScrolled(v.relative_offset().y)))
        .height(Length::Fill)
        .into()
    }
}

/// Config file path hint for the native client per platform. Claude
/// Code reads MCP servers from `~/.claude.json` (user scope) or a
/// project-root `.mcp.json`; it explicitly does NOT read files inside
/// `~/.claude/`. Claude Desktop uses `claude_desktop_config.json`. The
/// WSL target has its own hint, built inline in the info panel, so
/// this no longer needs to mention WSL on Windows.
fn mcp_config_path() -> &'static str {
    if cfg!(target_os = "windows") {
        "~/.claude.json (Claude Code)  or  %APPDATA%\\Claude\\claude_desktop_config.json"
    } else if cfg!(target_os = "macos") {
        "~/.claude.json (Claude Code)  or  ~/Library/Application Support/Claude/claude_desktop_config.json"
    } else {
        "~/.claude.json (Claude Code)"
    }
}

/// Monospaced code block widget.
fn code_block<'a>(content: &str) -> Element<'a, Message> {
    container(
        // `selectable(true)` lets the user drag-highlight the snippet
        // and copy it with Ctrl+C, instead of being forced through the
        // Copy button.
        text(content.to_owned()).size(12).selectable(true).color(OryxisColors::t().text_primary),
    )
    .padding(12)
    .width(Length::Fill)
    .style(|_| container::Style {
        background: Some(Background::Color(OryxisColors::t().bg_primary)),
        border: Border { radius: Radius::from(6.0), color: OryxisColors::t().border, width: 1.0 },
        ..Default::default()
    })
    .into()
}

/// The expandable MCP info panel shown inside the MCP Server settings.
/// Takes the app so every interactive control can record a settings
/// keynav slot, which also means construction below strictly follows
/// visual order.
fn mcp_info_panel(app: &crate::app::Oryxis) -> Element<'_, Message> {
    let copied = app.mcp.config_copied;
    let install_status = &app.mcp.install_status;
    let token: &str = &app.mcp.server_token;
    let token_visible = app.mcp.token_visible;
    let target_wsl = app.mcp.target_wsl;
    let vault_pw = app.mcp_vault_pw();

    // `target_wsl` switches the snippet (and the Copy / Install button
    // targets, handled in dispatch) between the native client and a
    // Claude Code / Cursor running inside WSL. The toggle that flips it
    // is Windows-only, so on other platforms this stays false.
    // The snippet obeys the SAME visibility the token row does: it
    // spells out the token and, when embedded, the vault password, so
    // revealing one and hiding the other would be a mask in name only.
    // Copy / Install rebuild the JSON from state and always carry the
    // real values.
    let json_text = mcp_config_json_display(token, vault_pw.as_deref(), target_wsl, token_visible);
    let path_hint: &str = if target_wsl {
        "~/.claude.json (WSL)"
    } else {
        mcp_config_path()
    };

    let copy_label = if copied {
        crate::i18n::t("mcp_copied")
    } else {
        crate::i18n::t("mcp_info_copy")
    };
    let copy_color = if copied { OryxisColors::t().success } else { OryxisColors::t().accent };

    let copy_btn = button(
        container(text(copy_label).size(12).color(copy_color))
            .padding(Padding { top: 6.0, right: 16.0, bottom: 6.0, left: 16.0 }),
    )
    .on_press(Message::Mcp(McpMessage::CopyMcpConfig))
    .style(move |_, status| {
        let bg = match status {
            BtnStatus::Hovered => Color { a: 0.1, ..copy_color },
            _ => Color::TRANSPARENT,
        };
        button::Style {
            background: Some(Background::Color(bg)),
            border: Border { radius: Radius::from(6.0), color: copy_color, width: 1.0 },
            ..Default::default()
        }
    });

    let (install_label, install_color) = match install_status {
        Some(Ok(_)) => (crate::i18n::t("mcp_installed"), OryxisColors::t().success),
        Some(Err(_)) => (crate::i18n::t("mcp_install_failed"), OryxisColors::t().error),
        None => (crate::i18n::t("mcp_install_claude"), OryxisColors::t().success),
    };
    let install_btn = button(
        container(text(install_label).size(12).color(install_color))
            .padding(Padding { top: 6.0, right: 16.0, bottom: 6.0, left: 16.0 }),
    )
    .on_press(Message::Mcp(McpMessage::InstallMcpConfig))
    .style(move |_, status| {
        let bg = match status {
            BtnStatus::Hovered => Color { a: 0.1, ..install_color },
            _ => Color::TRANSPARENT,
        };
        button::Style {
            background: Some(Background::Color(bg)),
            border: Border { radius: Radius::from(6.0), color: install_color, width: 1.0 },
            ..Default::default()
        }
    });

    let close_btn = button(
        container(text(crate::i18n::t("mcp_info_close")).size(12).color(OryxisColors::t().text_muted))
            .padding(Padding { top: 6.0, right: 16.0, bottom: 6.0, left: 16.0 }),
    )
    .on_press(Message::Mcp(McpMessage::HideMcpInfo))
    .style(|_, status| {
        let bg = match status {
            BtnStatus::Hovered => OryxisColors::t().bg_hover,
            _ => Color::TRANSPARENT,
        };
        button::Style {
            background: Some(Background::Color(bg)),
            border: Border { radius: Radius::from(6.0), color: OryxisColors::t().border, width: 1.0 },
            ..Default::default()
        }
    });

    // Token row: shows the active MCP token (masked by default), with
    // show/hide, copy, and regenerate affordances. Show/Hide governs
    // every secret on the panel, this row and the snippet below, which
    // is where the token and the opt-in vault password are spelled out
    // again.
    let token_display: String = if token.is_empty() {
        crate::i18n::t("mcp_token_unset").to_string()
    } else if token_visible {
        token.to_string()
    } else {
        token_mask(token)
    };
    let token_color = if token.is_empty() {
        OryxisColors::t().warning
    } else {
        OryxisColors::t().text_primary
    };
    let toggle_label = if token_visible {
        crate::i18n::t("mcp_token_hide")
    } else {
        crate::i18n::t("mcp_token_show")
    };

    fn token_action_btn<'a>(
        label: &'a str,
        color: Color,
        msg: Message,
    ) -> Element<'a, Message> {
        button(
            container(text(label).size(11).color(color))
                .padding(Padding { top: 4.0, right: 10.0, bottom: 4.0, left: 10.0 }),
        )
        .on_press(msg)
        .style(move |_, status| {
            let bg = match status {
                BtnStatus::Hovered => Color { a: 0.12, ..color },
                _ => Color::TRANSPARENT,
            };
            button::Style {
                background: Some(Background::Color(bg)),
                border: Border { radius: Radius::from(6.0), color, width: 1.0 },
                ..Default::default()
            }
        })
        .into()
    }

    let mut info_col = column![
        text(crate::i18n::t("mcp_info_title")).size(14).color(OryxisColors::t().text_primary),
        Space::new().height(8),
        text(crate::i18n::t("mcp_info_desc")).size(12).color(OryxisColors::t().text_secondary),
    ];

    // Target toggle (Native / WSL): only relevant on Windows, where the
    // binary is an `.exe` a WSL-resident client reaches via `/mnt/c`.
    // On other platforms there is a single target, so the toggle is
    // omitted and `target_wsl` stays false.
    #[cfg(target_os = "windows")]
    {
        fn target_btn<'a>(label: &'a str, selected: bool, msg: Message) -> Element<'a, Message> {
            let text_color = if selected {
                OryxisColors::t().bg_primary
            } else {
                OryxisColors::t().text_secondary
            };
            button(
                container(text(label).size(11).color(text_color))
                    .padding(Padding { top: 4.0, right: 14.0, bottom: 4.0, left: 14.0 }),
            )
            .on_press(msg)
            .style(move |_, status| {
                let bg = if selected {
                    OryxisColors::t().accent
                } else if matches!(status, BtnStatus::Hovered) {
                    OryxisColors::t().bg_hover
                } else {
                    Color::TRANSPARENT
                };
                button::Style {
                    background: Some(Background::Color(bg)),
                    border: Border { radius: Radius::from(6.0), color: OryxisColors::t().border, width: 1.0 },
                    ..Default::default()
                }
            })
            .into()
        }

        let target_row = crate::widgets::dir_row(vec![
            text(crate::i18n::t("mcp_target_label"))
                .size(11)
                .color(OryxisColors::t().text_muted)
                .into(),
            Space::new().width(8).into(),
            app.settings_nav_slot(
                crate::keynav::RowAction::activate(Message::Mcp(McpMessage::SetMcpTarget(false))),
                6.0,
                target_btn(crate::i18n::t("mcp_target_native"), !target_wsl, Message::Mcp(McpMessage::SetMcpTarget(false))),
            ),
            Space::new().width(6).into(),
            app.settings_nav_slot(
                crate::keynav::RowAction::activate(Message::Mcp(McpMessage::SetMcpTarget(true))),
                6.0,
                target_btn(crate::i18n::t("mcp_target_wsl"), target_wsl, Message::Mcp(McpMessage::SetMcpTarget(true))),
            ),
        ])
        .align_y(iced::Alignment::Center);

        info_col = info_col.push(Space::new().height(12)).push(target_row);
    }

    // Token row, built after the target row so the keynav slots the
    // action buttons record land in visual order.
    let mut token_items: Vec<Element<'_, Message>> = vec![
        text(crate::i18n::t("mcp_token_label"))
            .size(11)
            .color(OryxisColors::t().text_muted)
            .into(),
        Space::new().width(8).into(),
        container(
            text(token_display)
                .size(11)
                .selectable(true)
                .font(iced::Font::MONOSPACE)
                .color(token_color),
        )
        .padding(Padding { top: 4.0, right: 8.0, bottom: 4.0, left: 8.0 })
        .style(|_| container::Style {
            background: Some(Background::Color(OryxisColors::t().bg_primary)),
            border: Border { radius: Radius::from(4.0), ..Default::default() },
            ..Default::default()
        })
        .into(),
    ];
    // The reveal is offered whenever the panel holds a secret at all: a
    // vault whose auth token is unset can still carry an embedded master
    // password in the snippet, and a masked value with no way back would
    // be worse than showing it.
    if !token.is_empty() || vault_pw.is_some() {
        token_items.push(Space::new().width(8).into());
        token_items.push(app.settings_nav_slot(
            crate::keynav::RowAction::activate(Message::Mcp(McpMessage::ToggleMcpTokenVisibility)),
            6.0,
            token_action_btn(
                toggle_label,
                OryxisColors::t().text_secondary,
                Message::Mcp(McpMessage::ToggleMcpTokenVisibility),
            ),
        ));
    }
    if !token.is_empty() {
        token_items.push(Space::new().width(6).into());
        token_items.push(app.settings_nav_slot(
            crate::keynav::RowAction::activate(Message::Mcp(McpMessage::CopyMcpToken)),
            6.0,
            token_action_btn(
                crate::i18n::t("mcp_token_copy"),
                OryxisColors::t().accent,
                Message::Mcp(McpMessage::CopyMcpToken),
            ),
        ));
    }
    token_items.push(Space::new().width(6).into());
    token_items.push(app.settings_nav_slot_labeled(
        crate::i18n::t("mcp_token_regenerate"),
        crate::keynav::RowAction::activate(Message::Mcp(McpMessage::RegenerateMcpToken)),
        6.0,
        token_action_btn(
            crate::i18n::t("mcp_token_regenerate"),
            OryxisColors::t().warning,
            Message::Mcp(McpMessage::RegenerateMcpToken),
        ),
    ));
    let token_row = crate::widgets::dir_row(token_items)
        .align_y(iced::Alignment::Center);

    info_col = info_col
        .push(Space::new().height(12))
        .push(token_row)
        .push(Space::new().height(4))
        .push(
            text(crate::i18n::t("mcp_token_desc"))
                .size(10)
                .color(OryxisColors::t().text_muted),
        )
        .push(Space::new().height(12))
        .push(code_block(&json_text))
        .push(Space::new().height(8))
        .push(
            text(format!("{} {}", crate::i18n::t("mcp_info_path_label"), path_hint))
                .size(11)
                .color(OryxisColors::t().text_muted),
        );

    // Explain that the WSL snippet targets a client living inside the
    // distro, shown only while that target is selected.
    #[cfg(target_os = "windows")]
    if target_wsl {
        info_col = info_col
            .push(Space::new().height(8))
            .push(
                text(crate::i18n::t("mcp_info_note_wsl"))
                    .size(11)
                    .color(OryxisColors::t().warning),
            );
    }

    if let Some(Err(e)) = install_status {
        info_col = info_col
            .push(Space::new().height(4))
            .push(text(e.clone()).size(11).color(OryxisColors::t().error));
    } else if let Some(Ok(path)) = install_status {
        info_col = info_col
            .push(Space::new().height(4))
            .push(text(format!("{} {path}", crate::i18n::t("mcp_installed_to"))).size(11).color(OryxisColors::t().success));
    }

    // ── Vault password (ORYXIS_VAULT_PASSWORD) ──
    // A password-protected vault makes the MCP server exit at startup
    // unless the client passes the master password, which MCP clients
    // report as a failed connection (issue #72). Surface that here and
    // offer to embed the password after an explicit typed confirmation;
    // without a master password the static note still explains the
    // variable for users who add one later.
    info_col = info_col.push(Space::new().height(8));
    if !app.vault_ui.has_user_password {
        info_col = info_col.push(
            text(crate::i18n::t("mcp_info_vault_password_note"))
                .size(11)
                .color(OryxisColors::t().text_muted),
        );
    } else if app.mcp.include_vault_password {
        info_col = info_col
            .push(
                crate::widgets::dir_row(vec![
                    text(crate::i18n::t("mcp_vault_pw_included"))
                        .size(11)
                        .color(OryxisColors::t().success)
                        .into(),
                    Space::new().width(8).into(),
                    app.settings_nav_slot(
                        crate::keynav::RowAction::activate(Message::Mcp(McpMessage::McpVaultPwRemove)),
                        6.0,
                        token_action_btn(
                            crate::i18n::t("remove"),
                            OryxisColors::t().warning,
                            Message::Mcp(McpMessage::McpVaultPwRemove),
                        ),
                    ),
                ])
                .align_y(iced::Alignment::Center),
            )
            .push(Space::new().height(4))
            .push(
                text(crate::i18n::t("mcp_vault_pw_plaintext_warning"))
                    .size(10)
                    .color(OryxisColors::t().warning),
            );
    } else if let Some(typed) = &app.mcp.vault_pw_prompt {
        // Typed confirmation: embedding only happens after the user
        // proves they know the master password, so an unattended
        // unlocked app can't be used to exfiltrate it into a file.
        let input_id = iced::widget::Id::new("mcp-vault-pw");
        let pw_input = app.settings_nav_slot(
            crate::keynav::RowAction::input(input_id.clone()),
            6.0,
            iced::widget::text_input(crate::i18n::t("mcp_vault_pw_placeholder"), typed)
                .id(input_id)
                .secure(true)
                .on_input(|v| Message::Mcp(McpMessage::McpVaultPwInput(v)))
                .on_submit(Message::Mcp(McpMessage::McpVaultPwConfirm))
                .padding(8)
                .size(12)
                .width(240)
                .style(crate::widgets::rounded_input_style)
                .into(),
        );
        info_col = info_col
            .push(
                text(crate::i18n::t("mcp_vault_pw_confirm_prompt"))
                    .size(11)
                    .color(OryxisColors::t().text_secondary),
            )
            .push(Space::new().height(6))
            .push(
                crate::widgets::dir_row(vec![
                    pw_input,
                    Space::new().width(6).into(),
                    app.settings_nav_slot(
                        crate::keynav::RowAction::activate(Message::Mcp(McpMessage::McpVaultPwConfirm)),
                        6.0,
                        token_action_btn(
                            crate::i18n::t("mcp_vault_pw_confirm"),
                            OryxisColors::t().success,
                            Message::Mcp(McpMessage::McpVaultPwConfirm),
                        ),
                    ),
                    Space::new().width(6).into(),
                    app.settings_nav_slot(
                        crate::keynav::RowAction::activate(Message::Mcp(McpMessage::McpVaultPwPromptCancel)),
                        6.0,
                        token_action_btn(
                            crate::i18n::t("cancel"),
                            OryxisColors::t().text_secondary,
                            Message::Mcp(McpMessage::McpVaultPwPromptCancel),
                        ),
                    ),
                ])
                .align_y(iced::Alignment::Center),
            );
        if app.mcp.vault_pw_error {
            info_col = info_col.push(Space::new().height(4)).push(
                text(crate::i18n::t("mcp_vault_pw_wrong"))
                    .size(11)
                    .color(OryxisColors::t().error),
            );
        }
        info_col = info_col.push(Space::new().height(4)).push(
            text(crate::i18n::t("mcp_vault_pw_plaintext_warning"))
                .size(10)
                .color(OryxisColors::t().text_muted),
        );
    } else {
        info_col = info_col
            .push(
                text(crate::i18n::t("mcp_vault_pw_note"))
                    .size(11)
                    .color(OryxisColors::t().warning),
            )
            .push(Space::new().height(6))
            .push(app.settings_nav_slot(
                crate::keynav::RowAction::activate(Message::Mcp(McpMessage::McpVaultPwPromptOpen)),
                6.0,
                token_action_btn(
                    crate::i18n::t("mcp_vault_pw_include"),
                    OryxisColors::t().accent,
                    Message::Mcp(McpMessage::McpVaultPwPromptOpen),
                ),
            ));
        // Outcome of the last Remove: confirm the scrub, or surface a
        // failure so the user is never told the credential is gone while
        // it lingers on disk.
        match &app.mcp.vault_pw_strip_status {
            Some(Ok(())) => {
                info_col = info_col.push(Space::new().height(6)).push(
                    text(crate::i18n::t("mcp_vault_pw_removed"))
                        .size(10)
                        .color(OryxisColors::t().text_muted),
                );
            }
            Some(Err(e)) => {
                info_col = info_col.push(Space::new().height(6)).push(
                    text(format!(
                        "{} {e}",
                        crate::i18n::t("mcp_vault_pw_remove_failed")
                    ))
                    .size(10)
                    .color(OryxisColors::t().error),
                );
            }
            None => {}
        }
    }

    info_col = info_col
        .push(Space::new().height(12))
        .push(crate::widgets::dir_row(vec![
            app.settings_nav_slot_labeled(
                crate::i18n::t("mcp_install_claude"),
                crate::keynav::RowAction::activate(Message::Mcp(McpMessage::InstallMcpConfig)),
                6.0,
                install_btn.into(),
            ),
            Space::new().width(8).into(),
            app.settings_nav_slot(
                crate::keynav::RowAction::activate(Message::Mcp(McpMessage::CopyMcpConfig)),
                6.0,
                copy_btn.into(),
            ),
            Space::new().width(8).into(),
            app.settings_nav_slot(
                crate::keynav::RowAction::activate(Message::Mcp(McpMessage::HideMcpInfo)),
                6.0,
                close_btn.into(),
            ),
        ]));

    container(info_col)
        .padding(16)
        .width(Length::Fill)
        .style(|_| container::Style {
            background: Some(Background::Color(OryxisColors::t().bg_surface)),
            border: Border { radius: Radius::from(8.0), color: OryxisColors::t().accent, width: 1.0 },
            ..Default::default()
        })
        .into()
}
