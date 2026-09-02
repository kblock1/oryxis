//! Settings -> Security & Privacy section view. Split out of views/settings/mod.rs.

use super::*;
use iced::widget::column;

/// A highlighted, tinted callout: leading icon, bold title, muted
/// description, all framed by a soft border in `accent`. Shared by the
/// "why set a password" prompt (accent) and the "vault is protected"
/// state (success) so both read as deliberate emphasis, not stray text.
fn vault_callout<'a>(
    icon: Element<'a, Message>,
    title: &'a str,
    desc: &'a str,
    accent: Color,
) -> Element<'a, Message> {
    container(
        dir_row(vec![
            icon,
            Space::new().width(12).into(),
            column![
                text(title)
                    .size(13)
                    .font(iced::Font {
                        weight: iced::font::Weight::Semibold,
                        ..iced::Font::DEFAULT
                    })
                    .color(OryxisColors::t().text_primary),
                Space::new().height(4),
                text(desc).size(11).color(OryxisColors::t().text_secondary),
            ]
            .width(Length::Fill)
            .align_x(dir_align_x())
            .into(),
        ])
        .align_y(iced::Alignment::Start),
    )
    .padding(14)
    .width(Length::Fill)
    .style(move |_| container::Style {
        background: Some(Background::Color(Color { a: 0.10, ..accent })),
        border: Border {
            radius: Radius::from(8.0),
            color: Color { a: 0.4, ..accent },
            width: 1.0,
        },
        ..Default::default()
    })
    .into()
}

/// Localized label for a size-cap code. The numbers are byte counts
/// (stable setting values), so the label is built from the code rather
/// than carrying one i18n key per size.
fn log_size_cap_label(code: &str) -> String {
    match code.parse::<u64>() {
        Ok(n) if n > 0 => crate::views::sftp::format_size(n),
        _ => crate::i18n::t("log_retention_off").to_string(),
    }
}

impl Oryxis {
    /// The change-master-password form: current password (verified
    /// before the rotation runs), then the new password twice. Tab walks
    /// the three fields (see the KeyboardEvent handler); Enter on any
    /// field submits. Only rendered while `change_password_open`.
    fn change_password_form(&self) -> Element<'_, Message> {
        use crate::state::SecretField;
        let current = container(crate::widgets::password_input_with_eye(
            t("current_master_password_placeholder"),
            &self.vault_ui.current_password,
            |v| Message::Vault(VaultMessage::VaultCurrentPasswordChanged(v.into())),
            Some(Message::Vault(VaultMessage::ConfirmChangeVaultPassword)),
            self.revealed_secrets.contains(&SecretField::VaultCurrentPassword),
            Message::Settings(SettingsMessage::ToggleSecretVisibility(SecretField::VaultCurrentPassword)),
            10.0,
        ))
        .width(300);
        let new = container(crate::widgets::password_input_with_eye(
            t("new_master_password_placeholder"),
            &self.vault_ui.new_password,
            |v| Message::Vault(VaultMessage::VaultNewPasswordChanged(v.into())),
            Some(Message::Vault(VaultMessage::ConfirmChangeVaultPassword)),
            self.revealed_secrets.contains(&SecretField::VaultNewPassword),
            Message::Settings(SettingsMessage::ToggleSecretVisibility(SecretField::VaultNewPassword)),
            10.0,
        ))
        .width(300);
        let confirm = container(crate::widgets::password_input_with_eye(
            t("confirm_master_password_placeholder"),
            &self.vault_ui.confirm_password,
            |v| Message::Vault(VaultMessage::VaultConfirmPasswordChanged(v.into())),
            Some(Message::Vault(VaultMessage::ConfirmChangeVaultPassword)),
            self.revealed_secrets.contains(&SecretField::VaultConfirmPassword),
            Message::Settings(SettingsMessage::ToggleSecretVisibility(SecretField::VaultConfirmPassword)),
            10.0,
        ))
        .width(300);
        let error: Element<'_, Message> = if let Some(err) = &self.vault_ui.password_error {
            text(err.clone()).size(12).color(OryxisColors::t().error).into()
        } else {
            Space::new().into()
        };
        let update_btn = if self.vault_ui.calibrating {
            // E1: KDF calibration in flight; disable + show progress.
            crate::widgets::styled_button_opt(
                crate::i18n::t("kdf_calibrating"),
                None,
                OryxisColors::t().accent,
            )
        } else {
            styled_button(
                crate::i18n::t("update_password"),
                Message::Vault(VaultMessage::ConfirmChangeVaultPassword),
                OryxisColors::t().accent,
            )
        };
        let cancel_btn = styled_button(
            crate::i18n::t("cancel"),
            Message::Vault(VaultMessage::CancelChangeVaultPassword),
            OryxisColors::t().text_muted,
        );
        panel_section(column![
            text(t("change_password_title")).size(13).color(OryxisColors::t().text_primary),
            Space::new().height(10),
            current,
            Space::new().height(8),
            new,
            Space::new().height(8),
            confirm,
            Space::new().height(10),
            dir_row(vec![update_btn, Space::new().width(8).into(), cancel_btn]),
            error,
        ])
    }

    /// Vault master-password card: the header toggle (already wrapped
    /// in its panel section) plus the state-dependent body below it
    /// (set-up form, protected callout or remove confirm). Keyboard
    /// slots record during construction, so `view_settings_security`
    /// must call the per-card builders in on-screen order.
    fn security_password_card(&self) -> (Element<'_, Message>, Element<'_, Message>) {
        // The switch reflects either a committed password or an open
        // set-password form, so toggling it before a password exists
        // visibly moves the control (and reveals / hides the form).
        let password_toggle = self.nav_toggle_row(
            crate::i18n::t("vault_password"),
            self.vault_ui.has_user_password || self.vault_ui.show_password_form,
            Message::Vault(VaultMessage::ToggleVaultPassword),
        );

        let password_section: Element<'_, Message> = if !self.vault_ui.has_user_password {
            // No master password yet. Always lead with a highlighted
            // callout explaining why one matters; reveal the actual
            // input form only once the user flips the switch on.
            let importance = vault_callout(
                iced_fonts::lucide::shield()
                    .size(20)
                    .color(OryxisColors::t().accent)
                    .into(),
                t("vault_importance_title"),
                t("vault_importance_desc"),
                OryxisColors::t().accent,
            );

            if !self.vault_ui.show_password_form {
                // Switch is off: callout only, no input fields.
                column![Space::new().height(8), importance].into()
            } else {
            // Show password input to enable
            let input = container(crate::widgets::password_input_with_eye(
                t("new_master_password_placeholder"),
                &self.vault_ui.new_password,
                |v| Message::Vault(VaultMessage::VaultNewPasswordChanged(v.into())),
                Some(Message::Vault(VaultMessage::SetVaultPassword)),
                self.revealed_secrets
                    .contains(&crate::state::SecretField::VaultNewPassword),
                Message::Settings(SettingsMessage::ToggleSecretVisibility(
                    crate::state::SecretField::VaultNewPassword,
                )),
                10.0,
            ))
            .width(300);
            // Second hidden entry: both are masked, so a typo in
            // the first would otherwise only surface at the next
            // unlock, when the only recovery is to destroy the
            // vault. Require them to match before accepting.
            let confirm = container(crate::widgets::password_input_with_eye(
                t("confirm_master_password_placeholder"),
                &self.vault_ui.confirm_password,
                |v| Message::Vault(VaultMessage::VaultConfirmPasswordChanged(v.into())),
                Some(Message::Vault(VaultMessage::SetVaultPassword)),
                self.revealed_secrets
                    .contains(&crate::state::SecretField::VaultConfirmPassword),
                Message::Settings(SettingsMessage::ToggleSecretVisibility(
                    crate::state::SecretField::VaultConfirmPassword,
                )),
                10.0,
            ))
            .width(300);
            let btn = if self.vault_ui.calibrating {
                crate::widgets::styled_button_opt(crate::i18n::t("kdf_calibrating"), None, OryxisColors::t().accent)
            } else {
                styled_button(crate::i18n::t("set_password"), Message::Vault(VaultMessage::SetVaultPassword), OryxisColors::t().accent)
            };
            // Offer the biometric convenience layer at password-creation
            // time (the market-standard moment: 1Password / Bitwarden ask
            // right after the master password is set), pre-checked when
            // the platform supports it.
            let bio_opt: Element<'_, Message> = if self.biometric_available {
                column![
                    Space::new().height(8),
                    container(crate::widgets::toggle_row(
                        crate::biometric::bio_setup_label(),
                        self.vault_ui.setup_enable_biometric,
                        Message::Vault(VaultMessage::ToggleSetupBiometric),
                    ))
                    .width(300),
                ]
                .into()
            } else {
                Space::new().into()
            };
            let error: Element<'_, Message> = if let Some(err) = &self.vault_ui.password_error {
                text(err.clone()).size(12).color(OryxisColors::t().error).into()
            } else {
                Space::new().into()
            };
            column![
                Space::new().height(8),
                importance,
                Space::new().height(12),
                text(t("vault_set_password_desc"))
                    .size(11).color(OryxisColors::t().text_muted),
                Space::new().height(8),
                input,
                Space::new().height(8),
                confirm,
                bio_opt,
                Space::new().height(8),
                btn,
                error,
            ].into()
            }
        } else {
            let error: Element<'_, Message> = if let Some(err) = &self.vault_ui.password_error {
                text(err.clone()).size(12).color(OryxisColors::t().error).into()
            } else {
                Space::new().into()
            };
            if self.vault_ui.confirm_remove_password {
                // Confirm prompt armed by the header switch. The header
                // toggle is the single removal path now (the old
                // standalone Remove button was redundant); this two-step
                // gate makes disabling deliberate, not a one-click slip.
                let warn_callout = vault_callout(
                    iced_fonts::lucide::triangle_alert()
                        .size(20)
                        .color(OryxisColors::t().warning)
                        .into(),
                    t("vault_remove_confirm_title"),
                    t("vault_remove_confirm_desc"),
                    OryxisColors::t().warning,
                );
                let remove_btn = self.settings_nav_slot(
                    crate::keynav::RowAction::activate(Message::Vault(VaultMessage::ConfirmRemoveVaultPassword)),
                    6.0,
                    styled_button(
                        crate::i18n::t("remove_password"),
                        Message::Vault(VaultMessage::ConfirmRemoveVaultPassword),
                        OryxisColors::t().error,
                    ),
                );
                let cancel_btn = self.settings_nav_slot(
                    crate::keynav::RowAction::activate(Message::Vault(VaultMessage::CancelRemoveVaultPassword)),
                    6.0,
                    styled_button(
                        crate::i18n::t("cancel"),
                        Message::Vault(VaultMessage::CancelRemoveVaultPassword),
                        OryxisColors::t().text_muted,
                    ),
                );
                column![
                    Space::new().height(8),
                    warn_callout,
                    Space::new().height(10),
                    dir_row(vec![
                        remove_btn,
                        Space::new().width(8).into(),
                        cancel_btn,
                    ]),
                    error,
                ]
                .into()
            } else {
                // Steady protected state: a highlighted success callout
                // rather than a faint one-liner, so the reassurance reads
                // as a deliberate status.
                let protected = vault_callout(
                    iced_fonts::lucide::shield_check()
                        .size(20)
                        .color(OryxisColors::t().success)
                        .into(),
                    t("vault_protected_title"),
                    t("vault_protected_note"),
                    OryxisColors::t().success,
                );
                column![Space::new().height(8), protected, error].into()
            }
        };

        (panel_section(column![password_toggle]), password_section)
    }

    /// Biometric (OS-keystore) unlock card: lives inside the
    /// vault-password block, since it is a convenience layer over the
    /// master password (it stores that password in the OS keystore).
    /// Rendered as a highlighted card like the other vault callouts;
    /// offered only where the platform supports it and the vault
    /// actually has a password to store. Built before the lock/update
    /// buttons, so the keynav recording order matches the on-screen
    /// order.
    fn security_biometric_card(&self) -> Element<'_, Message> {
        if self.biometric_available && self.vault_ui.has_user_password {
            let tint = OryxisColors::t().accent;
            let card = container(
                dir_row(vec![
                    crate::biometric::bio_icon().size(20).color(tint).into(),
                    Space::new().width(12).into(),
                    column![
                        self.nav_toggle_row(
                            crate::biometric::bio_setting_label(),
                            self.prefs.biometric_unlock_enabled,
                            Message::Vault(VaultMessage::ToggleBiometricUnlock),
                        ),
                        Space::new().height(4),
                        text(t("biometric_unlock_desc"))
                            .size(11)
                            .color(OryxisColors::t().text_secondary),
                    ]
                    .width(Length::Fill)
                    .align_x(dir_align_x())
                    .into(),
                ])
                .align_y(iced::Alignment::Start),
            )
            .padding(14)
            .width(Length::Fill)
            .style(move |_| container::Style {
                background: Some(Background::Color(Color { a: 0.10, ..tint })),
                border: Border {
                    radius: Radius::from(8.0),
                    color: Color { a: 0.4, ..tint },
                    width: 1.0,
                },
                ..Default::default()
            });
            column![Space::new().height(12), card].into()
        } else {
            Space::new().into()
        }
    }

    /// Lock / auto-lock card pair: the Lock Vault + Update password
    /// button row (with the change form dropping in below when open)
    /// and the idle auto-lock panel. Returned as two elements so the
    /// assembly keeps its exact spacing between them.
    fn security_lock_card(&self) -> (Element<'_, Message>, Element<'_, Message>) {
        // Lock Vault only makes sense once a master password is
        // set; without one, locking has nothing to protect and
        // the unlock screen would have no way to re-enter (the
        // vault re-opens itself with an empty key). Show the
        // button when a password exists; otherwise replace with
        // a muted note telling the user how to enable locking.
        let lock_btn: Element<'_, Message> = if self.vault_ui.has_user_password {
            // Asks first: Lock Vault tears every live session down, so
            // the button opens the confirm dialog (`LockVaultConfirm`)
            // rather than committing directly.
            self.settings_nav_slot_labeled(
                t("lock_vault"),
                crate::keynav::RowAction::activate(Message::Vault(VaultMessage::LockVaultConfirm)),
                8.0,
                button(
                    container(
                        dir_row(vec![
                            iced_fonts::lucide::lock().size(14).color(OryxisColors::t().warning).into(),
                            Space::new().width(10).into(),
                            text(crate::i18n::t("lock_vault")).size(13).color(OryxisColors::t().warning).into(),
                        ]).align_y(iced::Alignment::Center),
                    )
                    .padding(Padding { top: 10.0, right: 20.0, bottom: 10.0, left: 20.0 }),
                )
                .on_press(Message::Vault(VaultMessage::LockVaultConfirm))
                .style(|_, status| {
                    let bg = match status {
                        BtnStatus::Hovered => Color { a: 0.15, ..OryxisColors::t().warning },
                        _ => Color::TRANSPARENT,
                    };
                    button::Style {
                        background: Some(Background::Color(bg)),
                        border: Border { radius: Radius::from(8.0), color: OryxisColors::t().warning, width: 1.0 },
                        ..Default::default()
                    }
                })
                .into(),
            )
        } else {
            text(crate::i18n::t("lock_vault_requires_password"))
                .size(11)
                .color(OryxisColors::t().text_muted)
                .into()
        };

        // "Update password" sits beside Lock Vault: rotate the master
        // password (current -> new) without removing encryption. Both
        // controls only apply once a password exists; when it does, the
        // change form drops in below the button row when opened.
        let lock_row: Element<'_, Message> = if self.vault_ui.has_user_password {
            let update_btn = self.settings_nav_slot_labeled(
                t("update_password"),
                crate::keynav::RowAction::activate(Message::Vault(VaultMessage::OpenChangeVaultPassword)),
                8.0,
                button(
                    container(
                        dir_row(vec![
                            iced_fonts::lucide::key_round().size(14).color(OryxisColors::t().accent).into(),
                            Space::new().width(10).into(),
                            text(crate::i18n::t("update_password")).size(13).color(OryxisColors::t().accent).into(),
                        ]).align_y(iced::Alignment::Center),
                    )
                    .padding(Padding { top: 10.0, right: 20.0, bottom: 10.0, left: 20.0 }),
                )
                .on_press(Message::Vault(VaultMessage::OpenChangeVaultPassword))
                .style(|_, status| {
                    let bg = match status {
                        BtnStatus::Hovered => Color { a: 0.15, ..OryxisColors::t().accent },
                        _ => Color::TRANSPARENT,
                    };
                    button::Style {
                        background: Some(Background::Color(bg)),
                        border: Border { radius: Radius::from(8.0), color: OryxisColors::t().accent, width: 1.0 },
                        ..Default::default()
                    }
                })
                .into(),
            );
            let buttons = dir_row(vec![lock_btn, Space::new().width(10).into(), update_btn]);
            if self.vault_ui.change_password_open {
                column![buttons, Space::new().height(12), self.change_password_form()].into()
            } else {
                buttons.into()
            }
        } else {
            lock_btn
        };

        // What the Lock Vault button does (issue #169 follow-up):
        // ask every time (the confirm dialog, default), sleep (soft
        // lock, sessions survive) or lock (teardown directly). The
        // dialog's "always use the selected option" opt-in writes the
        // same setting; this row is the way back to "ask". Recorded
        // after the button pair, matching the on-screen order, and
        // hidden with the buttons while no master password exists.
        let manual_action: Element<'_, Message> = if self.vault_ui.has_user_password {
            column![
                self.nav_pick_row(
                    t("manual_lock_action"),
                    vec!["ask".into(), "sleep".into(), "lock".into()],
                    self.prefs.manual_lock_action.clone(),
                    |v| crate::i18n::t(match v.as_str() {
                        "sleep" => "lock_vault_sleep",
                        "lock" => "lock_vault",
                        _ => "manual_lock_ask",
                    })
                    .to_string(),
                    240.0,
                    |v| Message::Settings(SettingsMessage::SettingManualLockActionChanged(v)),
                ),
                Space::new().height(4),
                text(t("manual_lock_action_desc"))
                    .size(11)
                    .color(OryxisColors::t().text_muted),
            ]
            .into()
        } else {
            Space::new().into()
        };

        // MCP Server moved to its own Settings sidebar entry
        // in v0.7 (see `view_settings_mcp`). Keeping it here
        // was crowding the Security panel.

        // Auto-lock + clipboard hygiene. Both are numeric fields with
        // 0 = off; the idle lock only applies once a master password
        // exists (without one, locking has nothing to protect), so the
        // field is replaced by the same muted note as the Lock button.
        // Built here, before the export/import card, so the keyboard
        // recording order matches the on-screen order.
        let auto_lock_field: Element<'_, Message> = if self.vault_ui.has_user_password {
            self.settings_nav_slot_labeled(
                t("auto_lock_minutes"),
                crate::keynav::RowAction::input(iced::widget::Id::new("set-security-auto-lock")),
                10.0,
                text_input("0", &self.prefs.auto_lock_minutes)
                    .id(iced::widget::Id::new("set-security-auto-lock"))
                    .on_input(|v| Message::Settings(SettingsMessage::SettingAutoLockChanged(v)))
                    .padding(10)
                    .width(240)
                    .style(crate::widgets::rounded_input_style)
                    .align_x(dir_align_x())
                    .into(),
            )
        } else {
            text(crate::i18n::t("lock_vault_requires_password"))
                .size(11)
                .color(OryxisColors::t().text_muted)
                .into()
        };
        let auto_lock_section = panel_section(column![
            text(crate::i18n::t("auto_lock_minutes"))
                .size(13)
                .color(OryxisColors::t().text_primary),
            Space::new().height(4),
            text(t("setting_auto_lock_desc"))
                .size(11).color(OryxisColors::t().text_muted),
            Space::new().height(8),
            auto_lock_field,
        ]);

        // The picker only renders once a password exists; without one
        // the button row already collapsed to the muted note, so the
        // block stays a single element either way.
        let lock_block: Element<'_, Message> = if self.vault_ui.has_user_password {
            column![lock_row, Space::new().height(14), manual_action].into()
        } else {
            lock_row
        };
        (lock_block, auto_lock_section)
    }

    /// Privacy Mode card: the master toggle plus, while the mode is
    /// on, the per-class gates and the always/never mask lists.
    fn security_privacy_card(&self) -> Element<'_, Message> {
        // Privacy & logging: session recordings, connection
        // history and the retention window. Moved here from the
        // Terminal section, recordings are scrubbed for secrets
        // and sealed at rest, so they belong with the vault.
        let mut privacy_rows = column![
            self.nav_toggle_row(
                crate::i18n::t("privacy_mode_label"),
                self.privacy.mode,
                Message::Settings(SettingsMessage::TogglePrivacyMode),
            ),
            Space::new().height(4),
            text(crate::i18n::t("privacy_mode_desc"))
                .size(11).color(OryxisColors::t().text_muted),
        ];
        // Mask lists (issue #78), shown only while the mode is on
        // (toggle-hidden, the house rule for optional machinery).
        // Always-mask: user literals joining the vault-derived terms.
        // Never-mask: words the derived terms must skip, seeded with
        // the generic usernames (root, ubuntu, ...) so `ls -l` output
        // stays readable by default.
        if self.privacy.mode {
            // Per-class gates (issue #78 block 1), all on by default.
            // Public IPs get their own switch because documentation
            // screenshots sometimes NEED the public address readable
            // while everything else stays masked. Deliberately NOT a
            // terminal-vs-app-UI split: a half-enabled mode is exactly
            // the kind of state that made #53 confusing.
            use crate::messages::PrivacyMaskClass;
            privacy_rows = privacy_rows
                .push(Space::new().height(10))
                .push(self.nav_toggle_row(
                    crate::i18n::t("privacy_class_public_ips"),
                    self.privacy.mask_public_ips,
                    Message::Settings(SettingsMessage::TogglePrivacyMaskClass(PrivacyMaskClass::PublicIps)),
                ))
                .push(Space::new().height(8))
                .push(self.nav_toggle_row(
                    crate::i18n::t("privacy_class_private_ips"),
                    self.privacy.mask_private_ips,
                    Message::Settings(SettingsMessage::TogglePrivacyMaskClass(PrivacyMaskClass::PrivateIps)),
                ))
                .push(Space::new().height(8))
                .push(self.nav_toggle_row(
                    crate::i18n::t("privacy_class_usernames"),
                    self.privacy.mask_usernames,
                    Message::Settings(SettingsMessage::TogglePrivacyMaskClass(PrivacyMaskClass::Usernames)),
                ))
                .push(Space::new().height(8))
                .push(self.nav_toggle_row(
                    crate::i18n::t("privacy_class_hostnames"),
                    self.privacy.mask_hostnames,
                    Message::Settings(SettingsMessage::TogglePrivacyMaskClass(PrivacyMaskClass::Hostnames)),
                ));
            privacy_rows = privacy_rows
                .push(Space::new().height(14))
                .push(
                    text(crate::i18n::t("privacy_always_mask_label"))
                        .size(13)
                        .color(OryxisColors::t().text_primary),
                )
                .push(Space::new().height(4))
                .push(
                    text(crate::i18n::t("privacy_always_mask_desc"))
                        .size(11)
                        .color(OryxisColors::t().text_muted),
                )
                .push(Space::new().height(8))
                .push(self.settings_nav_slot_labeled(
                    t("privacy_always_mask_label"),
                    crate::keynav::RowAction::input(iced::widget::Id::new(
                        "set-security-privacy-always",
                    )),
                    10.0,
                    // Multi-line textarea: long lists wrap instead of
                    // scrolling out of a one-line input; newlines
                    // separate entries like commas (parse_privacy_list
                    // splits on both). Auto-grows with the content.
                    // No `dir_align_x()` counterpart: the fork's
                    // text_editor exposes no alignment builder and uses
                    // text::Alignment::Default internally, which already
                    // right-aligns genuinely-RTL lines; forcing layout
                    // alignment would need a fork change (off-limits).
                    iced::widget::text_editor(&self.privacy.always_mask_editor)
                        .placeholder("internal.example.com, acme-corp")
                        .id(iced::widget::Id::new("set-security-privacy-always"))
                        .on_action(|a| Message::Settings(SettingsMessage::SettingPrivacyAlwaysMaskAction(a)))
                        .padding(10)
                        .height(Length::Shrink)
                        .style(crate::widgets::rounded_text_editor_style)
                        .into(),
                ))
                .push(Space::new().height(14))
                .push(
                    text(crate::i18n::t("privacy_never_mask_label"))
                        .size(13)
                        .color(OryxisColors::t().text_primary),
                )
                .push(Space::new().height(4))
                .push(
                    text(crate::i18n::t("privacy_never_mask_desc"))
                        .size(11)
                        .color(OryxisColors::t().text_muted),
                )
                .push(Space::new().height(8))
                .push(self.settings_nav_slot_labeled(
                    t("privacy_never_mask_label"),
                    crate::keynav::RowAction::input(iced::widget::Id::new(
                        "set-security-privacy-never",
                    )),
                    10.0,
                    iced::widget::text_editor(&self.privacy.never_mask_editor)
                        .placeholder("root, ubuntu, admin")
                        .id(iced::widget::Id::new("set-security-privacy-never"))
                        .on_action(|a| Message::Settings(SettingsMessage::SettingPrivacyNeverMaskAction(a)))
                        .padding(10)
                        .height(Length::Shrink)
                        .style(crate::widgets::rounded_text_editor_style)
                        .into(),
                ));
        }
        panel_section(privacy_rows)
    }

    /// Sub-row for the plain-text session-log folder, shown only while
    /// the mirror is on: the effective folder (default
    /// `~/.oryxis/session-logs/`) with a Browse button, indented like
    /// the other nested sub-options. Sibling of the command-history
    /// folder row in Settings > Terminal.
    fn session_log_dir_row(&self) -> Element<'_, Message> {
        if !self.prefs.session_log_file {
            return Space::new().into();
        }
        let indent = if crate::i18n::is_rtl_layout() {
            Padding { right: 22.0, ..Padding::ZERO }
        } else {
            Padding { left: 22.0, ..Padding::ZERO }
        };
        let dir = self.session_log_file_dir().display().to_string();
        let change = self.settings_nav_slot(
            crate::keynav::RowAction::activate(Message::Settings(
                SettingsMessage::PickSessionLogFileDir,
            )),
            8.0,
            crate::widgets::styled_button_opt(
                crate::i18n::t("browse"),
                Some(Message::Settings(SettingsMessage::PickSessionLogFileDir)),
                OryxisColors::t().accent,
            ),
        );
        container(
            dir_row(vec![
                text(dir)
                    .size(12)
                    .color(OryxisColors::t().text_muted)
                    .width(Length::Fill)
                    .into(),
                Space::new().width(10).into(),
                change,
            ])
            .align_y(iced::Alignment::Center),
        )
        .padding(Padding { top: 8.0, ..indent })
        .width(Length::Fill)
        .into()
    }

    /// Logging card: session recording (+ its sub-options), the
    /// connection-history toggle and the retention picker, one
    /// logging theme in a single card.
    fn security_logging_card(&self) -> Element<'_, Message> {
        let session_logging_enabled = self.prefs.session_logging;
        let mut session_logging_rows = column![
            self.nav_toggle_row(
                crate::i18n::t("session_logging"),
                session_logging_enabled,
                Message::Settings(SettingsMessage::SettingToggleSessionLogging),
            ),
            Space::new().height(4),
            text(t("setting_session_logging_desc"))
                .size(11).color(OryxisColors::t().text_muted),
        ];
        // Recording sub-options only make sense while recording is on
        // (progressive disclosure; keynav slots record during view(),
        // so conditional rows drop out of the Tab walk for free).
        if session_logging_enabled {
            session_logging_rows = session_logging_rows
                .push(Space::new().height(8))
                .push(self.nav_toggle_row(
                    crate::i18n::t("session_log_full"),
                    self.prefs.session_log_full,
                    Message::Settings(SettingsMessage::SettingToggleSessionLogFull),
                ))
                .push(Space::new().height(4))
                .push(
                    text(t("setting_session_log_full_desc"))
                        .size(11).color(OryxisColors::t().text_muted),
                )
                .push(Space::new().height(8))
                .push(self.nav_toggle_row(
                    crate::i18n::t("session_log_compress"),
                    self.prefs.session_log_compress,
                    Message::Settings(SettingsMessage::SettingToggleSessionLogCompress),
                ))
                .push(Space::new().height(4))
                .push(
                    text(t("setting_session_log_compress_desc"))
                        .size(11).color(OryxisColors::t().text_muted),
                )
                // The plain-text mirror (issue #187). Nested under the
                // recording rather than standing on its own, because it
                // writes what the recording captures: the per-host
                // override still decides which sessions produce a file.
                .push(Space::new().height(8))
                .push(self.nav_toggle_row(
                    crate::i18n::t("session_log_file"),
                    self.prefs.session_log_file,
                    Message::Settings(SettingsMessage::SettingToggleSessionLogFile),
                ))
                .push(Space::new().height(4))
                .push(
                    text(t("setting_session_log_file_desc"))
                        .size(11).color(OryxisColors::t().text_muted),
                )
                .push(self.session_log_dir_row());
        }
        // Session recording, connection history and the retention
        // window are one logging theme, so they share a single card
        // below (16 px between the sub-blocks).
        let logging_rows = session_logging_rows
            .push(Space::new().height(16))
            .push(self.nav_toggle_row(
                crate::i18n::t("connection_history"),
                self.prefs.connection_history,
                Message::Settings(SettingsMessage::SettingToggleConnectionHistory),
            ))
            .push(Space::new().height(4))
            .push(
                text(t("setting_connection_history_desc"))
                    .size(11).color(OryxisColors::t().text_muted),
            );

        // Retention: auto-delete connection events + finished
        // recordings past the picked age. Codes are stable
        // setting values; the mapper localizes per code.
        const RETENTION_CODES: [&str; 7] =
            ["off", "1d", "3d", "7d", "14d", "30d", "90d"];
        let retention_selected = RETENTION_CODES
            .iter()
            .copied()
            .find(|c| *c == self.prefs.logs_retention)
            .unwrap_or("off");
        // Left/Right cycle the retention codes without opening the
        // dropdown (non-standard picker layout: label above, list
        // below, so nav_pick_row does not apply).
        let (retention_prev, retention_next) = crate::keynav::slots::cycle_pair(
            &RETENTION_CODES,
            &retention_selected,
            |v| Message::Settings(SettingsMessage::LogsRetentionChanged(v)),
        );
        // Byte counts, stable as setting values; "off" is the default
        // and the only one that is not a number.
        const SIZE_CAP_CODES: [&str; 6] =
            ["off", "536870912", "1073741824", "5368709120", "10737418240", "53687091200"];
        let cap_now = self
            .prefs
            .session_log_max_bytes
            .map(|n| n.to_string())
            .unwrap_or_else(|| "off".into());
        let size_cap_selected = SIZE_CAP_CODES
            .iter()
            .copied()
            .find(|c| *c == cap_now)
            .unwrap_or("off");
        let (size_cap_prev, size_cap_next) = crate::keynav::slots::cycle_pair(
            &SIZE_CAP_CODES,
            &size_cap_selected,
            |v| Message::Settings(SettingsMessage::LogsSizeCapChanged(v)),
        );
        panel_section(logging_rows.push(Space::new().height(16)).push(column![
            text(crate::i18n::t("log_retention_label"))
                .size(13)
                .color(OryxisColors::t().text_primary),
            Space::new().height(4),
            text(t("setting_log_retention_desc"))
                .size(11).color(OryxisColors::t().text_muted),
            Space::new().height(8),
            self.settings_nav_slot_labeled(
                t("log_retention_label"),
                crate::keynav::RowAction::picker(retention_prev, retention_next),
                8.0,
                pick_list(
                    Some(retention_selected),
                    &RETENTION_CODES[..],
                    |code: &&str| {
                        crate::i18n::t(match *code {
                            "1d" => "log_retention_1d",
                            "3d" => "log_retention_3d",
                            "7d" => "log_retention_7d",
                            "14d" => "log_retention_14d",
                            "30d" => "log_retention_30d",
                            "90d" => "log_retention_90d",
                            _ => "log_retention_off",
                        })
                        .to_string()
                    },
                )
                .on_select(|v| Message::Settings(SettingsMessage::LogsRetentionChanged(v)))
                .on_open(Message::Navigation(NavigationMessage::PickOpenChanged(true)))
                .on_close(Message::Navigation(NavigationMessage::PickOpenChanged(false)))
                .width(260).padding(10).style(crate::widgets::rounded_pick_list_style)
                .into(),
            ),
            // Size cap: the user's own quota on what recordings may
            // occupy together. Reaching it drops the OLDEST FINISHED
            // recordings (retention by size, sibling of the retention
            // by age above), it does not stop recording the present.
            // The floor that stops a runaway is the free-space check in
            // `dispatch_terminal::output`, deliberately not a setting.
            Space::new().height(16),
            text(crate::i18n::t("log_size_cap_label"))
                .size(13)
                .color(OryxisColors::t().text_primary),
            Space::new().height(4),
            text(t("setting_log_size_cap_desc"))
                .size(11).color(OryxisColors::t().text_muted),
            Space::new().height(8),
            self.settings_nav_slot_labeled(
                t("log_size_cap_label"),
                crate::keynav::RowAction::picker(size_cap_prev, size_cap_next),
                8.0,
                pick_list(
                    Some(size_cap_selected),
                    &SIZE_CAP_CODES[..],
                    |code: &&str| log_size_cap_label(code),
                )
                .on_select(|v| Message::Settings(SettingsMessage::LogsSizeCapChanged(v)))
                .on_open(Message::Navigation(NavigationMessage::PickOpenChanged(true)))
                .on_close(Message::Navigation(NavigationMessage::PickOpenChanged(false)))
                .width(260).padding(10).style(crate::widgets::rounded_pick_list_style)
                .into(),
            ),
        ]))
    }

    /// Export / import card body: the vault export + import buttons,
    /// the inline export / import dialogs, the SFTP backup-target
    /// picker and the status lines. The SSH config importer is built
    /// separately (`security_ssh_import_card`); the assembly joins
    /// the two inside one panel.
    fn security_export_import_card(&self) -> iced::widget::Column<'_, Message> {
        // Export/Import section
        let export_btn = self.settings_nav_slot_labeled(
            t("export_vault"),
            crate::keynav::RowAction::activate(Message::Share(ShareMessage::ExportVault)),
            6.0,
            styled_button(crate::i18n::t("export_vault"), Message::Share(ShareMessage::ExportVault), OryxisColors::t().accent),
        );
        let import_btn = self.settings_nav_slot_labeled(
            t("import_vault"),
            crate::keynav::RowAction::activate(Message::Share(ShareMessage::ImportVault)),
            6.0,
            styled_button(crate::i18n::t("import_vault"), Message::Share(ShareMessage::ImportVault), OryxisColors::t().text_muted),
        );
        // Restore from a remote host. Export-to-SFTP is reached from
        // inside the export dialog (it needs the password first).
        let import_sftp_btn = self.settings_nav_slot_labeled(
            t("import_from_sftp"),
            crate::keynav::RowAction::activate(Message::Share(ShareMessage::ImportFromSftp)),
            6.0,
            styled_button(crate::i18n::t("import_from_sftp"), Message::Share(ShareMessage::ImportFromSftp), OryxisColors::t().text_muted),
        );
        // Secrets-free spreadsheet of the host list (the encrypted
        // .oryxis export above stays the only secrets-bearing path).
        let export_csv_btn = self.settings_nav_slot_labeled(
            t("export_hosts_csv"),
            crate::keynav::RowAction::activate(Message::Share(ShareMessage::ExportHostsCsv)),
            6.0,
            styled_button(crate::i18n::t("export_hosts_csv"), Message::Share(ShareMessage::ExportHostsCsv), OryxisColors::t().text_muted),
        );

        let mut export_import_section: iced::widget::Column<'_, Message> = column![
            text(crate::i18n::t("export_import")).size(13).color(OryxisColors::t().text_primary),
            Space::new().height(8),
            dir_row(vec![export_btn, Space::new().width(8).into(), export_csv_btn, Space::new().width(8).into(), import_btn, Space::new().width(8).into(), import_sftp_btn]),
        ];

        // Show export dialog inline
        if self.panels.export_dialog {
            // Keyboard rows (audit fix: the field recorded nothing
            // while its neighbors did): the field, then its eye.
            let pw_idx = self.settings_nav_record(crate::keynav::RowAction::input(
                iced::widget::Id::new("set-export-password"),
            ));
            let pw_input = self.settings_nav_ring_at(
                pw_idx,
                10.0,
                container(crate::widgets::password_input_with_eye_nav(
                    crate::i18n::t("export_password"),
                    &self.export_password,
                    |v| Message::Share(ShareMessage::ExportPasswordChanged(v.into())),
                    None,
                    self.revealed_secrets
                        .contains(&crate::state::SecretField::ExportPassword),
                    Message::Settings(SettingsMessage::ToggleSecretVisibility(
                        crate::state::SecretField::ExportPassword,
                    )),
                    10.0,
                    Some(iced::widget::Id::new("set-export-password")),
                    |eye| self.settings_nav_slot(
                        crate::keynav::RowAction::activate(
                            Message::Settings(SettingsMessage::ToggleSecretVisibility(
                                crate::state::SecretField::ExportPassword,
                            )),
                        ),
                        6.0,
                        eye,
                    ),
                ))
                .width(300)
                .into(),
            );
            // One checkbox per category, all checked by default.
            let mut categories: iced::widget::Column<'_, Message> =
                column![text(crate::i18n::t("export_select_what"))
                    .size(12)
                    .color(OryxisColors::t().text_muted)]
                .spacing(6);
            for cat in oryxis_vault::ExportCategory::ALL {
                // Enter/Space flips the checkbox from the keyboard,
                // same message the mouse toggle produces.
                categories = categories.push(self.settings_nav_slot(
                    crate::keynav::RowAction::activate(Message::Share(ShareMessage::ExportToggleCategory(cat))),
                    4.0,
                    checkbox(self.export_selection.get(cat))
                        .label(crate::i18n::t(category_label_key(cat)))
                        .on_toggle(move |_| Message::Share(ShareMessage::ExportToggleCategory(cat)))
                        .size(16)
                        .text_size(13)
                        .into(),
                ));
            }
            // Private-key material is a sub-option of the Keys
            // category, only meaningful when Keys is being exported.
            let keys_toggle: Element<'_, Message> = if self.export_selection.keys {
                self.settings_nav_slot(
                    crate::keynav::RowAction::activate(Message::Share(ShareMessage::ExportToggleKeys)),
                    8.0,
                    dir_row(vec![
                        text(crate::i18n::t("include_private_keys")).size(13).color(OryxisColors::t().text_secondary).into(),
                        Space::new().width(Length::Fill).into(),
                        button(
                            text(if self.export_include_keys { "ON" } else { "OFF" }).size(12)
                        ).on_press(Message::Share(ShareMessage::ExportToggleKeys)).style(move |_theme, _status| {
                            button::Style {
                                background: Some(Background::Color(if self.export_include_keys { OryxisColors::t().success } else { OryxisColors::t().bg_hover })),
                                border: Border { radius: Radius::from(4.0), ..Default::default() },
                                text_color: OryxisColors::t().text_primary,
                                ..Default::default()
                            }
                        }).into(),
                    ]).align_y(iced::Alignment::Center).into(),
                )
            } else {
                Space::new().into()
            };
            let confirm_btn = self.settings_nav_slot(
                crate::keynav::RowAction::activate(Message::Share(ShareMessage::ExportConfirm)),
                6.0,
                styled_button(crate::i18n::t("export_confirm"), Message::Share(ShareMessage::ExportConfirm), OryxisColors::t().success),
            );
            let sftp_btn = self.settings_nav_slot(
                crate::keynav::RowAction::activate(Message::Share(ShareMessage::ExportToSftp)),
                6.0,
                styled_button(crate::i18n::t("export_to_sftp"), Message::Share(ShareMessage::ExportToSftp), OryxisColors::t().accent),
            );
            let cancel_btn = self.settings_nav_slot(
                crate::keynav::RowAction::activate(Message::Share(ShareMessage::ExportImportDismiss)),
                6.0,
                styled_button(crate::i18n::t("cancel"), Message::Share(ShareMessage::ExportImportDismiss), OryxisColors::t().text_muted),
            );
            export_import_section = export_import_section
                .push(Space::new().height(12))
                .push(pw_input)
                .push(Space::new().height(10))
                .push(categories)
                .push(Space::new().height(8))
                .push(keys_toggle)
                .push(Space::new().height(8))
                .push(dir_row(vec![confirm_btn, Space::new().width(8).into(), sftp_btn, Space::new().width(8).into(), cancel_btn]));
        }

        // Show import dialog inline
        if self.panels.import_dialog {
            // Keyboard rows (audit fix, same as the export dialog).
            let pw_idx = self.settings_nav_record(crate::keynav::RowAction::input(
                iced::widget::Id::new("set-import-password"),
            ));
            let pw_input = self.settings_nav_ring_at(
                pw_idx,
                10.0,
                container(crate::widgets::password_input_with_eye_nav(
                    crate::i18n::t("import_password"),
                    &self.vault_import.password,
                    |v| Message::Share(ShareMessage::ImportPasswordChanged(v.into())),
                    // Enter inspects in phase 1, imports in phase 2.
                    Some(if self.vault_import.summary.is_some() {
                        Message::Share(ShareMessage::ImportConfirm)
                    } else {
                        Message::Share(ShareMessage::ImportInspect)
                    }),
                    self.revealed_secrets
                        .contains(&crate::state::SecretField::ImportPassword),
                    Message::Settings(SettingsMessage::ToggleSecretVisibility(
                        crate::state::SecretField::ImportPassword,
                    )),
                    10.0,
                    Some(iced::widget::Id::new("set-import-password")),
                    |eye| self.settings_nav_slot(
                        crate::keynav::RowAction::activate(
                            Message::Settings(SettingsMessage::ToggleSecretVisibility(
                                crate::state::SecretField::ImportPassword,
                            )),
                        ),
                        6.0,
                        eye,
                    ),
                ))
                .width(300)
                .into(),
            );
            export_import_section = export_import_section
                .push(Space::new().height(12))
                .push(text(crate::i18n::t("import_password_hint")).size(12).color(OryxisColors::t().text_muted))
                .push(Space::new().height(4))
                .push(pw_input);
            if let Some(summary) = &self.vault_import.summary {
                // Phase 2: the file is decrypted, show what it
                // holds. Present categories are interactive
                // checkboxes (with counts); absent ones are
                // greyed so the user sees the full shape.
                let mut categories: iced::widget::Column<'_, Message> =
                    column![text(crate::i18n::t("import_select_what"))
                        .size(12)
                        .color(OryxisColors::t().text_muted)]
                    .spacing(6);
                for cat in oryxis_vault::ExportCategory::ALL {
                    let count = summary.count(cat);
                    let label = crate::i18n::t(category_label_key(cat));
                    if count > 0 {
                        // Enter/Space flips the checkbox from the
                        // keyboard, same message the mouse toggle
                        // produces. Absent categories stay read-only.
                        categories = categories.push(self.settings_nav_slot(
                            crate::keynav::RowAction::activate(Message::Share(ShareMessage::ImportToggleCategory(cat))),
                            4.0,
                            checkbox(self.vault_import.selection.get(cat))
                                .label(format!("{label} ({count})"))
                                .on_toggle(move |_| Message::Share(ShareMessage::ImportToggleCategory(cat)))
                                .size(16)
                                .text_size(13)
                                .into(),
                        ));
                    } else {
                        categories = categories.push(
                            text(format!("{label} ({})", crate::i18n::t("import_not_in_file")))
                                .size(13)
                                .color(OryxisColors::t().text_muted),
                        );
                    }
                }
                // The Cancel button is built per phase (not shared
                // above) so the recording order follows the on-screen
                // order: checkboxes first, then Confirm, then Cancel.
                let confirm_btn = self.settings_nav_slot(
                    crate::keynav::RowAction::activate(Message::Share(ShareMessage::ImportConfirm)),
                    6.0,
                    styled_button(crate::i18n::t("import_confirm"), Message::Share(ShareMessage::ImportConfirm), OryxisColors::t().success),
                );
                let cancel_btn = self.settings_nav_slot(
                    crate::keynav::RowAction::activate(Message::Share(ShareMessage::ExportImportDismiss)),
                    6.0,
                    styled_button(crate::i18n::t("cancel"), Message::Share(ShareMessage::ExportImportDismiss), OryxisColors::t().text_muted),
                );
                export_import_section = export_import_section
                    .push(Space::new().height(10))
                    .push(categories)
                    .push(Space::new().height(8))
                    .push(dir_row(vec![confirm_btn, Space::new().width(8).into(), cancel_btn]));
            } else {
                // Phase 1: enter the password, then inspect.
                let inspect_btn = self.settings_nav_slot(
                    crate::keynav::RowAction::activate(Message::Share(ShareMessage::ImportInspect)),
                    6.0,
                    styled_button(crate::i18n::t("import_inspect"), Message::Share(ShareMessage::ImportInspect), OryxisColors::t().accent),
                );
                let cancel_btn = self.settings_nav_slot(
                    crate::keynav::RowAction::activate(Message::Share(ShareMessage::ExportImportDismiss)),
                    6.0,
                    styled_button(crate::i18n::t("cancel"), Message::Share(ShareMessage::ExportImportDismiss), OryxisColors::t().text_muted),
                );
                export_import_section = export_import_section
                    .push(Space::new().height(8))
                    .push(dir_row(vec![inspect_btn, Space::new().width(8).into(), cancel_btn]));
            }
        }

        // SFTP backup-target picker (export to / import from a
        // saved host). Reuses the export/import password + selection
        // state above; here the user only picks the host and path.
        if self.sftp_backup.open {
            let is_import = self.sftp_backup.is_import;
            let host_options: Vec<String> =
                self.connections.iter().map(|c| c.label.clone()).collect();
            let selected_host = self
                .sftp_backup.host
                .and_then(|i| self.connections.get(i))
                .map(|c| c.label.clone());
            let host_lookup: std::collections::HashMap<String, usize> = self
                .connections
                .iter()
                .enumerate()
                .map(|(i, c)| (c.label.clone(), i))
                .collect();
            // Left/Right cycle the saved hosts without opening the
            // dropdown (non-standard picker: label sits above it).
            let current_host_label = selected_host.clone().unwrap_or_default();
            let cycle_lookup = host_lookup.clone();
            let (host_prev, host_next) = crate::keynav::slots::cycle_pair(
                &host_options,
                &current_host_label,
                move |label: String| {
                    Message::Share(ShareMessage::SftpBackupHostSelected(
                        cycle_lookup.get(&label).copied().unwrap_or(0),
                    ))
                },
            );
            let host_picker = self.settings_nav_slot(
                crate::keynav::RowAction::picker(host_prev, host_next),
                8.0,
                pick_list(selected_host, host_options, |s: &String| s.clone())
                    .on_select(move |label: String| {
                        Message::Share(ShareMessage::SftpBackupHostSelected(
                            host_lookup.get(&label).copied().unwrap_or(0),
                        ))
                    })
                    .on_open(Message::Navigation(NavigationMessage::PickOpenChanged(true)))
                    .on_close(Message::Navigation(NavigationMessage::PickOpenChanged(false)))
                    .width(300)
                    .padding(10)
                    .style(crate::widgets::rounded_pick_list_style)
                    .into(),
            );
            let path_field = self.settings_nav_slot(
                crate::keynav::RowAction::input(iced::widget::Id::new("set-security-sftp-path")),
                10.0,
                text_input("vault.oryxis", &self.sftp_backup.path)
                    .id(iced::widget::Id::new("set-security-sftp-path"))
                    .on_input(|v| Message::Share(ShareMessage::SftpBackupPathChanged(v)))
                    .on_submit(Message::Share(ShareMessage::SftpBackupConfirm))
                    .width(300)
                    .padding(10)
                    .style(crate::widgets::rounded_input_style)
                    .into(),
            );
            // Restore collects the decrypt password here (export
            // already has it in the dialog above), so both flows ask
            // for the password before the confirm button.
            let import_pw: Option<Element<'_, Message>> = if is_import {
                // Keyboard rows (audit fix): the field, then its eye.
                let pw_idx = self.settings_nav_record(crate::keynav::RowAction::input(
                    iced::widget::Id::new("set-sftp-restore-password"),
                ));
                Some(self.settings_nav_ring_at(
                    pw_idx,
                    10.0,
                    container(crate::widgets::password_input_with_eye_nav(
                        crate::i18n::t("import_password"),
                        &self.vault_import.password,
                        |v| Message::Share(ShareMessage::ImportPasswordChanged(v.into())),
                        Some(Message::Share(ShareMessage::SftpBackupConfirm)),
                        self.revealed_secrets
                            .contains(&crate::state::SecretField::ImportPassword),
                        Message::Settings(SettingsMessage::ToggleSecretVisibility(
                            crate::state::SecretField::ImportPassword,
                        )),
                        10.0,
                        Some(iced::widget::Id::new("set-sftp-restore-password")),
                        |eye| self.settings_nav_slot(
                            crate::keynav::RowAction::activate(
                                Message::Settings(SettingsMessage::ToggleSecretVisibility(
                                    crate::state::SecretField::ImportPassword,
                                )),
                            ),
                            6.0,
                            eye,
                        ),
                    ))
                    .width(300)
                    .into(),
                ))
            } else {
                None
            };
            let title_key = if is_import { "restore_from_sftp" } else { "backup_to_sftp" };
            let confirm_msg = if self.sftp_backup.busy {
                None
            } else {
                Some(Message::Share(ShareMessage::SftpBackupConfirm))
            };
            let confirm_label = if self.sftp_backup.busy {
                crate::i18n::t("sftp_backup_working")
            } else if is_import {
                crate::i18n::t("sftp_backup_restore_confirm")
            } else {
                crate::i18n::t("sftp_backup_confirm")
            };
            // The confirm button is recorded only while it is enabled
            // (a busy round disables it, so Enter must not re-fire).
            let confirm_btn =
                styled_button_opt(confirm_label, confirm_msg.clone(), OryxisColors::t().success);
            let confirm_btn: Element<'_, Message> = if let Some(msg) = confirm_msg {
                self.settings_nav_slot(
                    crate::keynav::RowAction::activate(msg),
                    6.0,
                    confirm_btn,
                )
            } else {
                confirm_btn
            };
            let cancel_btn = self.settings_nav_slot(
                crate::keynav::RowAction::activate(Message::Share(ShareMessage::SftpBackupCancel)),
                6.0,
                styled_button(
                    crate::i18n::t("cancel"),
                    Message::Share(ShareMessage::SftpBackupCancel),
                    OryxisColors::t().text_muted,
                ),
            );
            let mut sftp_section: iced::widget::Column<'_, Message> = column![
                text(crate::i18n::t(title_key)).size(13).color(OryxisColors::t().text_primary),
                Space::new().height(2),
                text(crate::i18n::t("sftp_backup_hint")).size(12).color(OryxisColors::t().text_muted),
                Space::new().height(8),
                text(crate::i18n::t("sftp_backup_host")).size(12).color(OryxisColors::t().text_secondary),
                Space::new().height(4),
                host_picker,
                Space::new().height(8),
                text(crate::i18n::t("sftp_backup_remote_path")).size(12).color(OryxisColors::t().text_secondary),
                Space::new().height(4),
                path_field,
            ];
            if let Some(pw) = import_pw {
                sftp_section = sftp_section
                    .push(Space::new().height(10))
                    .push(pw);
            }
            sftp_section = sftp_section
                .push(Space::new().height(10))
                .push(dir_row(vec![confirm_btn, Space::new().width(8).into(), cancel_btn]));
            if let Some(status) = &self.sftp_backup.status {
                let (msg, color) = match status {
                    Ok(m) => (m.clone(), OryxisColors::t().success),
                    Err(e) => (e.clone(), OryxisColors::t().error),
                };
                sftp_section = sftp_section
                    .push(Space::new().height(8))
                    .push(text(msg).size(12).color(color));
            }
            export_import_section = export_import_section
                .push(Space::new().height(14))
                .push(sftp_section);
        }

        // Status messages
        if let Some(status) = &self.export_status {
            let (msg, color) = match status {
                Ok(m) => (m.as_str(), OryxisColors::t().success),
                Err(m) => (m.as_str(), OryxisColors::t().error),
            };
            export_import_section = export_import_section
                .push(Space::new().height(8))
                .push(text(msg).size(12).color(color));
        }
        if let Some(status) = &self.vault_import.status {
            let (msg, color) = match status {
                Ok(m) => (m.as_str(), OryxisColors::t().success),
                Err(m) => (m.as_str(), OryxisColors::t().error),
            };
            export_import_section = export_import_section
                .push(Space::new().height(8))
                .push(text(msg).size(12).color(color));
        }

        export_import_section
    }

    /// SSH config import block: sits below the vault export/import
    /// inside the same panel. One-shot batch importer; no preview
    /// yet.
    fn security_ssh_import_card(&self) -> iced::widget::Column<'_, Message> {
        let ssh_config_btn = self.settings_nav_slot_labeled(
            t("import_ssh_config_btn"),
            crate::keynav::RowAction::activate(Message::Share(ShareMessage::ImportSshConfig)),
            6.0,
            styled_button(
                t("import_ssh_config_btn"),
                Message::Share(ShareMessage::ImportSshConfig),
                OryxisColors::t().accent,
            ),
        );
        let mut ssh_config_section: iced::widget::Column<'_, Message> = column![
            text(t("ssh_config_import"))
                .size(13)
                .color(OryxisColors::t().text_primary),
            Space::new().height(4),
            text(t("ssh_config_import_desc"))
                .size(11)
                .color(OryxisColors::t().text_muted),
            Space::new().height(8),
            ssh_config_btn,
        ];
        if let Some(status) = &self.ssh_config_import_status {
            let (msg, color) = match status {
                Ok(m) => (m.as_str(), OryxisColors::t().success),
                Err(m) => (m.as_str(), OryxisColors::t().error),
            };
            ssh_config_section = ssh_config_section
                .push(Space::new().height(8))
                .push(text(msg).size(12).color(color));
        }

        ssh_config_section
    }

    pub(crate) fn view_settings_security(&self) -> Element<'_, Message> {
        // Keyboard rows are recorded in visual order. The set-password
        // and change-password forms are deliberately NOT recorded:
        // they carry their own Tab-walk and the keyboard router is
        // disabled while they are open.
        self.keynav_settings_reset();
        // Keyboard slots record during construction, so the cards are
        // built here in on-screen order; do not reorder these calls.
        let (password_toggle_card, password_section) = self.security_password_card();
        let biometric_section = self.security_biometric_card();
        let (lock_row, auto_lock_section) = self.security_lock_card();
        let privacy_mode_section = self.security_privacy_card();
        let logging_section = self.security_logging_card();
        let export_import_section = self.security_export_import_card();
        let ssh_config_section = self.security_ssh_import_card();

        scrollable(
            container(
                column![
                    crate::widgets::settings_group_header(crate::i18n::t("security_group_vault")),
                    Space::new().height(8),
                    password_toggle_card,
                    password_section,
                    biometric_section,
                    Space::new().height(16),
                    lock_row,
                    Space::new().height(16),
                    auto_lock_section,
                    Space::new().height(18),
                    crate::widgets::settings_group_header(crate::i18n::t("security_group_privacy")),
                    Space::new().height(8),
                    privacy_mode_section,
                    Space::new().height(18),
                    crate::widgets::settings_group_header(crate::i18n::t("security_group_logging")),
                    Space::new().height(8),
                    logging_section,
                    Space::new().height(18),
                    crate::widgets::settings_group_header(crate::i18n::t("security_group_import_export")),
                    Space::new().height(8),
                    // Vault export/import and the SSH config importer
                    // share one import/export card.
                    panel_section(
                        export_import_section
                            .push(Space::new().height(16))
                            .push(ssh_config_section),
                    ),
                    Space::new().height(24),
                ]
                .width(Length::Fill)
                .align_x(dir_align_x()),
            )
            .padding(Padding { top: 24.0, right: 24.0, bottom: 24.0, left: 24.0 }),
        )
        // Stable id so the keyboard router can keep the selected row
        // in view.
        .id(iced::widget::Id::new("settings-security-scroll"))
        .on_scroll(|v| Message::Settings(SettingsMessage::SectionScrolled(v.relative_offset().y)))
        .height(Length::Fill)
        .into()
    }
}
