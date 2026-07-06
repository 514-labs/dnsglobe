//! Anycast site discovery: ask a resolver *which* of its sites answered.
//!
//! Large anycast networks expose an identification query — Quad9 answers
//! `TXT id.server.on.quad9.net`, Cloudflare answers `CH TXT id.server`,
//! Google reports its egress subnet, OpenDNS has `TXT debug.opendns.com`.
//! The answer names the POP (usually by IATA airport code), telling you
//! where your queries actually land instead of the operator's home region.
//! See <https://github.com/514-labs/dnsglobe/issues/6>.

use std::net::IpAddr;

use crate::dns;

/// Which identification query a resolver understands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SiteProbe {
    /// `IN TXT id.server.on.quad9.net` → `res120.qyul1.on.quad9.net`;
    /// the second label carries the POP as `q<iata><n>`.
    Quad9,
    /// `CH TXT id.server` → `yul01` (IATA code plus a node number).
    Cloudflare,
    /// `IN TXT o-o.myaddr.l.google.com` returns the answering cluster's
    /// egress IP; `IN TXT locations.publicdns.goog` maps egress prefixes to
    /// IATA codes.
    Google,
    /// `IN TXT debug.opendns.com` → a `server r3.yyz` line; the last label
    /// is the IATA code.
    OpenDns,
    /// Generic `CH TXT id.server` with a free-form site string (CleanBrowsing,
    /// UltraDNS, …). Best-effort short token, usually not an airport code.
    ChIdServer,
}

/// A discovered anycast site.
#[derive(Debug, Clone)]
pub struct Site {
    /// Short display code, e.g. `YUL`. Uppercase, at most 7 chars.
    pub code: String,
    /// Map position when the code is a known IATA airport.
    pub coords: Option<(f64, f64)>,
}

impl Site {
    fn from_code(code: &str) -> Self {
        let code: String = code.chars().take(7).collect::<String>().to_uppercase();
        let coords = airport_coords(&code);
        Site { code, coords }
    }
}

/// Run one resolver's identification query. None means the probe failed or
/// the answer was unparseable — the resolver keeps its configured location.
pub async fn discover(probe: SiteProbe, server: IpAddr) -> Option<Site> {
    match probe {
        SiteProbe::Quad9 => {
            let strings = dns::txt_strings(server, "id.server.on.quad9.net").await?;
            parse_quad9(&strings)
        }
        SiteProbe::Cloudflare => {
            let strings = dns::chaos_txt(server, "id.server").await?;
            parse_cloudflare(&strings)
        }
        SiteProbe::Google => {
            let myaddr = dns::txt_strings(server, "o-o.myaddr.l.google.com").await?;
            let locations = dns::txt_strings(server, "locations.publicdns.goog").await?;
            parse_google(&myaddr, &locations)
        }
        SiteProbe::OpenDns => {
            let strings = dns::txt_strings(server, "debug.opendns.com").await?;
            parse_opendns(&strings)
        }
        SiteProbe::ChIdServer => {
            let strings = dns::chaos_txt(server, "id.server").await?;
            parse_freeform(&strings)
        }
    }
}

/// `res120.qyul1.on.quad9.net` → the label starting with `q` names the POP:
/// strip the `q` and the trailing node digits to get the IATA code.
fn parse_quad9(strings: &[String]) -> Option<Site> {
    let name = strings.first()?;
    let pop = name.split('.').find(|label| {
        label.len() >= 4
            && label.starts_with('q')
            && label[1..].chars().all(|c| c.is_ascii_alphanumeric())
    })?;
    let iata = pop[1..].trim_end_matches(|c: char| c.is_ascii_digit());
    (iata.len() == 3).then(|| Site::from_code(iata))
}

/// `yul01` → leading letters are the IATA code.
fn parse_cloudflare(strings: &[String]) -> Option<Site> {
    let answer = strings.first()?.trim();
    if answer.is_empty() || !answer.chars().all(|c| c.is_ascii_alphanumeric()) {
        return None;
    }
    let iata: String = answer
        .chars()
        .take_while(|c| c.is_ascii_alphabetic())
        .collect();
    (iata.len() == 3).then(|| Site::from_code(&iata))
}

/// `debug.opendns.com` answers several strings; the `server r3.yyz` line's
/// last dot-label is the IATA code.
fn parse_opendns(strings: &[String]) -> Option<Site> {
    let server = strings
        .iter()
        .find_map(|s| s.strip_prefix("server "))?
        .trim();
    let iata = server.rsplit('.').next()?;
    (iata.len() == 3 && iata.chars().all(|c| c.is_ascii_alphabetic()))
        .then(|| Site::from_code(iata))
}

/// Match the egress IP from `o-o.myaddr.l.google.com` against the
/// `locations.publicdns.goog` list of `<prefix> <code>` entries
/// (longest prefix wins).
fn parse_google(myaddr: &[String], locations: &[String]) -> Option<Site> {
    // myaddr returns the egress IP plus informational strings like
    // "edns0-client-subnet 203.0.113.0/24" — take the string that IS an IP.
    let egress: IpAddr = myaddr.iter().find_map(|s| s.trim().parse().ok())?;

    let mut best: Option<(u8, &str)> = None;
    for entry in locations {
        let mut parts = entry.split_whitespace();
        let (Some(prefix), Some(code)) = (parts.next(), parts.next()) else {
            continue;
        };
        let Some((net, len)) = parse_cidr(prefix) else {
            continue;
        };
        if cidr_contains(net, len, egress) && best.is_none_or(|(l, _)| len > l) {
            best = Some((len, code));
        }
    }
    let (_, code) = best?;
    Some(Site::from_code(code))
}

/// Free-form `id.server` strings, e.g.
/// `CleanBrowsing v1.6a - dns-edge-canada-toronto3` or `rcrsv4.cator1.ultradns`.
/// Pull the most site-looking token: the last `-`/`.` segment that isn't a
/// version or product name, digits stripped.
fn parse_freeform(strings: &[String]) -> Option<Site> {
    let answer = strings.first()?.trim();
    if answer.is_empty() {
        return None;
    }
    // Prefer what follows " - " when present, else the whole string.
    let tail = answer.rsplit(" - ").next().unwrap_or(answer);
    let token = tail
        .split(['-', '.', ' '])
        .map(|t| t.trim_end_matches(|c: char| c.is_ascii_digit()))
        .rfind(|t| {
            // Alphabetic, short enough to be a site name, and not a product
            // suffix like "ultradns" — those carry no location.
            (3..=12).contains(&t.len())
                && t.chars().all(|c| c.is_ascii_alphabetic())
                && !t.to_ascii_lowercase().contains("dns")
        })?;
    Some(Site::from_code(token))
}

/// Parse `a.b.c.d/len` or `x::/len`. IPv4 addresses are mapped into the
/// IPv6 space so one u128 comparison covers both.
fn parse_cidr(text: &str) -> Option<(u128, u8)> {
    let (addr, len) = text.split_once('/')?;
    let addr: IpAddr = addr.parse().ok()?;
    let len: u8 = len.parse().ok()?;
    let (bits, max) = ip_bits(addr);
    (len <= max).then_some((bits, len + (128 - max)))
}

fn cidr_contains(net: u128, len: u8, ip: IpAddr) -> bool {
    let (bits, _) = ip_bits(ip);
    let mask = if len == 0 {
        0
    } else {
        u128::MAX << (128 - len)
    };
    bits & mask == net & mask
}

/// An address as its IPv6-mapped bits plus the family's prefix-length span.
fn ip_bits(ip: IpAddr) -> (u128, u8) {
    match ip {
        IpAddr::V4(v4) => (v4.to_ipv6_mapped().to_bits(), 32),
        IpAddr::V6(v6) => (v6.to_bits(), 128),
    }
}

fn airport_coords(code: &str) -> Option<(f64, f64)> {
    AIRPORTS
        .binary_search_by(|(c, _, _)| c.cmp(&code))
        .ok()
        .map(|i| (AIRPORTS[i].1, AIRPORTS[i].2))
}

/// (IATA code, lat, lon) for cities that host POPs of the probed networks
/// (Cloudflare, Quad9/PCH, Google, OpenDNS). City-level accuracy is plenty
/// for the braille world map. Sorted by code for binary search.
const AIRPORTS: &[(&str, f64, f64)] = &[
    ("ABJ", 5.3, -3.9),
    ("ACC", 5.6, -0.2),
    ("ADD", 9.0, 38.8),
    ("ADL", -34.9, 138.5),
    ("AKL", -37.0, 174.8),
    ("ALA", 43.4, 77.0),
    ("ALG", 36.7, 3.2),
    ("AMD", 23.1, 72.6),
    ("AMM", 31.7, 36.0),
    ("AMS", 52.3, 4.8),
    ("ANC", 61.2, -150.0),
    ("ARN", 59.7, 17.9),
    ("ATH", 37.9, 23.9),
    ("ATL", 33.6, -84.4),
    ("AUH", 24.4, 54.7),
    ("AUS", 30.2, -97.7),
    ("BAH", 26.3, 50.6),
    ("BCN", 41.3, 2.1),
    ("BEG", 44.8, 20.3),
    ("BEL", -1.4, -48.5),
    ("BER", 52.4, 13.5),
    ("BEY", 33.8, 35.5),
    ("BGI", 13.1, -59.5),
    ("BGW", 33.3, 44.2),
    ("BKK", 13.7, 100.7),
    ("BLR", 13.2, 77.7),
    ("BNA", 36.1, -86.7),
    ("BNE", -27.4, 153.1),
    ("BOD", 44.8, -0.7),
    ("BOG", 4.7, -74.1),
    ("BOM", 19.1, 72.9),
    ("BOS", 42.4, -71.0),
    ("BRU", 50.9, 4.5),
    ("BSB", -15.9, -47.9),
    ("BUD", 47.4, 19.3),
    ("BUF", 42.9, -78.7),
    ("CAI", 30.1, 31.4),
    ("CAN", 23.4, 113.3),
    ("CBR", -35.3, 149.2),
    ("CCU", 22.7, 88.4),
    ("CDG", 49.0, 2.5),
    ("CEB", 10.3, 124.0),
    ("CGK", -6.1, 106.7),
    ("CHC", -43.5, 172.5),
    ("CKG", 29.7, 106.6),
    ("CLE", 41.4, -81.8),
    ("CLT", 35.2, -80.9),
    ("CMB", 7.2, 79.9),
    ("CMH", 40.0, -83.0),
    ("CMN", 33.4, -7.6),
    ("CNX", 18.8, 99.0),
    ("COK", 10.2, 76.4),
    ("COR", -31.3, -64.2),
    ("CPH", 55.6, 12.6),
    ("CPT", -34.0, 18.6),
    ("CTS", 42.8, 141.7),
    ("CTU", 30.6, 104.0),
    ("CUR", 12.2, -69.0),
    ("CWB", -25.5, -49.2),
    ("DAC", 23.8, 90.4),
    ("DAR", -6.9, 39.2),
    ("DEL", 28.6, 77.1),
    ("DEN", 39.9, -104.7),
    ("DFW", 32.9, -97.0),
    ("DKR", 14.7, -17.4),
    ("DLA", 4.0, 9.7),
    ("DMM", 26.5, 49.8),
    ("DOH", 25.3, 51.6),
    ("DPS", -8.7, 115.2),
    ("DTW", 42.2, -83.4),
    ("DUB", 53.4, -6.3),
    ("DUR", -29.6, 31.1),
    ("DUS", 51.3, 6.8),
    ("DXB", 25.3, 55.4),
    ("EBB", 0.0, 32.4),
    ("EDI", 55.9, -3.4),
    ("EVN", 40.1, 44.4),
    ("EWR", 40.7, -74.2),
    ("EZE", -34.8, -58.5),
    ("FCO", 41.8, 12.2),
    ("FOR", -3.8, -38.5),
    ("FRA", 50.0, 8.6),
    ("FUK", 33.6, 130.5),
    ("GDL", 20.5, -103.3),
    ("GIG", -22.8, -43.2),
    ("GOT", 57.7, 12.3),
    ("GRU", -23.4, -46.5),
    ("GUA", 14.6, -90.5),
    ("GUM", 13.5, 144.8),
    ("GVA", 46.2, 6.1),
    ("GYD", 40.5, 50.1),
    ("GYE", -2.2, -79.9),
    ("HAM", 53.6, 10.0),
    ("HAN", 21.2, 105.8),
    ("HEL", 60.3, 24.9),
    ("HGH", 30.2, 120.4),
    ("HKG", 22.3, 113.9),
    ("HND", 35.6, 139.8),
    ("HNL", 21.3, -157.9),
    ("HRE", -17.9, 31.1),
    ("HYD", 17.2, 78.4),
    ("IAD", 39.0, -77.5),
    ("IAH", 30.0, -95.3),
    ("ICN", 37.5, 126.4),
    ("IND", 39.7, -86.3),
    ("ISB", 33.6, 73.1),
    ("IST", 41.0, 28.8),
    ("JAX", 30.5, -81.7),
    ("JED", 21.7, 39.2),
    ("JFK", 40.6, -73.8),
    ("JIB", 11.5, 43.2),
    ("JNB", -26.1, 28.2),
    ("KBP", 50.3, 30.9),
    ("KEF", 64.0, -22.6),
    ("KGL", -2.0, 30.1),
    ("KHH", 22.6, 120.3),
    ("KHI", 24.9, 67.2),
    ("KIN", 18.0, -76.8),
    ("KIV", 46.9, 28.9),
    ("KIX", 34.4, 135.2),
    ("KTM", 27.7, 85.4),
    ("KUL", 2.7, 101.7),
    ("KWI", 29.2, 48.0),
    ("LAD", -8.9, 13.2),
    ("LAS", 36.1, -115.2),
    ("LAX", 33.9, -118.4),
    ("LED", 59.8, 30.3),
    ("LHE", 31.5, 74.4),
    ("LHR", 51.5, -0.5),
    ("LIM", -12.0, -77.1),
    ("LIS", 38.8, -9.1),
    ("LOS", 6.6, 3.3),
    ("LPA", 27.9, -15.4),
    ("LPB", -16.5, -68.2),
    ("LUN", -15.3, 28.5),
    ("LUX", 49.6, 6.2),
    ("LYS", 45.7, 5.1),
    ("MAA", 13.0, 80.2),
    ("MAD", 40.5, -3.6),
    ("MAN", 53.4, -2.3),
    ("MAO", -3.0, -60.0),
    ("MBA", -4.0, 39.6),
    ("MCI", 39.3, -94.7),
    ("MCO", 28.4, -81.3),
    ("MCT", 23.6, 58.3),
    ("MDE", 6.2, -75.6),
    ("MEL", -37.7, 144.8),
    ("MEM", 35.0, -90.0),
    ("MEX", 19.4, -99.1),
    ("MFM", 22.1, 113.6),
    ("MIA", 25.8, -80.3),
    ("MNL", 14.5, 121.0),
    ("MPM", -25.9, 32.6),
    ("MRS", 43.4, 5.2),
    ("MRU", -20.4, 57.7),
    ("MSP", 44.9, -93.2),
    ("MSY", 30.0, -90.3),
    ("MTY", 25.8, -100.1),
    ("MUC", 48.4, 11.8),
    ("MVD", -34.8, -56.0),
    ("MXP", 45.6, 8.7),
    ("NBO", -1.3, 36.9),
    ("NOU", -22.0, 166.2),
    ("NRT", 35.8, 140.4),
    ("OKA", 26.2, 127.6),
    ("OKC", 35.4, -97.6),
    ("OMA", 41.3, -95.9),
    ("ORD", 42.0, -87.9),
    ("OSL", 60.2, 11.1),
    ("OTP", 44.6, 26.1),
    ("PDX", 45.6, -122.6),
    ("PER", -31.9, 116.0),
    ("PHL", 39.9, -75.2),
    ("PHX", 33.4, -112.0),
    ("PIT", 40.5, -80.2),
    ("PMO", 38.2, 13.1),
    ("PNH", 11.5, 104.9),
    ("POA", -30.0, -51.2),
    ("POM", -9.4, 147.2),
    ("POS", 10.6, -61.4),
    ("PPT", -17.6, -149.6),
    ("PRG", 50.1, 14.3),
    ("PTY", 9.1, -79.4),
    ("PVG", 31.1, 121.8),
    ("QRO", 20.6, -100.4),
    ("RDU", 35.9, -78.8),
    ("REC", -8.1, -34.9),
    ("RGN", 16.9, 96.1),
    ("RIC", 37.5, -77.3),
    ("RIX", 56.9, 24.0),
    ("RUH", 25.0, 46.7),
    ("SAN", 32.7, -117.2),
    ("SAT", 29.5, -98.5),
    ("SCL", -33.4, -70.8),
    ("SDQ", 18.4, -69.7),
    ("SEA", 47.4, -122.3),
    ("SFO", 37.6, -122.4),
    ("SGN", 10.8, 106.7),
    ("SIN", 1.4, 104.0),
    ("SJC", 37.4, -121.9),
    ("SJO", 10.0, -84.2),
    ("SJU", 18.4, -66.0),
    ("SKG", 40.5, 23.0),
    ("SKP", 42.0, 21.6),
    ("SLC", 40.8, -112.0),
    ("SMF", 38.7, -121.6),
    ("SOF", 42.7, 23.4),
    ("SSA", -12.9, -38.3),
    ("STL", 38.7, -90.4),
    ("STR", 48.7, 9.2),
    ("SUB", -7.4, 112.8),
    ("SUV", -18.0, 178.4),
    ("SYD", -33.9, 151.2),
    ("SZX", 22.6, 113.8),
    ("TAS", 41.3, 69.3),
    ("TBS", 41.7, 45.0),
    ("TGU", 14.1, -87.2),
    ("TIA", 41.4, 19.7),
    ("TLH", 30.4, -84.4),
    ("TLL", 59.4, 24.8),
    ("TLV", 32.0, 34.9),
    ("TNR", -18.8, 47.5),
    ("TPA", 28.0, -82.5),
    ("TPE", 25.1, 121.2),
    ("TSN", 39.1, 117.3),
    ("TUN", 36.9, 10.2),
    ("UIO", -0.1, -78.4),
    ("ULN", 47.8, 106.8),
    ("VIE", 48.1, 16.6),
    ("VNO", 54.6, 25.3),
    ("VTE", 18.0, 102.6),
    ("WAW", 52.2, 21.0),
    ("WLG", -41.3, 174.8),
    ("WUH", 30.8, 114.2),
    ("XIY", 34.4, 108.8),
    ("YHZ", 44.9, -63.5),
    ("YOW", 45.3, -75.7),
    ("YUL", 45.5, -73.7),
    ("YVR", 49.2, -123.2),
    ("YWG", 49.9, -97.2),
    ("YYC", 51.1, -114.0),
    ("YYZ", 43.7, -79.6),
    ("ZAG", 45.7, 16.1),
    ("ZRH", 47.5, 8.6),
];

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn airports_are_sorted_unique_and_in_range() {
        for pair in AIRPORTS.windows(2) {
            assert!(pair[0].0 < pair[1].0, "{} out of order", pair[1].0);
        }
        for &(code, lat, lon) in AIRPORTS {
            assert_eq!(code.len(), 3);
            assert!(code.chars().all(|c| c.is_ascii_uppercase()));
            assert!((-90.0..=90.0).contains(&lat), "{code} lat");
            assert!((-180.0..=180.0).contains(&lon), "{code} lon");
        }
    }

    #[test]
    fn quad9_pop_name_yields_iata() {
        let site = parse_quad9(&strings(&["res120.qyul1.on.quad9.net"])).unwrap();
        assert_eq!(site.code, "YUL");
        assert!(site.coords.is_some());
        // Older PCH-hosted form.
        let site = parse_quad9(&strings(&["res100.qzrh2.rrdns.pch.net"])).unwrap();
        assert_eq!(site.code, "ZRH");
        assert!(parse_quad9(&strings(&["unexpected"])).is_none());
    }

    #[test]
    fn cloudflare_colo_yields_iata() {
        let site = parse_cloudflare(&strings(&["yul01"])).unwrap();
        assert_eq!(site.code, "YUL");
        assert_eq!(site.coords, Some((45.5, -73.7)));
        assert!(parse_cloudflare(&strings(&["not-a-colo"])).is_none());
    }

    #[test]
    fn opendns_server_line_yields_iata() {
        let answer = strings(&[
            "server r3.yyz",
            "flags 22040030 0 70 4001800000000000",
            "originid 0",
        ]);
        let site = parse_opendns(&answer).unwrap();
        assert_eq!(site.code, "YYZ");
        assert!(site.coords.is_some());
        assert!(parse_opendns(&strings(&["flags 0"])).is_none());
    }

    #[test]
    fn google_longest_prefix_wins() {
        let myaddr = strings(&["edns0-client-subnet 203.0.113.0/24", "34.64.1.9"]);
        let locations = strings(&[
            "34.64.0.0/16 xxx ",
            "34.64.1.0/24 icn ",
            "74.114.28.64/26 bkk ",
        ]);
        let site = parse_google(&myaddr, &locations).unwrap();
        assert_eq!(site.code, "ICN");
        assert!(site.coords.is_some());
    }

    #[test]
    fn google_matches_ipv6_prefixes() {
        let myaddr = strings(&["2404:6800:4008:c06::1"]);
        let locations = strings(&["2404:6800:4008::/48 tpe ", "192.0.2.0/24 xxx "]);
        let site = parse_google(&myaddr, &locations).unwrap();
        assert_eq!(site.code, "TPE");
    }

    #[test]
    fn google_without_match_is_none() {
        let myaddr = strings(&["198.51.100.7"]);
        let locations = strings(&["34.64.1.0/24 icn "]);
        assert!(parse_google(&myaddr, &locations).is_none());
    }

    #[test]
    fn freeform_extracts_a_site_token() {
        let site = parse_freeform(&strings(&[
            "CleanBrowsing v1.6a - dns-edge-canada-toronto3",
        ]))
        .unwrap();
        assert_eq!(site.code, "TORONTO");
        assert!(site.coords.is_none());

        let site = parse_freeform(&strings(&["rcrsv4.cator1.ultradns"])).unwrap();
        assert_eq!(site.code, "CATOR");

        assert!(parse_freeform(&strings(&[""])).is_none());
    }

    #[test]
    fn cidr_matching_handles_edge_lengths() {
        let (net, len) = parse_cidr("0.0.0.0/0").unwrap();
        assert!(cidr_contains(net, len, "203.0.113.9".parse().unwrap()));
        let (net, len) = parse_cidr("203.0.113.9/32").unwrap();
        assert!(cidr_contains(net, len, "203.0.113.9".parse().unwrap()));
        assert!(!cidr_contains(net, len, "203.0.113.8".parse().unwrap()));
        // A v4 prefix must not swallow v6 addresses.
        let (net, len) = parse_cidr("0.0.0.0/0").unwrap();
        assert!(!cidr_contains(net, len, "2001:db8::1".parse().unwrap()));
        assert!(parse_cidr("34.64.0.0/33").is_none());
    }
}
