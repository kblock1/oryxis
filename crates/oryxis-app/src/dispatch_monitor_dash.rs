//! `Oryxis::handle_monitor_dash`: the multi-host monitor dashboard
//! (issue #95).
//!
//! One link per monitored MACHINE (issue #156, not per card: the rows
//! that point at one server share its window): a live terminal tab's
//! session when any of them has one, otherwise a headless probe-only
//! [`oryxis_ssh::MonitorConn`] dialed with one row's stored credentials
//! (strict host key, TOTP autofill; auth that would need an interactive
//! answer fails onto the card, and the card's open-terminal action is
//! the interactive path out). Samples land through the same
//! `MonitorMessage::Sampled` handler the sidebar uses, into the same
//! rings, so the two surfaces can never disagree. Polling only runs
//! while the view is up; leaving it arms an idle TTL that closes the
//! dialed connections.

use iced::Task;
use uuid::Uuid;

use crate::monitor::endpoint::MonitorKey;
use crate::state::{DashLink, DashTransport};
use crate::app::{Message, MonitorMessage, Oryxis};

/// Cap on a single dashboard probe, mirroring the sidebar's.
const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(8);

/// How long dialed connections survive after the user leaves the view.
/// Long enough that a quick round-trip elsewhere doesn't redial the
/// fleet, short enough that closed dashboards don't hold idle logins.
const IDLE_TTL: std::time::Duration = std::time::Duration::from_secs(60);

impl Oryxis {
    pub(crate) fn handle_monitor_dash(
        &mut self,
        message: MonitorMessage,
    ) -> Result<Task<Message>, MonitorMessage> {
        match message {
            MonitorMessage::DashTick => Ok(self.dash_tick()),
            MonitorMessage::DashDialed(key, via, stamp, result) => {
                // A sweep while the dial was in flight: the link map it
                // would land in no longer exists, so a successful dial
                // is closed instead of leaked.
                if stamp != self.monitor_dash.stamp {
                    if let Ok(conn) = &result {
                        conn.close();
                    }
                    return Ok(Task::none());
                }
                // The slot has to still be waiting for THIS row. A card
                // retry (or the machine leaving the fleet) can restart
                // the round while the dial is on the wire, and landing
                // on the round that replaced it would bury a fresh
                // `Connecting` under a stale answer.
                let tried: Vec<Uuid> = match self.monitor_dash.links.get(&key) {
                    Some(DashLink::Connecting { via: waiting, tried }) if *waiting == via => {
                        tried.clone()
                    }
                    _ => {
                        if let Ok(conn) = &result {
                            conn.close();
                        }
                        return Ok(Task::none());
                    }
                };
                match result {
                    Ok(conn) => {
                        let transport = DashTransport::Pool(conn);
                        self.monitor_dash.links.insert(
                            key.clone(),
                            DashLink::Live {
                                via,
                                transport: transport.clone(),
                                tried: tried.clone(),
                            },
                        );
                        // First sample now, so the card fills in instead
                        // of waiting out the stagger.
                        Ok(self.dash_probe(key, via, transport))
                    }
                    Err(e) => {
                        self.monitor_dash.links.insert(
                            key,
                            DashLink::Failed {
                                via,
                                error: e,
                                tried,
                            },
                        );
                        // The next tick tries the machine's remaining
                        // rows, if it has any: one row's credentials
                        // failing is not the server being down.
                        Ok(Task::none())
                    }
                }
            }
            MonitorMessage::DashRetry(conn_id) => {
                // Only a failed card offers the retry; a live one has
                // nothing to retry and a connecting one is already busy.
                // The retry dials THIS card's row and starts a fresh
                // round, so retrying from the card whose credentials
                // the user just fixed uses them.
                let Some(key) = self.monitor_key(&conn_id) else {
                    return Ok(Task::none());
                };
                if matches!(
                    self.monitor_dash.links.get(&key),
                    Some(DashLink::Failed { .. })
                ) {
                    self.monitor_dash.links.remove(&key);
                    if let Some(task) = self.dash_adopt_live(&key, Vec::new()) {
                        return Ok(task);
                    }
                    return Ok(self.dash_dial(&key, conn_id, Vec::new()));
                }
                Ok(Task::none())
            }
            MonitorMessage::DashSweepDue(stamp) => {
                // Back on the view: the TTL did its job, keep the links.
                if self.active_view == crate::state::View::Monitoring {
                    return Ok(Task::none());
                }
                if stamp == self.monitor_dash.stamp {
                    self.monitor_dash.sweep();
                }
                Ok(Task::none())
            }
            MonitorMessage::DashSelectHost(conn_id) => {
                self.monitor_dash.selected = Some(conn_id);
                Ok(Task::none())
            }
            MonitorMessage::DashCloseDetail => {
                self.monitor_dash.selected = None;
                Ok(Task::none())
            }
            MonitorMessage::DashSearchChanged(s) => {
                self.monitor_dash.search = s;
                Ok(Task::none())
            }
            MonitorMessage::DashSortBy(key) => {
                if self.monitor_dash.sort_key == key {
                    self.monitor_dash.sort_asc = !self.monitor_dash.sort_asc;
                } else {
                    self.monitor_dash.sort_key = key;
                    // Metrics start descending (the hot host first is
                    // what a fleet sort is for); labels start A-z.
                    self.monitor_dash.sort_asc =
                        matches!(key, crate::state::DashSortKey::Label);
                }
                Ok(Task::none())
            }
            MonitorMessage::DashTogglePause => {
                self.monitor_dash.paused = !self.monitor_dash.paused;
                // Resuming re-links and samples right away, so the
                // board is live again on the click rather than at the
                // end of the next interval. Pausing leaves the closing
                // to the tick, which knows what is still in flight.
                if self.monitor_dash.paused {
                    Ok(Task::none())
                } else {
                    Ok(self.dash_enter())
                }
            }
            MonitorMessage::DashRefreshNow => {
                // One round for every machine on the board, links
                // included. While paused this is the single-shot
                // monitor: the tick closes what this opened as soon as
                // the sample lands, so nothing is held between clicks.
                Ok(self.dash_round(false))
            }
            MonitorMessage::DashToggleListView => {
                self.prefs.monitor_dash_list_view = !self.prefs.monitor_dash_list_view;
                self.persist_setting(
                    "monitor_dash_list_view",
                    if self.prefs.monitor_dash_list_view { "true" } else { "false" },
                );
                Ok(Task::none())
            }
            MonitorMessage::DashOpenHost(conn_id) => {
                // An existing tab wins; otherwise the normal connect
                // flow (progress screen, prompts and all).
                if let Some(idx) = self.tab_index_for_host(conn_id) {
                    return Ok(Task::done(Message::Tabs(
                        crate::app::TabsMessage::SelectTab(idx),
                    )));
                }
                if let Some(idx) =
                    self.connections.iter().position(|c| c.id == conn_id)
                {
                    return Ok(Task::done(Message::Ssh(
                        crate::app::SshMessage::ConnectSsh(idx),
                    )));
                }
                Ok(Task::none())
            }
            m => Err(m),
        }
    }

    /// The opted-in fleet, sorted by label so the grid order (and the
    /// probe stagger derived from the position) is stable across
    /// re-renders and reboots.
    pub(crate) fn dash_hosts(&self) -> Vec<Uuid> {
        // "Only hosts with a live session" (issue #197): the fleet is
        // the one surface that opens connections nobody asked for, so
        // the answer is to narrow WHAT IT IS, not to keep the board and
        // suppress the dialing. A machine off the board holds no link,
        // gets no probe and leaves no card that would have to explain
        // why it is empty. The keys are collected once rather than per
        // row: this runs on every render and every heartbeat.
        let live = self
            .prefs
            .monitor_dash_live_only
            .then(|| self.dash_live_keys());
        let mut hosts: Vec<(String, Uuid)> = self
            .connections
            .iter()
            .filter(|c| self.monitor_conn_opted_in(c))
            .filter(|c| live.as_ref().is_none_or(|keys| keys.contains(&MonitorKey::new(c))))
            .map(|c| (c.label.to_lowercase(), c.id))
            .collect();
        hosts.sort();
        hosts.into_iter().map(|(_, id)| id).collect()
    }

    /// The MACHINES a terminal tab is logged in to right now, keyed the
    /// way the dashboard keys everything else, so a row whose own
    /// session is closed still counts when a sibling row reaches the
    /// same server (issue #156's rule, applied to the filter above).
    fn dash_live_keys(&self) -> std::collections::HashSet<MonitorKey> {
        let mut keys = std::collections::HashSet::new();
        for tab in &self.tabs {
            for pane in tab.pane_grid.panes.values() {
                let crate::state::PaneOrigin::Host(id) = pane.origin else {
                    continue;
                };
                if pane
                    .session
                    .as_ref()
                    .and_then(|s| s.ssh())
                    .is_some_and(|s| s.is_alive())
                    && let Some(key) = self.monitor_key(&id)
                {
                    keys.insert(key);
                }
            }
        }
        keys
    }

    /// The fleet folded into MACHINES (issue #156): one entry per
    /// monitored server with the rows that reach it, in the cards' own
    /// order, so the entry's position (and the probe stagger derived
    /// from it) is as stable as the grid.
    pub(crate) fn dash_groups(&self) -> Vec<(MonitorKey, Vec<Uuid>)> {
        crate::monitor::endpoint::group_by_machine(
            self.dash_hosts()
                .into_iter()
                .filter_map(|id| Some((self.monitor_key(&id)?, id)))
                .collect(),
        )
    }

    /// One-second heartbeat while the view is up: prune machines whose
    /// last row opted out, establish missing links, redial dead ones,
    /// try a failed machine's untried rows, and probe each live link on
    /// its staggered slot.
    fn dash_tick(&mut self) -> Task<Message> {
        self.monitor_dash.tick = self.monitor_dash.tick.wrapping_add(1);
        let interval = self.monitor_interval_secs();
        let groups = self.dash_groups();

        // Paused: no probes, no dials, and nothing held open. The
        // links a one-shot refresh opened are closed here, on the tick
        // after its sample lands, which is why the parking asks what is
        // still busy instead of closing blind.
        if self.monitor_dash.paused {
            let probing = self.monitor.probing.clone();
            let connecting = self
                .monitor_dash
                .links
                .values()
                .any(|l| matches!(l, DashLink::Connecting { .. }));
            if !connecting {
                self.monitor_dash
                    .park_idle_links(|key| probing.contains(key));
            }
            return Task::none();
        }

        // A host edited out of the fleet mid-session: close the dialed
        // connection of any machine nothing points at any more.
        let stale: Vec<MonitorKey> = self
            .monitor_dash
            .links
            .keys()
            .filter(|key| !groups.iter().any(|(k, _)| k == *key))
            .cloned()
            .collect();
        for key in stale {
            if let Some(DashLink::Live { transport, .. }) = self.monitor_dash.links.remove(&key) {
                transport.close_pooled();
            }
        }
        // The detail panel dies with its host's fleet membership.
        if let Some(sel) = self.monitor_dash.selected
            && !groups.iter().any(|(_, members)| members.contains(&sel))
        {
            self.monitor_dash.selected = None;
        }

        let mut tasks: Vec<Task<Message>> = Vec::new();
        for (i, (key, members)) in groups.into_iter().enumerate() {
            match self.monitor_dash.links.get(&key) {
                None => tasks.push(self.dash_link(&key, &members)),
                Some(DashLink::Live { transport, tried, .. }) if !transport.is_alive() => {
                    // The link died (tab closed, network drop): one
                    // automatic re-establish. If the redial fails the
                    // card goes Failed and stays there (no hammering a
                    // down host every second). The tried list rides
                    // along, so a tab that flaps cannot restart the
                    // machine's rotation on the row that already
                    // failed, once per flap.
                    let tried = tried.clone();
                    transport.close_pooled();
                    self.monitor_dash.links.remove(&key);
                    tasks.push(self.dash_link_with(&key, &members, tried));
                }
                Some(DashLink::Live { via, transport, .. })
                    if (self.monitor_dash.tick + i as u64).is_multiple_of(interval) =>
                {
                    let (via, transport) = (*via, transport.clone());
                    tasks.push(self.dash_probe(key, via, transport));
                }
                Some(DashLink::Failed { tried, .. }) => {
                    let tried = tried.clone();
                    // A live tab opened AFTER the failure is the way
                    // back for a machine whose stored credentials are
                    // stale: adopt its session now instead of leaving
                    // the card Failed until the 60s off-view sweep.
                    if let Some(task) = self.dash_adopt_live(&key, tried.clone()) {
                        tasks.push(task);
                        continue;
                    }
                    // One row's credentials failing is not the machine
                    // being down: try the next row that reaches it,
                    // once each, before the slot settles as failed.
                    if let Some(next) = members.iter().find(|m| !tried.contains(m)) {
                        tasks.push(self.dash_dial(&key, *next, tried));
                    }
                }
                _ => {}
            }
        }
        Task::batch(tasks)
    }

    /// Adopt a live terminal tab's session for this machine, if any of
    /// its rows has one. This outranks a headless dial everywhere it is
    /// offered: a host whose STORED credentials went stale is exactly
    /// the one an authenticated tab can still serve, and redialing them
    /// would only repeat the failed auth (fail2ban counts those). The
    /// `tried` list rides along so a link that later drops does not
    /// restart the machine's dial rotation from scratch.
    fn dash_adopt_live(
        &mut self,
        key: &MonitorKey,
        tried: Vec<Uuid>,
    ) -> Option<Task<Message>> {
        let (via, session) = self.live_session_for_machine(key)?;
        let transport = DashTransport::Tab(session);
        self.monitor_dash.links.insert(
            key.clone(),
            DashLink::Live {
                via,
                transport: transport.clone(),
                tried,
            },
        );
        Some(self.dash_probe(key.clone(), via, transport))
    }

    /// Establish a machine's link: borrow a live tab session when any
    /// of its rows has one (plus an immediate first sample), otherwise
    /// dial the first row.
    fn dash_link(&mut self, key: &MonitorKey, members: &[Uuid]) -> Task<Message> {
        self.dash_link_with(key, members, Vec::new())
    }

    /// `dash_link` carrying forward what has already been tried, so a
    /// re-establish after a dropped link continues the machine's dial
    /// rotation instead of restarting it on the row that just failed.
    fn dash_link_with(
        &mut self,
        key: &MonitorKey,
        members: &[Uuid],
        tried: Vec<Uuid>,
    ) -> Task<Message> {
        if let Some(task) = self.dash_adopt_live(key, tried.clone()) {
            return task;
        }
        match members.iter().find(|m| !tried.contains(m)).or(members.first()) {
            Some(via) => self.dash_dial(key, *via, tried),
            None => Task::none(),
        }
    }

    /// Headless dial for a machine, through one of its rows, mirroring
    /// the port-forward auto-start path: the stored credentials and
    /// pinned settings apply, nothing prompts. `tried` carries the rows
    /// already attempted for this machine, so a failure can move on to
    /// the next one instead of retrying the same credentials forever.
    fn dash_dial(&mut self, key: &MonitorKey, conn_id: Uuid, tried: Vec<Uuid>) -> Task<Message> {
        let Some(mut conn) = self
            .connections
            .iter()
            .find(|c| c.id == conn_id)
            .cloned()
        else {
            return Task::none();
        };
        // Same working copy every connect path dials: group inheritance
        // (D4) plus the effective proxy, so the probe authenticates
        // exactly like a tab to the same host would.
        self.apply_group_inheritance(&mut conn);
        let (password, private_key, certificate) = self.resolve_credentials(&conn);
        let pinned_agent = self.pinned_agent_public(&conn);
        let totp_secret = self
            .vault
            .as_ref()
            .and_then(|v| v.get_connection_totp_secret(&conn.id).ok().flatten());
        let resolver = self.make_jump_resolver(&mut conn);
        let host_key_check = self.make_host_key_check();
        let keepalive = self.effective_keepalive(&conn);

        let mut tried = tried;
        if !tried.contains(&conn_id) {
            tried.push(conn_id);
        }
        self.monitor_dash.links.insert(
            key.clone(),
            DashLink::Connecting {
                via: conn_id,
                tried,
            },
        );
        let key = key.clone();
        let stamp = self.monitor_dash.stamp;
        // Dashboard cards dial themselves as the grid fills, so this is
        // an unattended path: approved command proxies still run, the
        // rest are refused without a modal over the dashboard.
        let trusted_proxy_commands = self.trusted_proxy_commands();
        Task::perform(
            async move {
                let engine = oryxis_ssh::SshEngine::new()
                    .with_host_key_check(host_key_check)
                    .with_strict_host_key(true)
                    .with_proxy_command_ask(oryxis_ssh::trusted_only_proxy_command_ask(
                        trusted_proxy_commands,
                    ))
                    .with_totp_secret(totp_secret.as_deref())
                    .with_keepalive(keepalive)
                    .with_address_family(conn.address_family)
                    .with_rekey_limit_mb(conn.rekey_limit_mb)
                    .with_pinned_agent_key(pinned_agent.as_deref())
                    .with_algorithm_overrides(
                        conn.ciphers.clone(),
                        conn.kex.clone(),
                        conn.macs.clone(),
                        conn.host_key_algorithms.clone(),
                    );
                engine
                    .connect_monitor(
                        &conn,
                        password.as_deref(),
                        private_key.as_deref().map(|pem| {
                            oryxis_ssh::KeyMaterial::new(pem, certificate.as_deref())
                        }),
                        resolver.as_ref(),
                    )
                    .await
                    .map(std::sync::Arc::new)
                    .map_err(|e| e.to_string())
            },
            move |res| {
                Message::Monitor(MonitorMessage::DashDialed(key.clone(), conn_id, stamp, res))
            },
        )
    }

    /// Probe one machine's link. The in-flight guard, the stamp and the
    /// landing handler are the sidebar's own (`MonitorMessage::Sampled`),
    /// which is what keeps every surface on identical data. `via` is
    /// the row the link runs as, and only rides along for the error
    /// text and the alert label: the sample belongs to the machine.
    fn dash_probe(
        &mut self,
        key: MonitorKey,
        via: Uuid,
        transport: DashTransport,
    ) -> Task<Message> {
        if !self.monitor.probing.insert(key.clone()) {
            return Task::none();
        }
        let stamp = self.monitor_stamp;
        // Vitals only (owner call), EXCEPT the machine whose detail
        // panel is open: its panel shows the sidebar's full
        // presentation, ports section included, so that one pays for
        // the full probe. The unused slot stays in place either way
        // (the parser splits by position).
        let selected_here = self
            .monitor_dash
            .selected
            .and_then(|id| self.monitor_key(&id))
            .is_some_and(|k| k == key);
        let command = if selected_here {
            crate::monitor::probe::linux_probe_command()
        } else {
            crate::monitor::probe::linux_probe_command_vitals()
        };
        Task::perform(
            async move {
                let payload = match &transport {
                    DashTransport::Tab(s) => s.probe(&command, PROBE_TIMEOUT).await,
                    DashTransport::Pool(c) => c.probe(&command, PROBE_TIMEOUT).await,
                };
                match payload {
                    Some(payload) => Ok(payload),
                    None => Err(crate::i18n::t("monitor_probe_failed").to_string()),
                }
            },
            move |result| {
                Message::Monitor(MonitorMessage::Sampled(key.clone(), via, stamp, result))
            },
        )
    }

    /// Entering the Monitoring view: one round right away so the grid
    /// fills without waiting out the stagger. A pause survives leaving
    /// and re-entering the view, so this does nothing while it is on;
    /// the board keeps the last sample and the Refresh action is the
    /// way to take another.
    pub(crate) fn dash_enter(&mut self) -> Task<Message> {
        if self.monitor_dash.paused {
            return Task::none();
        }
        self.dash_round(true)
    }

    /// One round over the whole board: link what has no link, probe
    /// what has one, and give the machines that failed another try
    /// (entering the view and the Refresh action are the deliberate
    /// acts the sticky failure waits for, same as the card's retry).
    ///
    /// `fresh` decides what "another try" means for a failed machine.
    /// Entering the view starts a new round from its first row, since
    /// whatever was wrong last time may well be fixed by now. Refresh
    /// CONTINUES the round instead, moving to a row that has not been
    /// tried: clicking it twice must not dial the same broken login
    /// twice, which is exactly what a fresh round would do.
    fn dash_round(&mut self, fresh: bool) -> Task<Message> {
        let groups = self.dash_groups();
        let mut tasks: Vec<Task<Message>> = Vec::new();
        for (key, members) in groups {
            match self.monitor_dash.links.get(&key) {
                None => tasks.push(self.dash_link(&key, &members)),
                // A quick round-trip elsewhere kept the links warm
                // (that is the idle TTL's point); refresh them now.
                Some(DashLink::Live { via, transport, .. }) if transport.is_alive() => {
                    let (via, transport) = (*via, transport.clone());
                    tasks.push(self.dash_probe(key, via, transport));
                }
                Some(DashLink::Failed { tried, .. }) => {
                    // Once every row has been tried the list is spent,
                    // so the next ask starts over rather than doing
                    // nothing.
                    let mut tried = if fresh { Vec::new() } else { tried.clone() };
                    if members.iter().all(|m| tried.contains(m)) {
                        tried.clear();
                    }
                    let next = members.iter().find(|m| !tried.contains(m)).copied();
                    self.monitor_dash.links.remove(&key);
                    // Same preference as every other establish path, and
                    // it has to be HERE rather than only inside
                    // `dash_link`: this arm always finds an untried row
                    // and dials it, so entering the view or clicking
                    // Refresh would re-dial stale credentials while a
                    // working tab sat open (and, while paused, would do
                    // it on every click, since the tick that adopts
                    // never runs).
                    if let Some(task) = self.dash_adopt_live(&key, tried.clone()) {
                        tasks.push(task);
                        continue;
                    }
                    match next {
                        Some(next) => tasks.push(self.dash_dial(&key, next, tried)),
                        None => tasks.push(self.dash_link(&key, &members)),
                    }
                }
                _ => {}
            }
        }
        Task::batch(tasks)
    }

    /// Leaving the Monitoring view: arm the idle TTL that closes the
    /// dialed connections unless the user comes back first.
    pub(crate) fn dash_leave(&mut self) -> Task<Message> {
        if self.monitor_dash.links.is_empty() {
            return Task::none();
        }
        let stamp = self.monitor_dash.stamp;
        Task::perform(
            async move {
                tokio::time::sleep(IDLE_TTL).await;
            },
            move |_| Message::Monitor(MonitorMessage::DashSweepDue(stamp)),
        )
    }

    /// A live SSH session already connected to this MACHINE, from any
    /// tab and pane (not just the focused one, unlike the sidebar's
    /// `monitor_target`), with the row it belongs to.
    ///
    /// Any row that reaches the machine will do: the probe reads the
    /// same `/proc` whoever it logged in as, and riding an open session
    /// is what keeps the dashboard from dialing at all (issue #156:
    /// three rows on one server used to mean three logins).
    fn live_session_for_machine(
        &self,
        key: &MonitorKey,
    ) -> Option<(Uuid, std::sync::Arc<oryxis_ssh::SshSession>)> {
        for tab in &self.tabs {
            for pane in tab.pane_grid.panes.values() {
                let crate::state::PaneOrigin::Host(id) = pane.origin else {
                    continue;
                };
                if self.monitor_key(&id).is_some_and(|k| k == *key)
                    && let Some(ssh) = pane.session.as_ref().and_then(|s| s.ssh())
                    && ssh.is_alive()
                {
                    return Some((id, ssh.clone()));
                }
            }
        }
        None
    }

    /// Index of a tab whose active pane belongs to this host, for the
    /// card's open-terminal action.
    fn tab_index_for_host(&self, conn_id: Uuid) -> Option<usize> {
        self.tabs.iter().position(|t| {
            t.pane_grid.panes.values().any(|p| {
                matches!(p.origin, crate::state::PaneOrigin::Host(id) if id == conn_id)
                    && p.session.as_ref().and_then(|s| s.ssh()).is_some_and(|s| s.is_alive())
            })
        })
    }
}
