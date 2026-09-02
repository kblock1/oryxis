//! Multi-host monitor dashboard view (issue #95): one vitals card per
//! opted-in host, fed by the same rings as the per-session sidebar.
//!
//! A sub-nav pill (gated by the master monitoring toggle) is the way
//! in; a direct navigation with the toggle off just shows the empty
//! state. The toolbar carries a search filter, the shared host-tag
//! filter and a grid/list toggle; both filters are display-only
//! lenses, the whole fleet keeps polling behind them. Clicking a card
//! opens the host's detail panel on the trailing edge (owner call on
//! the first live build: a card click must never open an SSH session
//! by itself); connecting is the panel's explicit action. Cards and
//! panel actions are recorded as generic content actions per the
//! keynav rule.

use iced::border::Radius;
use iced::widget::button::Status as BtnStatus;
use iced::widget::{button, column, container, scrollable, text, Space};
use iced::{Background, Border, Color, Element, Length, Padding};

use crate::app::{Message, MonitorMessage, NavigationMessage, Oryxis};
use crate::i18n::t;
use crate::state::DashLink;
use crate::theme::OryxisColors;
use crate::widgets::dir_row;

use super::sidebar_monitor::{fmt_bytes_short, fmt_uptime, gauge_block};

/// Card width in grid mode, matching the host cards so the two grids
/// read as one family.
const CARD_W: f32 = crate::app::CARD_WIDTH;

impl Oryxis {
    pub(crate) fn view_monitor_dash(&self) -> Element<'_, Message> {
        // Content actions are recorded during render: without this
        // clear the 1 s heartbeat re-render would append the whole
        // surface again every tick, and the keyboard walk would drown
        // in stale duplicate rows (live owner report, 05/08).
        self.keynav_clear_content();
        let fleet = self.dash_hosts();

        if fleet.is_empty() {
            // Two different empty boards, and telling the user to opt a
            // host in when they already have is how an empty screen
            // becomes a dead end: with the live-only filter on, hosts
            // ARE opted in and the board is waiting for a session
            // instead (issue #197).
            let waiting_for_session = self.prefs.monitor_dash_live_only
                && self.connections.iter().any(|c| self.monitor_conn_opted_in(c));
            let (empty, hint) = if waiting_for_session {
                ("monitor_dash_empty_live", "monitor_dash_empty_live_hint")
            } else {
                ("monitor_dash_empty", "monitor_dash_empty_hint")
            };
            return container(
                column![
                    text(t(empty))
                        .size(15)
                        .color(OryxisColors::t().text_primary),
                    Space::new().height(8),
                    text(t(hint))
                        .size(12)
                        .color(OryxisColors::t().text_muted),
                ]
                .align_x(iced::Alignment::Center),
            )
            .width(Length::Fill)
            .height(Length::Fill)
            .center_x(Length::Fill)
            .center_y(Length::Fill)
            .into();
        }

        // ── Toolbar: search + shared tag filter + grid/list toggle ──
        let tag_filter_btn: Element<'_, Message> = if self.host_tag_filter_available() {
            dir_row(vec![
                self.keynav_toolbar_ring(
                    crate::keynav::ToolbarItem::TagFilter,
                    crate::widgets::bounds_reporter(
                        crate::widgets::tag_filter_toolbar_button(
                            self.host_filter_tags.len(),
                            Message::Navigation(NavigationMessage::ShowHostTagFilterMenu),
                        ),
                        self.host_tag_filter_btn_bounds.clone(),
                    ),
                ),
                Space::new().width(6).into(),
            ])
            .align_y(iced::Alignment::Center)
            .into()
        } else {
            Space::new().into()
        };
        // Pause + Refresh (issue #156 follow-up): the fleet is the only
        // surface that opens connections on its own, so "stop reading
        // my servers" and "read them once" belong here rather than in
        // Settings, where the persistent answers already live.
        let paused = self.monitor_dash.paused;
        let pause_btn = self.keynav_toolbar_ring(
            crate::keynav::ToolbarItem::MonitorPause,
            dash_toolbar_icon(
                if paused {
                    iced_fonts::lucide::play()
                } else {
                    iced_fonts::lucide::pause()
                },
                Message::Monitor(MonitorMessage::DashTogglePause),
                t(if paused {
                    "monitor_dash_resume"
                } else {
                    "monitor_dash_pause"
                }),
                paused,
            ),
        );
        let refresh_btn = self.keynav_toolbar_ring(
            crate::keynav::ToolbarItem::MonitorRefresh,
            dash_toolbar_icon(
                iced_fonts::lucide::refresh_cw(),
                Message::Monitor(MonitorMessage::DashRefreshNow),
                t("monitor_dash_refresh"),
                false,
            ),
        );
        let view_toggle = self.keynav_toolbar_ring(
            crate::keynav::ToolbarItem::ViewToggle,
            dash_view_toggle_button(self.prefs.monitor_dash_list_view),
        );
        self.keynav_toolbar_reset();
        if self.host_tag_filter_available() {
            self.keynav_toolbar_record(crate::keynav::ToolbarItem::TagFilter);
        }
        self.keynav_toolbar_record(crate::keynav::ToolbarItem::MonitorPause);
        self.keynav_toolbar_record(crate::keynav::ToolbarItem::MonitorRefresh);
        self.keynav_toolbar_record(crate::keynav::ToolbarItem::ViewToggle);

        let toolbar = container(
            dir_row(vec![
                self.vault_search_field(),
                Space::new().width(10).into(),
                tag_filter_btn,
                pause_btn,
                Space::new().width(6).into(),
                refresh_btn,
                Space::new().width(6).into(),
                view_toggle,
            ])
            .align_y(iced::Alignment::Center),
        )
        .padding(Padding { top: 16.0, right: 24.0, bottom: 16.0, left: 24.0 })
        .width(Length::Fill);

        // ── Filtered fleet (lenses only; polling covers everything) ──
        let needle = self.monitor_dash.search.trim().to_lowercase();
        let hosts: Vec<uuid::Uuid> = fleet
            .into_iter()
            .filter(|id| {
                let Some(conn) = self.connections.iter().find(|c| c.id == *id) else {
                    return false;
                };
                let by_search =
                    needle.is_empty() || conn.label.to_lowercase().contains(&needle);
                let by_tags = self.host_filter_tags.is_empty()
                    || conn.tags.iter().any(|t| self.host_filter_tags.contains(t));
                by_search && by_tags
            })
            .collect();

        // ── Card grid ──
        // Grid mode: responsive fixed-width cards. List mode: at most
        // two full-width columns (the owner's "list (2 columns)").
        let panel_open = self.monitor_dash.selected.is_some();
        let panel_w = if panel_open { self.panel_width } else { 0.0 };
        let nav_width = self.vault_rail_width();
        let available = (self.window_size.width
            - nav_width
            - self.side_strip_reserve()
            - panel_w
            - 48.0)
            .max(0.0);
        let list_mode = self.prefs.monitor_dash_list_view;
        let body: Element<'_, Message> = if list_mode {
            self.dash_table(hosts)
        } else {
            let cols = crate::widgets::card_grid_columns(available, CARD_W, 12.0).max(1);
            let mut grid = column![].spacing(12);
            for chunk in hosts.chunks(cols) {
                let mut row_items: Vec<Element<'_, Message>> = Vec::new();
                for conn_id in chunk {
                    row_items.push(self.dash_card(*conn_id));
                    row_items.push(Space::new().width(12).into());
                }
                grid = grid.push(dir_row(row_items).align_y(iced::Alignment::Start));
            }
            grid.into()
        };

        let content = column![
            toolbar,
            scrollable(
                container(body)
                    .padding(Padding { top: 0.0, right: 24.0, bottom: 24.0, left: 24.0 })
                    .width(Length::Fill),
            )
            // Stable id so the keyboard walk can keep the selected
            // row scrolled into view (content_scroll_meta).
            .id(iced::widget::Id::new("monitor-dash-scroll"))
            .height(Length::Fill),
        ];

        match self.monitor_dash.selected {
            Some(conn_id) => dir_row(vec![
                container(content).width(Length::Fill).height(Length::Fill).into(),
                self.dash_detail_panel(conn_id),
            ])
            .into(),
            None => content.into(),
        }
    }

    /// One host's vitals card. Click (and Enter, via the recorded
    /// content action) opens the detail panel; nothing on the card
    /// itself touches the host.
    fn dash_card(&self, conn_id: uuid::Uuid) -> Element<'_, Message> {
        let Some(conn) = self.connections.iter().find(|c| c.id == conn_id) else {
            return Space::new().into();
        };
        // Both the link and the window belong to the MACHINE (issue
        // #156): the cards of rows that point at one server show the
        // same reading, taken at the same instant, because there is
        // only one of each.
        let key = self.monitor_key(&conn_id);
        let link = key.as_ref().and_then(|k| self.monitor_dash.links.get(k));
        // A paused board keeps its last reading on screen, but greyed:
        // the numbers are true, they are just not being refreshed, and
        // a live dot over a frozen sample is a lie.
        let (dot, dot_color) = match link {
            _ if self.monitor_dash.paused => ("●", OryxisColors::t().text_muted),
            Some(DashLink::Live { .. }) => ("●", OryxisColors::t().success),
            Some(DashLink::Connecting { .. }) | None => ("●", OryxisColors::t().warning),
            Some(DashLink::Failed { .. }) => ("●", OryxisColors::t().error),
        };

        let latest = self.monitor_sample(&conn_id);

        let mut body = column![].spacing(8);
        match link {
            Some(DashLink::Failed { via, error, .. }) => {
                body = body.push(
                    text(self.dash_via_prefixed(*via, conn_id, error))
                        .size(11)
                        .color(OryxisColors::t().error),
                );
            }
            _ => {
                if let Some(sample) = latest {
                    // CPU renders from the very first sample: the rate
                    // needs two ticks, so until then the bar is empty
                    // and the value an honest "-", never a fake 0%.
                    body = body.push(match &sample.cpu {
                        Some(cpu) => gauge_block(
                            t("monitor_cpu"),
                            cpu.pct,
                            &format!("{:.0}%", cpu.pct),
                        ),
                        None => gauge_block(t("monitor_cpu"), 0.0, "-"),
                    });
                    if let Some(mem) = &sample.mem
                        && mem.total > 0
                    {
                        body = body.push(gauge_block(
                            t("monitor_mem"),
                            mem.pct(),
                            &format!(
                                "{} / {}",
                                fmt_bytes_short(mem.used),
                                fmt_bytes_short(mem.total)
                            ),
                        ));
                    }
                    // One compact stats line: net rates, the fullest
                    // disk, GPU utilization when a GPU answered.
                    let mut stats: Vec<String> = Vec::new();
                    match &sample.net {
                        Some(net) => stats.push(format!(
                            "\u{2193}{}/s \u{2191}{}/s",
                            fmt_bytes_short(net.rx_bps),
                            fmt_bytes_short(net.tx_bps)
                        )),
                        None => stats.push("\u{2193}- \u{2191}-".to_string()),
                    }
                    if let Some(disk) = sample
                        .disks
                        .iter()
                        .filter(|d| d.total > 0)
                        .max_by(|a, b| a.pct().total_cmp(&b.pct()))
                    {
                        stats.push(format!("{} {:.0}%", t("monitor_disk"), disk.pct()));
                    }
                    if let Some(gpu) = sample.gpus.first() {
                        stats.push(format!("{} {:.0}%", t("monitor_gpu"), gpu.util_pct));
                    }
                    body = body.push(
                        text(stats.join("   "))
                            .size(11)
                            .color(OryxisColors::t().text_secondary),
                    );
                } else {
                    body = body.push(
                        text(match link {
                            _ if self.monitor_dash.paused => t("monitor_dash_paused"),
                            Some(DashLink::Live { .. }) => t("monitor_sampling"),
                            _ => t("monitor_dash_connecting"),
                        })
                        .size(11)
                        .color(OryxisColors::t().text_muted),
                    );
                }
            }
        }

        let uptime: Element<'_, Message> = match latest.and_then(|s| s.uptime_secs) {
            Some(secs) => text(fmt_uptime(secs))
                .size(10)
                .color(OryxisColors::t().text_muted)
                .into(),
            None => Space::new().into(),
        };

        let inner = column![
            dir_row(vec![
                text(dot).size(10).color(dot_color).into(),
                Space::new().width(6).into(),
                text(conn.label.clone())
                    .size(13)
                    .color(OryxisColors::t().text_primary)
                    .width(Length::Fill)
                    .into(),
                uptime,
            ])
            .align_y(iced::Alignment::Center),
            Space::new().height(10),
            body,
        ];

        let selected = self.monitor_dash.selected == Some(conn_id);
        let msg = Message::Monitor(MonitorMessage::DashSelectHost(conn_id));
        let card = button(container(inner).padding(14).width(Length::Fixed(CARD_W)))
            .on_press(msg.clone())
            .padding(0)
            .width(Length::Fixed(CARD_W))
            .style(move |_, status| {
                let bg = match status {
                    BtnStatus::Hovered => OryxisColors::t().bg_hover,
                    BtnStatus::Pressed => OryxisColors::t().bg_selected,
                    _ => OryxisColors::t().bg_surface,
                };
                button::Style {
                    background: Some(Background::Color(bg)),
                    border: Border {
                        radius: Radius::from(10.0),
                        color: if selected {
                            OryxisColors::t().accent
                        } else {
                            OryxisColors::t().border
                        },
                        width: 1.0,
                    },
                    ..Default::default()
                }
            });
        self.content_action_slot(
            crate::keynav::RowAction::activate(msg),
            10.0,
            card.into(),
        )
    }

    /// A message about a shared link, prefixed with the row it came
    /// through when that is not the card's own.
    ///
    /// The vitals belong to the machine, but the credentials that
    /// fetched them belong to one row, and an unlabelled error reads as
    /// "this card's host refused me" when it was a sibling's login that
    /// did (issue #156).
    fn dash_via_prefixed(&self, via: uuid::Uuid, card: uuid::Uuid, message: &str) -> String {
        if via == card {
            return message.to_string();
        }
        match self.connections.iter().find(|c| c.id == via) {
            Some(conn) => format!("{}: {message}", conn.label),
            None => message.to_string(),
        }
    }

    /// The trailing-edge detail panel: full vitals of the selected
    /// host, plus the explicit actions (open terminal / retry).
    fn dash_detail_panel(&self, conn_id: uuid::Uuid) -> Element<'_, Message> {
        let label = self
            .connections
            .iter()
            .find(|c| c.id == conn_id)
            .map(|c| c.label.clone())
            .unwrap_or_default();
        let key = self.monitor_key(&conn_id);
        let link = key.as_ref().and_then(|k| self.monitor_dash.links.get(k));

        let close_btn = button(
            container(
                iced_fonts::lucide::x()
                    .size(14)
                    .color(OryxisColors::t().text_secondary),
            )
            .center_y(Length::Fixed(22.0))
            .center_x(Length::Fixed(22.0)),
        )
        .on_press(Message::Monitor(MonitorMessage::DashCloseDetail))
        .padding(0)
        .style(|_, status| {
            let bg = match status {
                BtnStatus::Hovered => OryxisColors::t().bg_hover,
                BtnStatus::Pressed => OryxisColors::t().bg_selected,
                _ => Color::TRANSPARENT,
            };
            button::Style {
                background: Some(Background::Color(bg)),
                border: Border { radius: Radius::from(6.0), ..Default::default() },
                ..Default::default()
            }
        });
        let close = self.content_action_slot(
            crate::keynav::RowAction::activate(Message::Monitor(
                MonitorMessage::DashCloseDetail,
            )),
            6.0,
            crate::views::terminal::icon_tooltip(close_btn.into(), crate::i18n::t("close")),
        );

        // The vitals themselves come from the SAME renderer as the
        // terminal's Monitor sidebar tab (owner call: the two surfaces
        // must present a host identically, collapsible Disks / Ports
        // sections included).
        let body: Element<'_, Message> = match link {
            Some(DashLink::Failed { via, error, .. }) => column![
                text(self.dash_via_prefixed(*via, conn_id, error))
                    .size(12)
                    .color(OryxisColors::t().error),
                Space::new().height(12),
                self.content_action_slot(
                    crate::keynav::RowAction::activate(Message::Monitor(
                        MonitorMessage::DashRetry(conn_id),
                    )),
                    6.0,
                    crate::widgets::styled_button(
                        t("retry"),
                        Message::Monitor(MonitorMessage::DashRetry(conn_id)),
                        OryxisColors::t().accent,
                    ),
                ),
            ]
            .into(),
            _ => match self.monitor_vitals_body(
                conn_id,
                super::sidebar_monitor::MonitorVitalsSurface::Dashboard,
            ) {
                Some(body) => body,
                None => text(match link {
                    Some(DashLink::Live { .. }) => t("monitor_sampling"),
                    _ => t("monitor_dash_connecting"),
                })
                .size(12)
                .color(OryxisColors::t().text_muted)
                .into(),
            },
        };

        // A machine is read once, through one of its rows (issue #156).
        // When that row is not this one, the panel says so: identical
        // numbers on several cards are a shared reading, not a
        // coincidence, and the user should know which login produced
        // them before reading anything into the values.
        let via_hint: Element<'_, Message> = match link.map(|l| l.via()) {
            Some(via) if via != conn_id => {
                let name = self
                    .connections
                    .iter()
                    .find(|c| c.id == via)
                    .map(|c| c.label.clone())
                    .unwrap_or_default();
                column![
                    Space::new().height(10),
                    text(t("monitor_dash_sampled_via").replacen("{host}", &name, 1))
                        .size(11)
                        .color(OryxisColors::t().text_muted),
                ]
                .into()
            }
            _ => Space::new().into(),
        };

        // Explicit connect action: the one place the dashboard opens a
        // terminal (focuses an existing tab when one is live).
        let open_btn = self.content_action_slot(
            crate::keynav::RowAction::activate(Message::Monitor(
                MonitorMessage::DashOpenHost(conn_id),
            )),
            6.0,
            crate::widgets::styled_button(
                t("monitor_dash_open_terminal"),
                Message::Monitor(MonitorMessage::DashOpenHost(conn_id)),
                OryxisColors::t().accent,
            ),
        );

        container(
            column![
                dir_row(vec![
                    text(label)
                        .size(15)
                        .color(OryxisColors::t().text_primary)
                        .width(Length::Fill)
                        .into(),
                    close,
                ])
                .align_y(iced::Alignment::Center),
                Space::new().height(14),
                scrollable(body).height(Length::Fill),
                via_hint,
                Space::new().height(12),
                open_btn,
            ]
            .padding(Padding { top: 16.0, right: 16.0, bottom: 16.0, left: 16.0 }),
        )
        .width(Length::Fixed(self.panel_width))
        .height(Length::Fill)
        .style(|_| container::Style {
            background: Some(Background::Color(OryxisColors::t().bg_surface)),
            border: Border {
                color: OryxisColors::t().border,
                width: 1.0,
                ..Default::default()
            },
            ..Default::default()
        })
        .into()
    }
}

impl Oryxis {
    /// Table (list) mode: one row per host, metric columns, sortable
    /// by clicking a header (owner call: a list is a table, not wider
    /// cards). Row click selects the host like a card click.
    fn dash_table(&self, hosts: Vec<uuid::Uuid>) -> Element<'_, Message> {
        use crate::state::DashSortKey as K;

        // Latest-sample metrics per host, `None` sorting last so hosts
        // that haven't answered yet sink to the bottom either way.
        let metrics = |id: &uuid::Uuid| {
            let latest = self.monitor_sample(id);
            let cpu = latest.and_then(|s| s.cpu.map(|c| c.pct));
            let mem = latest.and_then(|s| s.mem.map(|m| m.pct()));
            let net = latest
                .and_then(|s| s.net.map(|n| n.rx_bps.saturating_add(n.tx_bps)));
            let disk = latest.and_then(|s| {
                s.disks
                    .iter()
                    .filter(|d| d.total > 0)
                    .map(|d| d.pct())
                    .max_by(f32::total_cmp)
            });
            let uptime = latest.and_then(|s| s.uptime_secs);
            (cpu, mem, net, disk, uptime)
        };

        let key = self.monitor_dash.sort_key;
        let asc = self.monitor_dash.sort_asc;
        let mut rows: Vec<uuid::Uuid> = hosts;
        // `hosts` arrives label-sorted ascending (dash_hosts), so the
        // Label key only needs the direction flip.
        match key {
            K::Label => {}
            _ => {
                let rank = |id: &uuid::Uuid| -> Option<f64> {
                    let (cpu, mem, net, disk, uptime) = metrics(id);
                    match key {
                        K::Label => None,
                        K::Cpu => cpu.map(f64::from),
                        K::Mem => mem.map(f64::from),
                        K::Net => net.map(|n| n as f64),
                        K::Disk => disk.map(f64::from),
                        K::Uptime => uptime.map(|u| u as f64),
                    }
                };
                // Stable sort keeps the label order inside ties.
                rows.sort_by(|a, b| match (rank(a), rank(b)) {
                    (Some(x), Some(y)) => x.total_cmp(&y),
                    (Some(_), None) => std::cmp::Ordering::Less,
                    (None, Some(_)) => std::cmp::Ordering::Greater,
                    (None, None) => std::cmp::Ordering::Equal,
                });
            }
        }
        if !asc {
            rows.reverse();
        }

        // Header: one button per column; the active column carries the
        // direction glyph. Widths mirror the row cells below.
        let header_cell = |label: &str, col: K, width: Length| -> Element<'_, Message> {
            let marker = if key == col {
                if asc { " \u{25b4}" } else { " \u{25be}" }
            } else {
                ""
            };
            let msg = Message::Monitor(MonitorMessage::DashSortBy(col));
            self.content_action_slot(
                crate::keynav::RowAction::activate(msg.clone()),
                4.0,
                button(
                    text(format!("{label}{marker}"))
                        .size(11)
                        .color(OryxisColors::t().text_secondary),
                )
                .on_press(msg)
                .padding(Padding { top: 4.0, right: 6.0, bottom: 4.0, left: 6.0 })
                .width(width)
                .style(|_, status| {
                    let bg = match status {
                        BtnStatus::Hovered | BtnStatus::Pressed => {
                            OryxisColors::t().bg_hover
                        }
                        _ => Color::TRANSPARENT,
                    };
                    button::Style {
                        background: Some(Background::Color(bg)),
                        border: Border {
                            radius: Radius::from(4.0),
                            ..Default::default()
                        },
                        ..Default::default()
                    }
                })
                .into(),
            )
        };
        const W_CPU: f32 = 70.0;
        const W_MEM: f32 = 150.0;
        const W_NET: f32 = 150.0;
        const W_DISK: f32 = 80.0;
        const W_UP: f32 = 90.0;
        let header = dir_row(vec![
            header_cell(t("hosts"), K::Label, Length::Fill),
            header_cell(t("monitor_cpu"), K::Cpu, Length::Fixed(W_CPU)),
            header_cell(t("monitor_mem"), K::Mem, Length::Fixed(W_MEM)),
            header_cell(t("monitor_net"), K::Net, Length::Fixed(W_NET)),
            header_cell(t("monitor_disk"), K::Disk, Length::Fixed(W_DISK)),
            header_cell(t("monitor_uptime"), K::Uptime, Length::Fixed(W_UP)),
        ])
        .align_y(iced::Alignment::Center);

        let mut table = column![header, Space::new().height(4)].spacing(2);
        for conn_id in rows {
            let Some(conn) = self.connections.iter().find(|c| c.id == conn_id) else {
                continue;
            };
            let link = self
                .monitor_key(&conn_id)
                .and_then(|k| self.monitor_dash.links.get(&k).cloned());
            let dot_color = match link {
                _ if self.monitor_dash.paused => OryxisColors::t().text_muted,
                Some(DashLink::Live { .. }) => OryxisColors::t().success,
                Some(DashLink::Connecting { .. }) | None => OryxisColors::t().warning,
                Some(DashLink::Failed { .. }) => OryxisColors::t().error,
            };
            let (cpu, _mem_pct, _net, disk, uptime) = metrics(&conn_id);
            let latest = self.monitor_sample(&conn_id);
            let cell = |value: String, width: f32| -> Element<'_, Message> {
                text(value)
                    .size(11)
                    .font(iced::Font::MONOSPACE)
                    .color(OryxisColors::t().text_primary)
                    .width(Length::Fixed(width))
                    .into()
            };
            let host_cell: Element<'_, Message> = dir_row(vec![
                text("●").size(9).color(dot_color).into(),
                Space::new().width(6).into(),
                text(conn.label.clone())
                    .size(12)
                    .color(OryxisColors::t().text_primary)
                    .into(),
            ])
            .align_y(iced::Alignment::Center)
            .width(Length::Fill)
            .into();
            let mem_text = match latest.and_then(|s| s.mem) {
                Some(m) if m.total > 0 => format!(
                    "{} / {}",
                    fmt_bytes_short(m.used),
                    fmt_bytes_short(m.total)
                ),
                _ => "-".into(),
            };
            let net_text = match latest.and_then(|s| s.net) {
                Some(n) => format!(
                    "\u{2193}{}/s \u{2191}{}/s",
                    fmt_bytes_short(n.rx_bps),
                    fmt_bytes_short(n.tx_bps)
                ),
                None => "-".into(),
            };
            let row_inner = dir_row(vec![
                host_cell,
                cell(cpu.map_or("-".into(), |c| format!("{c:.0}%")), W_CPU),
                cell(mem_text, W_MEM),
                cell(net_text, W_NET),
                cell(disk.map_or("-".into(), |d| format!("{d:.0}%")), W_DISK),
                cell(uptime.map_or("-".into(), fmt_uptime), W_UP),
            ])
            .align_y(iced::Alignment::Center);

            let selected = self.monitor_dash.selected == Some(conn_id);
            let msg = Message::Monitor(MonitorMessage::DashSelectHost(conn_id));
            let row = button(
                container(row_inner)
                    .padding(Padding { top: 6.0, right: 6.0, bottom: 6.0, left: 6.0 })
                    .width(Length::Fill),
            )
            .on_press(msg.clone())
            .padding(0)
            .width(Length::Fill)
            .style(move |_, status| {
                let bg = match status {
                    BtnStatus::Hovered => OryxisColors::t().bg_hover,
                    BtnStatus::Pressed => OryxisColors::t().bg_selected,
                    _ => Color::TRANSPARENT,
                };
                button::Style {
                    background: Some(Background::Color(bg)),
                    border: Border {
                        radius: Radius::from(6.0),
                        color: if selected {
                            OryxisColors::t().accent
                        } else {
                            Color::TRANSPARENT
                        },
                        width: 1.0,
                    },
                    ..Default::default()
                }
            });
            table = table.push(self.content_action_slot(
                crate::keynav::RowAction::activate(msg),
                6.0,
                row.into(),
            ));
        }
        table.into()
    }
}

/// Icon button for the monitor dashboard toolbar, in the same family
/// as the grid/list toggle. `active` tints it like a pressed state, so
/// a paused board says so without a second label.
fn dash_toolbar_icon(
    glyph: iced::widget::Text<'static, iced::Theme, iced::Renderer>,
    message: Message,
    tip: &'static str,
    active: bool,
) -> Element<'static, Message> {
    let btn = button(
        container(glyph.size(15).color(if active {
            OryxisColors::t().accent
        } else {
            OryxisColors::t().button_text
        }))
        .center_y(Length::Fixed(24.0))
        .center_x(Length::Fixed(24.0)),
    )
    .on_press(message)
    .style(move |_, status| {
        let c = OryxisColors::t();
        let bg = match status {
            BtnStatus::Hovered | BtnStatus::Pressed => c.button_bg_hover,
            _ if active => Color { a: 0.18, ..c.accent },
            _ => c.button_bg,
        };
        button::Style {
            background: Some(Background::Color(bg)),
            border: Border { radius: Radius::from(6.0), ..Default::default() },
            ..Default::default()
        }
    });
    crate::views::terminal::icon_tooltip(btn.into(), tip)
}

/// Grid/List toggle for the monitor dashboard toolbar, mirroring the
/// host grid's toggle button: the glyph shows the CURRENT mode, same
/// as `host_view_toggle_button` (the two live in the same card-grid
/// family and were reading opposite ways).
fn dash_view_toggle_button(list_view: bool) -> Element<'static, Message> {
    let glyph: iced::widget::Text<'static, iced::Theme, iced::Renderer> = if list_view {
        iced_fonts::lucide::list()
    } else {
        iced_fonts::lucide::layout_grid()
    };
    let btn = button(
        container(
            glyph.size(15).color(OryxisColors::t().button_text),
        )
        .center_y(Length::Fixed(24.0))
        .center_x(Length::Fixed(24.0)),
    )
    .on_press(Message::Monitor(MonitorMessage::DashToggleListView))
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
