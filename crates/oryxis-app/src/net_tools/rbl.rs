//! DNSBL (blocklist) lookups: is this address listed as a spam source.
//!
//! A blocklist query is an ordinary DNS query with the address reversed
//! into the zone's name, so this needs no protocol of its own: an answer
//! means listed, NXDOMAIN means clean, and the TXT record next to it
//! carries the reason.
//!
//! Verdicts are best-effort by construction. Every zone here is a public
//! mirror with its own usage policy, and several of them rate-limit or
//! refuse queries from open resolvers (Spamhaus in particular answers
//! `127.255.255.x` instead of a listing when it declines). That is why a
//! zone that does not answer is reported as "no answer" rather than
//! folded into "clean".

use std::net::IpAddr;

use hickory_resolver::proto::rr::RecordType;

use super::{CardStatus, NetToolCard};
use crate::i18n::t;

/// One blocklist.
struct Zone {
    name: &'static str,
    /// Whether the zone publishes IPv6 listings. The ones that do not
    /// are skipped for a v6 target rather than queried: a guaranteed
    /// NXDOMAIN would read as a clean verdict the zone never gave.
    ipv6: bool,
}

/// The zones queried, all public and all widely used by mail operators.
/// Deliberately a fixed list rather than a setting: a user-supplied zone
/// is a DNS query built from typed text, and the value of the panel is
/// that these are the ones whose answers mean something.
const ZONES: [Zone; 8] = [
    Zone { name: "zen.spamhaus.org", ipv6: true },
    Zone { name: "bl.spamcop.net", ipv6: false },
    Zone { name: "b.barracudacentral.org", ipv6: false },
    Zone { name: "dnsbl.sorbs.net", ipv6: false },
    Zone { name: "psbl.surriel.com", ipv6: false },
    Zone { name: "bl.blocklist.de", ipv6: true },
    Zone { name: "dnsbl-1.uceprotect.net", ipv6: false },
    Zone { name: "all.s5h.net", ipv6: false },
];

/// What one zone said.
enum Verdict {
    Listed { codes: Vec<String>, reasons: Vec<String> },
    Clean,
    NoAnswer(String),
    Skipped,
}

pub(crate) async fn probe(target: &str) -> Result<Vec<NetToolCard>, String> {
    let host = super::host_of(target);
    // A name is accepted and resolved first: people paste the mail
    // server's host name as often as its address, and refusing that
    // would be pedantry rather than a safeguard.
    let ip = super::port::resolve_one(host).await?;
    let resolver = super::dns::resolver()?;

    let mut cards = vec![NetToolCard::new(
        t("net_target").to_string(),
        vec![format!("{host} -> {ip}")],
    )];

    let mut listed: Vec<String> = Vec::new();
    let mut clean: Vec<String> = Vec::new();
    let mut quiet: Vec<String> = Vec::new();

    for zone in &ZONES {
        match query_zone(&resolver, ip, zone).await {
            Verdict::Listed { codes, reasons } => {
                listed.push(format!("{}   {}", zone.name, codes.join(", ")));
                listed.extend(reasons.into_iter().map(|r| format!("    {r}")));
            }
            Verdict::Clean => clean.push(zone.name.to_string()),
            Verdict::NoAnswer(why) => quiet.push(format!("{}   {why}", zone.name)),
            Verdict::Skipped => {
                quiet.push(format!("{}   {}", zone.name, t("net_rbl_no_ipv6")))
            }
        }
    }

    if listed.is_empty() {
        cards.push(
            NetToolCard::new(
                t("net_rbl_clean").to_string(),
                vec![format!("{} / {}", clean.len(), ZONES.len())],
            )
            .status(CardStatus::Ok),
        );
    } else {
        cards.push(NetToolCard::new(t("net_rbl_listed").to_string(), listed).status(CardStatus::Bad));
    }
    if !clean.is_empty() {
        cards.push(NetToolCard::new(t("net_rbl_clean_zones").to_string(), clean));
    }
    if !quiet.is_empty() {
        cards.push(
            NetToolCard::new(t("net_rbl_no_answer").to_string(), quiet).status(CardStatus::Warn),
        );
    }
    Ok(cards)
}

async fn query_zone(
    resolver: &hickory_resolver::TokioResolver,
    ip: IpAddr,
    zone: &Zone,
) -> Verdict {
    let Some(name) = query_name(ip, zone.name, zone.ipv6) else {
        return Verdict::Skipped;
    };
    match resolver.lookup(name.clone(), RecordType::A).await {
        Ok(lookup) => {
            let codes: Vec<String> = lookup
                .answers()
                .iter()
                .filter(|r| r.record_type() == RecordType::A)
                .map(|r| r.data.to_string())
                .collect();
            if codes.is_empty() {
                return Verdict::Clean;
            }
            // The TXT beside the A record is where the zone explains
            // itself ("listed in CSS", "see https://..."), which is the
            // half a user can act on.
            let reasons = match resolver.lookup(name, RecordType::TXT).await {
                Ok(txt) => txt
                    .answers()
                    .iter()
                    .filter(|r| r.record_type() == RecordType::TXT)
                    .map(|r| super::dns::unquote_txt(&r.data.to_string()))
                    .collect(),
                Err(_) => Vec::new(),
            };
            Verdict::Listed { codes, reasons }
        }
        Err(e) if e.is_nx_domain() || e.is_no_records_found() => Verdict::Clean,
        Err(e) => Verdict::NoAnswer(e.to_string()),
    }
}

/// The name a blocklist is asked about: the address reversed, then the
/// zone. `None` when the target is IPv6 and the zone does not publish v6
/// data, so the caller reports a skip instead of a verdict.
pub(crate) fn query_name(ip: IpAddr, zone: &str, zone_has_ipv6: bool) -> Option<String> {
    if ip.is_ipv6() && !zone_has_ipv6 {
        return None;
    }
    Some(format!("{}.{zone}", super::dns::reversed_labels(ip)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ipv4_query_name_reverses_the_octets() {
        assert_eq!(
            query_name("1.2.3.4".parse().unwrap(), "zen.spamhaus.org", true).unwrap(),
            "4.3.2.1.zen.spamhaus.org"
        );
        // The test address every DNSBL lists on purpose.
        assert_eq!(
            query_name("127.0.0.2".parse().unwrap(), "bl.spamcop.net", false).unwrap(),
            "2.0.0.127.bl.spamcop.net"
        );
    }

    #[test]
    fn ipv6_query_name_is_nibbles() {
        let name = query_name("2001:db8::1".parse().unwrap(), "zen.spamhaus.org", true).unwrap();
        assert!(name.starts_with("1.0.0.0."));
        assert!(name.ends_with(".zen.spamhaus.org"));
        let labels = name.trim_end_matches(".zen.spamhaus.org");
        assert_eq!(labels.split('.').count(), 32);
    }

    #[test]
    fn a_v4_only_zone_skips_a_v6_target() {
        assert!(query_name("2001:db8::1".parse().unwrap(), "bl.spamcop.net", false).is_none());
        // ... and still answers for a v4 target.
        assert!(query_name("1.2.3.4".parse().unwrap(), "bl.spamcop.net", false).is_some());
    }

    #[test]
    fn zone_list_has_no_duplicates() {
        let mut names: Vec<&str> = ZONES.iter().map(|z| z.name).collect();
        let count = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), count);
    }
}
