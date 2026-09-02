//! UI helper widgets: toolbar. Split out of widgets/mod.rs.

use super::*;
/// Toolbar trigger button that opens the Sort dropdown. The glyph
/// reflects the active sort so the user can read the current mode
/// without opening the menu (A-z / Z-a / new-first / old-first).
/// Sizing matches the "+ Host" / "+ ADD" buttons (24 px tall) so all
/// toolbar actions share a visual baseline.
pub(crate) fn sort_toolbar_button(
    kind: crate::state::SortMenuKind,
    current: crate::state::ListSort,
) -> Element<'static, Message> {
    use crate::state::ListSort;
    let glyph: iced::widget::Text<'static, iced::Theme, iced::Renderer> = match current {
        ListSort::LabelAsc => iced_fonts::lucide::arrow_down_a_z(),
        ListSort::LabelDesc => iced_fonts::lucide::arrow_down_z_a(),
        ListSort::NewestFirst => iced_fonts::lucide::calendar_arrow_down(),
        ListSort::OldestFirst => iced_fonts::lucide::calendar_arrow_up(),
    };
    button(
        container(
            glyph
                .size(15)
                .color(OryxisColors::t().button_text),
        )
        .center_y(Length::Fixed(24.0))
        .center_x(Length::Fixed(24.0)),
    )
    .on_press(Message::Navigation(NavigationMessage::ToggleSortMenu(kind)))
    .style(|_, status| {
        let bg = match status {
            BtnStatus::Hovered => OryxisColors::t().button_bg_hover,
            _ => OryxisColors::t().button_bg,
        };
        button::Style {
            background: Some(Background::Color(bg)),
            border: Border { radius: Radius::from(6.0), ..Default::default() },
            ..Default::default()
        }
    })
    .into()
}

/// Grid / List / Tree view cycler for the host dashboard toolbar.
/// Shows the glyph for the CURRENT mode, styled like the sort button;
/// each press advances to the next mode (issue #102).
pub(crate) fn host_view_toggle_button(
    mode: crate::state::HostViewMode,
) -> Element<'static, Message> {
    use crate::state::HostViewMode;
    let glyph: iced::widget::Text<'static, iced::Theme, iced::Renderer> = match mode {
        HostViewMode::Grid => iced_fonts::lucide::layout_grid(),
        HostViewMode::List => iced_fonts::lucide::list(),
        HostViewMode::Tree => iced_fonts::lucide::folder_tree(),
    };
    let btn = button(
        container(
            glyph
                .size(15)
                .color(OryxisColors::t().button_text),
        )
        .center_y(Length::Fixed(24.0))
        .center_x(Length::Fixed(24.0)),
    )
    .on_press(Message::Settings(SettingsMessage::CycleHostViewMode))
    .style(|_, status| {
        let bg = match status {
            BtnStatus::Hovered => OryxisColors::t().button_bg_hover,
            _ => OryxisColors::t().button_bg,
        };
        button::Style {
            background: Some(Background::Color(bg)),
            border: Border { radius: Radius::from(6.0), ..Default::default() },
            ..Default::default()
        }
    });
    crate::views::terminal::icon_tooltip(btn.into(), crate::i18n::t("toggle_view"))
}

/// Shared 24×24 toolbar icon button (search-collapse + overflow). Styled
/// like `sort_toolbar_button`; when `active` it carries an accent tint so
/// the open floating field / menu reads as toggled. A tooltip names the
/// action since the glyph alone isn't self-evident.
fn toolbar_icon_button(
    glyph: iced::widget::Text<'static, iced::Theme, iced::Renderer>,
    msg: Message,
    active: bool,
    tip: &'static str,
) -> Element<'static, Message> {
    let inner = button(
        container(glyph.size(15).color(OryxisColors::t().button_text))
            .center_y(Length::Fixed(24.0))
            .center_x(Length::Fixed(24.0)),
    )
    .on_press(msg)
    .style(move |_, status| {
        let bg = match status {
            BtnStatus::Hovered => OryxisColors::t().button_bg_hover,
            _ if active => Color { a: 0.18, ..OryxisColors::t().accent },
            _ => OryxisColors::t().button_bg,
        };
        button::Style {
            background: Some(Background::Color(bg)),
            border: Border { radius: Radius::from(6.0), ..Default::default() },
            ..Default::default()
        }
    });
    iced::widget::tooltip(
        inner,
        container(text(tip).size(11).color(OryxisColors::t().text_primary))
            .padding(Padding { top: 4.0, right: 8.0, bottom: 4.0, left: 8.0 })
            .style(|_| container::Style {
                background: Some(Background::Color(OryxisColors::t().bg_surface)),
                border: Border {
                    radius: Radius::from(6.0),
                    color: OryxisColors::t().border,
                    width: 1.0,
                },
                ..Default::default()
            }),
        iced::widget::tooltip::Position::Bottom,
    )
    .into()
}

/// Search icon shown in the toolbar when the window is too narrow for an
/// inline search field. Clicking it pops the floating search input
/// (`OverlayContent::ToolbarSearch`).
pub(crate) fn toolbar_search_icon(active: bool) -> Element<'static, Message> {
    toolbar_icon_button(
        iced_fonts::lucide::search(),
        Message::Navigation(NavigationMessage::ToggleToolbarSearch),
        active,
        crate::i18n::t("search"),
    )
}

/// Overflow `…` icon that folds the toolbar's secondary actions into a
/// menu (`OverlayContent::ToolbarOverflow`) when even the icon-collapsed
/// search can't free enough room for them inline.
pub(crate) fn toolbar_overflow_icon(active: bool) -> Element<'static, Message> {
    toolbar_icon_button(
        iced_fonts::lucide::ellipsis(),
        Message::Navigation(NavigationMessage::ToggleToolbarOverflow),
        active,
        crate::i18n::t("toolbar_more"),
    )
}

/// Stack a card's trailing affordance (the hover `⋮` kebab, or the
/// idle drill-in chevron) over the card in the shared overlay slot:
/// trailing edge (RTL-aware), vertically centered, 4 px edge pad. The
/// chassis every dashboard card wraps itself in; one copy so a new
/// card can't drift the slot geometry.
pub(crate) fn card_trailing_overlay<'a>(
    card: Element<'a, Message>,
    trailing: Element<'a, Message>,
) -> Element<'a, Message> {
    let rtl = crate::i18n::is_rtl_layout();
    let align = if rtl {
        iced::alignment::Horizontal::Left
    } else {
        iced::alignment::Horizontal::Right
    };
    let pad = if rtl {
        Padding { top: 0.0, right: 0.0, bottom: 0.0, left: 4.0 }
    } else {
        Padding { top: 0.0, right: 4.0, bottom: 0.0, left: 0.0 }
    };
    let overlay = container(trailing)
        .width(Length::Fill)
        .height(Length::Fill)
        .align_x(align)
        .align_y(iced::alignment::Vertical::Center)
        .padding(pad);
    iced::widget::Stack::new().push(card).push(overlay).into()
}

/// Floating `⋮` kebab action button shown on hover over cards (and the
/// SFTP pane toolbar). Fixed 22×22 with the glyph centered, so the hover
/// highlight is a square with a soft radius instead of the wider-than-tall
/// rectangle a horizontally padded glyph produces. 22 matches the reserved
/// slot widths (`SNIP_DOTS_SLOT_W`, `DG_DOTS_SLOT_W`) so the kebab never
/// shifts layout when it replaces an idle placeholder. `show_hover` gates
/// the highlight: pass `false` while the glyph is transparent (card not
/// hovered) so the square doesn't flash as the pointer crosses the slot.
pub(crate) fn card_kebab_button<'a>(
    glyph_color: Color,
    show_hover: bool,
    on_press: Message,
) -> button::Button<'a, Message> {
    button(
        container(text("\u{22EE}").size(14).color(glyph_color))
            .center_x(Length::Fixed(22.0))
            .center_y(Length::Fixed(22.0)),
    )
    .on_press(on_press)
    .padding(0)
    .style(move |_, status| {
        let bg = match status {
            BtnStatus::Hovered if show_hover => OryxisColors::t().bg_hover,
            _ => Color::TRANSPARENT,
        };
        button::Style {
            background: Some(Background::Color(bg)),
            border: Border { radius: Radius::from(6.0), ..Default::default() },
            ..Default::default()
        }
    })
}

/// One row of the toolbar Sort dropdown (Hosts / Keychain / Snippets).
/// Mirrors `context_menu_item` but adds a trailing checkmark when the
/// row matches the current sort. Icon is taken pre-built so the
/// caller can pass any `iced_fonts::lucide::*` glyph (their lifetime
/// is `'static`, which keeps the helper monomorphizable without a
/// closure that would force shorter borrows).
pub(crate) fn sort_menu_row(
    kind: crate::state::SortMenuKind,
    sort: crate::state::ListSort,
    icon: iced::widget::Text<'static, iced::Theme, iced::Renderer>,
    label_key: &'static str,
    is_active: bool,
) -> Element<'static, Message> {
    let check: Element<'static, Message> = if is_active {
        iced_fonts::lucide::check()
            .size(13)
            .color(OryxisColors::t().accent)
            .into()
    } else {
        Space::new().width(13).into()
    };
    button(
        container(
            dir_row(vec![
                icon.size(14)
                    .color(OryxisColors::t().text_secondary)
                    .into(),
                Space::new().width(10).into(),
                text(crate::i18n::t(label_key))
                    .size(12)
                    .color(OryxisColors::t().text_primary)
                    .into(),
                Space::new().width(Length::Fill).into(),
                check,
            ])
            .align_y(iced::Alignment::Center),
        )
        .width(Length::Fill)
        .align_x(dir_align_x()),
    )
    .on_press(Message::Navigation(NavigationMessage::SetListSort(kind, sort)))
    .width(Length::Fill)
    .padding(Padding { top: 6.0, right: 12.0, bottom: 6.0, left: 12.0 })
    .style(|_, status| {
        let bg = match status {
            BtnStatus::Hovered => OryxisColors::t().bg_hover,
            _ => Color::TRANSPARENT,
        };
        button::Style {
            background: Some(Background::Color(bg)),
            border: Border { radius: Radius::from(4.0), ..Default::default() },
            ..Default::default()
        }
    })
    .into()
}

pub(crate) fn context_menu_item<'a>(
    icon: impl Into<crate::os_icon::BrandIcon>,
    label: &'a str,
    msg: Message,
    color: Color,
) -> Element<'a, Message> {
    context_menu_item_chord(icon, label, msg, color, None)
}

/// [`context_menu_item`] with the row's keyboard chord on the trailing
/// edge, which is the only place a chord is ever discovered by someone
/// who does not already know it.
///
/// The chip is a LABEL and not a second door: the row still fires its
/// own message, so an action the terminal WIDGET performs from its
/// canvas state (`HotkeyAction::widget_dispatched`) can advertise the
/// chord that reaches it without the click having to travel that way.
pub(crate) fn context_menu_item_chord<'a>(
    icon: impl Into<crate::os_icon::BrandIcon>,
    label: &'a str,
    msg: Message,
    color: Color,
    chord: Option<String>,
) -> Element<'a, Message> {
    let mut row = vec![
        icon.into().view(14.0, color),
        Space::new().width(8).into(),
        text(label).size(12).color(OryxisColors::t().text_primary).into(),
    ];
    if let Some(chord) = chord.filter(|c| !c.is_empty()) {
        // The gap is elastic so every chip in a menu lines up on the
        // trailing edge whatever its label is long, and it survives RTL
        // because `dir_row` reverses the whole sequence.
        row.push(Space::new().width(Length::Fill).into());
        row.push(Space::new().width(16).into());
        row.push(text(chord).size(11).color(OryxisColors::t().text_muted).into());
    }
    button(
        container(dir_row(row).align_y(iced::Alignment::Center))
            .width(Length::Fill)
            .align_x(dir_align_x()),
    )
    .on_press(msg)
    .width(Length::Fill)
    .padding(Padding { top: 6.0, right: 12.0, bottom: 6.0, left: 12.0 })
    .style(|_, status| {
        let bg = match status {
            BtnStatus::Hovered => OryxisColors::t().bg_hover,
            _ => Color::TRANSPARENT,
        };
        button::Style {
            background: Some(Background::Color(bg)),
            border: Border { radius: Radius::from(4.0), ..Default::default() },
            ..Default::default()
        }
    })
    .into()
}

/// Owned-label sibling of [`context_menu_item`]: the label `String` is
/// moved into the returned element, so a label formatted at render
/// time (counts, substitutions) doesn't need to outlive the call.
pub(crate) fn context_menu_item_owned(
    icon: impl Into<crate::os_icon::BrandIcon>,
    label: String,
    msg: Message,
    color: Color,
) -> Element<'static, Message> {
    button(
        container(
            dir_row(vec![
                icon.into().view(14.0, color),
                Space::new().width(8).into(),
                text(label).size(12).color(OryxisColors::t().text_primary).into(),
            ])
            .align_y(iced::Alignment::Center),
        )
        .width(Length::Fill)
        .align_x(dir_align_x()),
    )
    .on_press(msg)
    .width(Length::Fill)
    .padding(Padding { top: 6.0, right: 12.0, bottom: 6.0, left: 12.0 })
    .style(|_, status| {
        let bg = match status {
            BtnStatus::Hovered => OryxisColors::t().bg_hover,
            _ => Color::TRANSPARENT,
        };
        button::Style {
            background: Some(Background::Color(bg)),
            border: Border { radius: Radius::from(4.0), ..Default::default() },
            ..Default::default()
        }
    })
    .into()
}

/// Tag-filter trigger for the host dashboard toolbar, styled like the
/// sort button. While tags are selected the button fills accent and
/// shows the selection count next to the glyph, so a narrowed list is
/// visibly narrowed and the "how many" question answers itself.
pub(crate) fn tag_filter_toolbar_button(
    selected: usize,
    msg: Message,
) -> Element<'static, Message> {
    let active = selected > 0;
    let mut inner: Vec<Element<'static, Message>> = vec![
        iced_fonts::lucide::tag()
            .size(15)
            .color(OryxisColors::t().button_text)
            .into(),
    ];
    if active {
        inner.push(Space::new().width(4).into());
        inner.push(
            text(selected.to_string())
                .size(11)
                .font(iced::Font {
                    weight: iced::font::Weight::Bold,
                    ..iced::Font::new(crate::theme::SYSTEM_UI_FAMILY)
                })
                .color(OryxisColors::t().button_text)
                .into(),
        );
    }
    button(
        container(dir_row(inner).align_y(iced::Alignment::Center))
            .center_y(Length::Fixed(24.0))
            .center_x(Length::Shrink)
            .padding(Padding { top: 0.0, right: 4.0, bottom: 0.0, left: 4.0 }),
    )
    .on_press(msg)
    .style(move |_, status| {
        let bg = if active {
            OryxisColors::t().accent
        } else {
            match status {
                BtnStatus::Hovered => OryxisColors::t().button_bg_hover,
                _ => OryxisColors::t().button_bg,
            }
        };
        button::Style {
            background: Some(Background::Color(bg)),
            border: Border { radius: Radius::from(6.0), ..Default::default() },
            ..Default::default()
        }
    })
    .into()
}
