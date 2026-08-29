//! Ping and traceroute, driven through the system binaries.
//!
//! Both could be spoken natively, and neither is, deliberately. An ICMP
//! echo needs either a raw socket (root on every platform) or the
//! datagram-ICMP path, which Linux gates behind `ping_group_range`,
//! macOS allows, Windows does not have at all (its answer is the
//! `IcmpSendEcho` API), and which no sandboxed build - AppImage,
//! Flatpak, MSIX - can count on. That is three implementations and four
//! permission regimes to reproduce output the user can already get, so
//! the panel runs the same command they would and reads what comes back.
//!
//! The consequence is that the RAW OUTPUT is always shown. Parsing only
//! adds a summary card on top: `ping` and `traceroute` phrase themselves
//! differently across iputils, BSD and Windows (which also translates
//! its output), and a summary that guessed wrong must never be the only
//! thing on screen.

use std::time::Duration;

use super::{CardStatus, NetToolCard};
use crate::i18n::t;

/// Echo requests per run: four is what every platform's default flag
/// count means to a person reading the output.
const COUNT: u8 = 4;
/// Wall-clock ceiling per run. Ping's own budget is bounded by `-c`;
/// traceroute's is not, so the longer one covers 20 hops timing out.
const PING_BUDGET: Duration = Duration::from_secs(25);
const TRACEROUTE_BUDGET: Duration = Duration::from_secs(90);
/// Hops a traceroute walks before giving up. The default is 30 or 64
/// depending on the implementation, which is a long wait for a path that
/// is not going to complete.
const MAX_HOPS: u8 = 20;

pub(crate) async fn probe_ping(target: &str) -> Result<Vec<NetToolCard>, String> {
    let host = super::host_of(target);
    let (program, args) = if cfg!(windows) {
        ("ping", vec!["-n".to_string(), COUNT.to_string(), host.to_string()])
    } else {
        ("ping", vec!["-c".to_string(), COUNT.to_string(), host.to_string()])
    };
    let output = match run_tool(program, &args, PING_BUDGET).await {
        Ok(o) => o,
        Err(card) => return Ok(vec![card]),
    };
    let mut cards = Vec::new();
    if let Some(summary) = parse_ping(&output) {
        let mut lines = vec![
            t("net_ping_summary")
                .replacen("{recv}", &summary.received.to_string(), 1)
                .replacen("{sent}", &summary.transmitted.to_string(), 1)
                .replacen("{loss}", &format!("{:.0}", summary.loss_pct), 1),
        ];
        if let Some((min, avg, max)) = summary.rtt_ms {
            // Sub-millisecond round trips are ordinary on loopback and on
            // a fast LAN, and one decimal renders all three of them as
            // "0.0 ms", which reads as a broken measurement rather than a
            // fast one. The scale follows the numbers.
            let line = if max < 10.0 {
                format!("{}: {min:.3} / {avg:.3} / {max:.3} ms", t("net_ping_rtt"))
            } else {
                format!("{}: {min:.1} / {avg:.1} / {max:.1} ms", t("net_ping_rtt"))
            };
            lines.push(line);
        }
        let status = match summary.received {
            0 => CardStatus::Bad,
            r if r < summary.transmitted => CardStatus::Warn,
            _ => CardStatus::Ok,
        };
        cards.push(NetToolCard::new(host.to_string(), lines).status(status).raw(output.clone()));
    }
    cards.push(raw_card(&output));
    Ok(cards)
}

pub(crate) async fn probe_traceroute(target: &str) -> Result<Vec<NetToolCard>, String> {
    let host = super::host_of(target);
    let (program, args) = if cfg!(windows) {
        (
            "tracert",
            vec![
                "-d".to_string(),
                "-h".to_string(),
                MAX_HOPS.to_string(),
                "-w".to_string(),
                "2000".to_string(),
                host.to_string(),
            ],
        )
    } else {
        (
            "traceroute",
            vec![
                "-n".to_string(),
                "-q".to_string(),
                "1".to_string(),
                "-w".to_string(),
                "2".to_string(),
                "-m".to_string(),
                MAX_HOPS.to_string(),
                host.to_string(),
            ],
        )
    };
    let output = match run_tool(program, &args, TRACEROUTE_BUDGET).await {
        Ok(o) => o,
        Err(card) => return Ok(vec![card]),
    };
    let mut cards = Vec::new();
    let hops = parse_traceroute(&output);
    if !hops.is_empty() {
        let unanswered = hops.iter().filter(|h| h.hosts.is_empty()).count();
        let lines: Vec<String> = hops.iter().map(Hop::render).collect();
        // Every hop silent means the path is invisible from here, which
        // is a finding; a few silent hops in the middle is ordinary
        // (plenty of routers simply do not answer).
        let status = if unanswered == hops.len() {
            CardStatus::Bad
        } else if unanswered > 0 {
            CardStatus::Warn
        } else {
            CardStatus::Ok
        };
        cards.push(
            NetToolCard::new(
                format!(
                    "{host}   {}",
                    t("net_trace_hops").replacen("{n}", &hops.len().to_string(), 1)
                ),
                lines,
            )
            .status(status)
            .raw(output.clone()),
        );
    }
    cards.push(raw_card(&output));
    Ok(cards)
}

/// The tool's own output, verbatim. Always present, and always the thing
/// the copy action yields for these two tools.
fn raw_card(output: &str) -> NetToolCard {
    NetToolCard::new(
        t("net_raw_output").to_string(),
        output.lines().map(str::to_string).collect(),
    )
    .raw(output.to_string())
}

/// Run a system tool and return its combined output. The `Err` arm is a
/// finished card rather than a message, because "traceroute is not
/// installed" is an answer worth rendering like any other.
async fn run_tool(program: &str, args: &[String], budget: Duration) -> Result<String, NetToolCard> {
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args);
    #[cfg(windows)]
    {
        // No console window for the spawned tool: this is a GUI app, and
        // a flashing black box next to the panel is not the output.
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let child = cmd
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn();
    let child = match child {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(NetToolCard::new(
                t("net_tool_missing_binary").to_string(),
                vec![
                    format!("{program}: {}", t("net_tool_missing_binary_desc")),
                    missing_hint(program).to_string(),
                ],
            )
            .status(CardStatus::Bad));
        }
        Err(e) => {
            return Err(NetToolCard::new(program.to_string(), vec![e.to_string()])
                .status(CardStatus::Bad));
        }
    };
    let out = match tokio::time::timeout(budget, child.wait_with_output()).await {
        Ok(Ok(out)) => out,
        Ok(Err(e)) => {
            return Err(NetToolCard::new(program.to_string(), vec![e.to_string()])
                .status(CardStatus::Bad));
        }
        // The child is dropped here, which kills it: tokio's Command
        // defaults to kill_on_drop(false), so the explicit drop of the
        // future is what has to end the process. `wait_with_output`
        // consumed the child, so the timeout arm owns nothing to kill,
        // and the process ends when its pipes close with the future.
        Err(_) => {
            return Err(NetToolCard::new(
                program.to_string(),
                vec![format!("{} ({}s)", t("net_err_timeout"), budget.as_secs())],
            )
            .status(CardStatus::Warn));
        }
    };
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    let err = String::from_utf8_lossy(&out.stderr);
    if !err.trim().is_empty() {
        // Unreachable-host messages arrive on stderr in several
        // implementations, so dropping it would blank the card in
        // exactly the failing case the user is looking at.
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(err.trim_end());
    }
    Ok(text)
}

/// Where to get the missing binary. Named per tool because the packages
/// differ (and on Windows both ship with the OS, so the message there is
/// about PATH rather than an install).
fn missing_hint(program: &str) -> &'static str {
    if cfg!(windows) {
        return t("net_tool_missing_windows");
    }
    match program {
        "traceroute" => t("net_tool_missing_traceroute"),
        _ => t("net_tool_missing_ping"),
    }
}

/// What a ping run reported.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PingSummary {
    pub transmitted: u32,
    pub received: u32,
    pub loss_pct: f32,
    /// min / avg / max, when the tool printed a statistics line.
    pub rtt_ms: Option<(f32, f32, f32)>,
}

/// Read the statistics block. Handles iputils ("4 received"), BSD and
/// macOS ("4 packets received") and Windows (whose counters are
/// localized, so only the percentage and the numeric fields are read).
/// Returns `None` when nothing recognizable is there, which is what
/// keeps the raw card from being contradicted by a made-up summary.
pub(crate) fn parse_ping(output: &str) -> Option<PingSummary> {
    let mut summary: Option<PingSummary> = None;
    for line in output.lines() {
        let l = line.trim();
        if let Some(s) = parse_posix_stats(l) {
            summary = Some(s);
        } else if summary.is_none()
            && let Some(s) = parse_windows_stats(l)
        {
            summary = Some(s);
        }
        if let Some(rtt) = parse_rtt_line(l)
            && let Some(s) = summary.as_mut()
        {
            s.rtt_ms = Some(rtt);
        }
    }
    // Windows prints its timing block after the counters, on its own
    // localized line, so it is read in a second pass over the same text.
    if let Some(s) = summary.as_mut()
        && s.rtt_ms.is_none()
        && let Some(rtt) = parse_windows_rtt(output)
    {
        s.rtt_ms = Some(rtt);
    }
    summary
}

/// `4 packets transmitted, 4 received, 0% packet loss, time 3005ms` and
/// its BSD spelling `4 packets transmitted, 4 packets received, 0.0% packet loss`.
fn parse_posix_stats(line: &str) -> Option<PingSummary> {
    if !line.contains("transmitted") {
        return None;
    }
    let transmitted = number_before(line, "packets transmitted")
        .or_else(|| number_before(line, "transmitted"))?;
    let received = number_before(line, "packets received")
        .or_else(|| number_before(line, "received"))?;
    let loss_pct = percent_before(line, "packet loss").unwrap_or_else(|| {
        if transmitted == 0.0 {
            0.0
        } else {
            (transmitted - received) / transmitted * 100.0
        }
    });
    Some(PingSummary {
        transmitted: transmitted as u32,
        received: received as u32,
        loss_pct,
        rtt_ms: None,
    })
}

/// Windows: `Packets: Sent = 4, Received = 4, Lost = 0 (0% loss),`. The
/// words are translated on a localized install, the `= n` shape and the
/// `(n% ...)` are not, so the three numbers are read positionally from
/// the `=` assignments and the percentage from the parentheses.
fn parse_windows_stats(line: &str) -> Option<PingSummary> {
    if !line.contains('=') || !line.contains('%') {
        return None;
    }
    let numbers: Vec<f32> = line
        .split('=')
        .skip(1)
        .filter_map(|piece| {
            let digits: String =
                piece.trim_start().chars().take_while(char::is_ascii_digit).collect();
            digits.parse::<f32>().ok()
        })
        .collect();
    if numbers.len() < 3 {
        return None;
    }
    let loss_pct = line
        .split('(')
        .nth(1)
        .and_then(|rest| rest.split('%').next())
        .and_then(|n| n.trim().parse::<f32>().ok())?;
    Some(PingSummary {
        transmitted: numbers[0] as u32,
        received: numbers[1] as u32,
        loss_pct,
        rtt_ms: None,
    })
}

/// `rtt min/avg/max/mdev = 11.155/11.402/11.717/0.205 ms` (iputils) and
/// `round-trip min/avg/max/stddev = ...` (BSD).
fn parse_rtt_line(line: &str) -> Option<(f32, f32, f32)> {
    if !line.contains("min/avg/max") {
        return None;
    }
    let values = line.rsplit_once('=')?.1;
    let mut parts = values.trim().trim_end_matches("ms").trim().split('/');
    let min = parts.next()?.trim().parse().ok()?;
    let avg = parts.next()?.trim().parse().ok()?;
    let max = parts.next()?.trim().parse().ok()?;
    Some((min, avg, max))
}

/// Windows: `Minimum = 11ms, Maximum = 12ms, Average = 11ms`, in
/// whatever language the install speaks. Read positionally like the
/// counters, and only from a line whose numbers all carry `ms`, so the
/// counters line above cannot be mistaken for it.
fn parse_windows_rtt(output: &str) -> Option<(f32, f32, f32)> {
    for line in output.lines() {
        let l = line.trim();
        if l.matches("ms").count() < 3 || !l.contains('=') {
            continue;
        }
        let values: Vec<f32> = l
            .split('=')
            .skip(1)
            .filter_map(|piece| {
                let digits: String =
                    piece.trim_start().chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
                digits.parse::<f32>().ok()
            })
            .collect();
        if values.len() >= 3 {
            // Windows prints minimum, maximum, average, in that order.
            return Some((values[0], values[2], values[1]));
        }
    }
    None
}

/// The number immediately before `marker` on the line.
fn number_before(line: &str, marker: &str) -> Option<f32> {
    let head = line.split(marker).next()?;
    head.split_whitespace().next_back()?.parse().ok()
}

/// The percentage immediately before `marker`, tolerating both `0%` and
/// `0.0%`.
fn percent_before(line: &str, marker: &str) -> Option<f32> {
    let head = line.split(marker).next()?;
    head.split_whitespace()
        .next_back()?
        .trim_end_matches('%')
        .parse()
        .ok()
}

/// One traceroute hop.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Hop {
    pub index: u32,
    /// Addresses that answered. More than one when the probes at this
    /// distance took different paths, which load-balanced networks do.
    pub hosts: Vec<String>,
    pub times_ms: Vec<f32>,
}

impl Hop {
    fn render(&self) -> String {
        if self.hosts.is_empty() {
            return format!("{:>2}   *", self.index);
        }
        let times = if self.times_ms.is_empty() {
            String::new()
        } else {
            format!(
                "   {}",
                self.times_ms
                    .iter()
                    .map(|t| format!("{t:.1} ms"))
                    .collect::<Vec<_>>()
                    .join("  ")
            )
        };
        format!("{:>2}   {}{}", self.index, self.hosts.join(", "), times)
    }
}

/// Read hop lines from either shape:
///
/// - POSIX: `` 1  192.168.0.1  0.512 ms  0.480 ms `` (a silent hop is `*`)
/// - Windows: `  1     1 ms    <1 ms     1 ms  192.168.0.1`
///
/// Both begin with the hop number, which is what anchors the parse; a
/// line that does not is header or trailer and is skipped.
pub(crate) fn parse_traceroute(output: &str) -> Vec<Hop> {
    let mut hops = Vec::new();
    for line in output.lines() {
        let trimmed = line.trim();
        let mut tokens = trimmed.split_whitespace();
        let Some(index) = tokens.next().and_then(|t| t.parse::<u32>().ok()) else {
            continue;
        };
        let mut hosts: Vec<String> = Vec::new();
        let mut times_ms: Vec<f32> = Vec::new();
        let rest: Vec<&str> = tokens.collect();
        let mut i = 0;
        while i < rest.len() {
            let token = rest[i];
            if token == "*" {
                i += 1;
                continue;
            }
            // `1 ms` and `0.512 ms` (value and unit split), `<1` (a
            // Windows sub-millisecond reply), and `1ms` all mean a time.
            if let Some(v) = parse_time_token(token, rest.get(i + 1).copied()) {
                times_ms.push(v.0);
                i += v.1;
                continue;
            }
            // Anything left that looks like an address is one, possibly
            // parenthesized in `name (1.2.3.4)` form when the tool
            // resolved names. Prose is skipped rather than collected:
            // Windows writes `Request timed out.` on a silent hop, and
            // three English words in the address column would read as
            // three routers.
            let host = token.trim_matches(|c| c == '(' || c == ')');
            if looks_like_address(host) && !hosts.iter().any(|h| h == host) {
                hosts.push(host.to_string());
            }
            i += 1;
        }
        hops.push(Hop { index, hosts, times_ms });
    }
    hops
}

/// Whether a token is a router address rather than part of a sentence.
/// An IP literal always is; a name has to carry a dot and end in a label
/// that could be a TLD, which is what keeps `out.` (the tail of
/// `Request timed out.`) from being read as a host.
fn looks_like_address(token: &str) -> bool {
    let token = token.trim_end_matches('.');
    if token.is_empty() {
        return false;
    }
    if token.parse::<std::net::IpAddr>().is_ok() {
        return true;
    }
    if !token
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' || c == ':')
    {
        return false;
    }
    let mut labels = token.split('.');
    let Some(tld) = labels.next_back() else {
        return false;
    };
    // A single label is a bare word, not a host: traceroute never prints
    // an unqualified name in the address column.
    labels.next().is_some() && tld.len() >= 2 && tld.chars().all(|c| c.is_ascii_alphanumeric())
}

/// A time token and how many tokens it consumed. `<1` is Windows for
/// "under a millisecond" and is read as zero rather than dropped, so the
/// hop keeps its answered/silent distinction.
fn parse_time_token(token: &str, next: Option<&str>) -> Option<(f32, usize)> {
    // `<1` is an upper bound, not a measurement: the hop answered in
    // less than the clock can resolve. It reads as 0, because rendering
    // it as 1 would claim a millisecond the router never spent.
    let (value, below_resolution) = match token.strip_prefix('<') {
        Some(rest) => (rest, true),
        None => (token, false),
    };
    let floor = |v: f32| if below_resolution { 0.0 } else { v };
    if let Some(stripped) = value.strip_suffix("ms")
        && let Ok(v) = stripped.parse::<f32>()
    {
        return Some((floor(v), 1));
    }
    let parsed: f32 = floor(value.parse().ok()?);
    // A bare number is only a time when `ms` follows it; otherwise it is
    // part of an address (or an AS number) and must not be eaten.
    if next == Some("ms") {
        return Some((parsed, 2));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const LINUX_PING: &str = "PING example.com (93.184.216.34) 56(84) bytes of data.\n\
64 bytes from 93.184.216.34: icmp_seq=1 ttl=54 time=11.2 ms\n\
\n--- example.com ping statistics ---\n\
4 packets transmitted, 4 received, 0% packet loss, time 3005ms\n\
rtt min/avg/max/mdev = 11.155/11.402/11.717/0.205 ms\n";

    const MACOS_PING: &str = "--- example.com ping statistics ---\n\
4 packets transmitted, 3 packets received, 25.0% packet loss\n\
round-trip min/avg/max/stddev = 11.155/11.402/11.717/0.205 ms\n";

    const WINDOWS_PING: &str = "Pinging example.com [93.184.216.34] with 32 bytes of data:\r\n\
Reply from 93.184.216.34: bytes=32 time=11ms TTL=54\r\n\
\r\nPing statistics for 93.184.216.34:\r\n\
    Packets: Sent = 4, Received = 4, Lost = 0 (0% loss),\r\n\
Approximate round trip times in milli-seconds:\r\n\
    Minimum = 11ms, Maximum = 13ms, Average = 12ms\r\n";

    const UNREACHABLE_PING: &str = "--- 10.0.0.1 ping statistics ---\n\
4 packets transmitted, 0 received, 100% packet loss, time 3068ms\n";

    #[test]
    fn linux_ping_stats() {
        let s = parse_ping(LINUX_PING).expect("summary");
        assert_eq!((s.transmitted, s.received), (4, 4));
        assert_eq!(s.loss_pct, 0.0);
        let (min, avg, max) = s.rtt_ms.expect("rtt");
        assert!((min - 11.155).abs() < 0.001);
        assert!((avg - 11.402).abs() < 0.001);
        assert!((max - 11.717).abs() < 0.001);
    }

    #[test]
    fn macos_ping_stats() {
        let s = parse_ping(MACOS_PING).expect("summary");
        assert_eq!((s.transmitted, s.received), (4, 3));
        assert_eq!(s.loss_pct, 25.0);
        assert!(s.rtt_ms.is_some());
    }

    #[test]
    fn windows_ping_stats() {
        let s = parse_ping(WINDOWS_PING).expect("summary");
        assert_eq!((s.transmitted, s.received), (4, 4));
        assert_eq!(s.loss_pct, 0.0);
        // Windows prints min, max, average; the summary reports
        // min / avg / max like every other platform.
        let (min, avg, max) = s.rtt_ms.expect("rtt");
        assert_eq!((min, avg, max), (11.0, 12.0, 13.0));
    }

    #[test]
    fn total_loss_is_reported_not_dropped() {
        let s = parse_ping(UNREACHABLE_PING).expect("summary");
        assert_eq!(s.received, 0);
        assert_eq!(s.loss_pct, 100.0);
        assert!(s.rtt_ms.is_none());
    }

    #[test]
    fn unparseable_output_yields_no_summary() {
        assert!(parse_ping("ping: unknown host nope.invalid").is_none());
        assert!(parse_ping("").is_none());
    }

    #[test]
    fn posix_traceroute_hops() {
        let out = "traceroute to example.com (93.184.216.34), 20 hops max\n\
 1  192.168.0.1  0.512 ms\n\
 2  * \n\
 3  93.184.216.34  11.402 ms\n";
        let hops = parse_traceroute(out);
        assert_eq!(hops.len(), 3);
        assert_eq!(hops[0].hosts, vec!["192.168.0.1"]);
        assert!((hops[0].times_ms[0] - 0.512).abs() < 0.001);
        assert!(hops[1].hosts.is_empty(), "a silent hop keeps its slot");
        assert_eq!(hops[2].hosts, vec!["93.184.216.34"]);
    }

    #[test]
    fn windows_traceroute_hops() {
        let out = "Tracing route to example.com [93.184.216.34]\r\n\
over a maximum of 20 hops:\r\n\r\n\
  1     1 ms    <1 ms     1 ms  192.168.0.1\r\n\
  2     *        *        *     Request timed out.\r\n\
  3    11 ms    12 ms    11 ms  93.184.216.34\r\n";
        let hops = parse_traceroute(out);
        assert_eq!(hops.len(), 3);
        assert_eq!(hops[0].hosts, vec!["192.168.0.1"]);
        assert_eq!(hops[0].times_ms, vec![1.0, 0.0, 1.0]);
        // "Request timed out." is prose, not three routers.
        assert!(hops[1].hosts.is_empty());
        assert_eq!(hops[2].times_ms.len(), 3);
        assert_eq!(hops[2].hosts, vec!["93.184.216.34"]);
    }

    #[test]
    fn a_hop_that_resolved_a_name_keeps_both() {
        let out = " 1  gw.example.com (192.168.0.1)  0.512 ms\n";
        let hops = parse_traceroute(out);
        assert_eq!(hops[0].hosts, vec!["gw.example.com", "192.168.0.1"]);
    }

    #[test]
    fn header_lines_are_not_hops() {
        let out = "traceroute to example.com (93.184.216.34), 20 hops max, 60 byte packets\n";
        assert!(parse_traceroute(out).is_empty());
    }
}
