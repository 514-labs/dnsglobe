use std::net::{IpAddr, SocketAddr};
use std::str::FromStr;
use std::time::{Duration, Instant};

use hickory_resolver::TokioResolver;
use hickory_resolver::config::{NameServerConfig, ResolveHosts, ResolverConfig};
use hickory_resolver::net::runtime::TokioRuntimeProvider;
use hickory_resolver::net::{DnsError, NetError};
use hickory_resolver::proto::op::{Edns, Message, Query, ResponseCode};
use hickory_resolver::proto::rr::rdata::opt::{EdnsCode, EdnsOption};
use hickory_resolver::proto::rr::{DNSClass, Name, RData, RecordType};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub use hickory_resolver::proto::rr::rdata::opt::ClientSubnet;

const QUERY_TIMEOUT: Duration = Duration::from_secs(3);

/// EDNS UDP payload size advertised on the raw ECS path — the post-flag-day
/// value (RFC 9715) hickory's resolver path also defaults to.
const EDNS_PAYLOAD: u16 = 1232;

#[derive(Debug, Clone)]
pub enum QueryResult {
    /// Record values (rdata strings) and the minimum TTL seen.
    Records { values: Vec<String>, min_ttl: u32 },
    /// The server answered that the record does not exist (NXDOMAIN or
    /// NOERROR with an empty answer section). This is a real propagation
    /// signal — the server's view is "nothing there" — so it counts toward
    /// the responding total.
    NoRecords(String),
    /// SERVFAIL: the resolver tried to resolve this name and could not —
    /// typically a delegation pointing at dead nameservers (mid-NS-migration
    /// with the old servers gone) or a DNSSEC validation failure. Unlike
    /// REFUSED or a timeout this is a statement about the *domain*, so it
    /// counts toward the responding total and blocks 100% propagation.
    ServFail,
    /// No usable answer: timeout, network error, or the server refused to
    /// serve us (REFUSED). Says nothing about propagation, so these are
    /// excluded from the percentage.
    Error(String),
}

#[derive(Debug)]
pub struct QueryOutcome {
    pub resolver_index: usize,
    pub generation: u64,
    pub result: QueryResult,
    pub elapsed: Duration,
    /// Some(_) only when the round carried an ECS option and the resolver
    /// gave a real answer: whether the response echoed the option back
    /// (RFC 7871 §7.2.2). No echo means the resolver ignored ECS — its
    /// answer describes its own vantage point, not the probed subnet.
    pub ecs_honored: Option<bool>,
}

/// One-server resolver going straight at `server` (no cache, single attempt)
/// so that server's own view of a record is what we measure.
fn build_resolver(server: IpAddr) -> Result<TokioResolver, NetError> {
    let mut config = ResolverConfig::default();
    // One server entry with both UDP and TCP connections: hickory retries
    // over TCP when a UDP answer comes back truncated (large TXT sets, long
    // MX lists, …).
    config
        .name_servers
        .push(NameServerConfig::udp_and_tcp(server));

    let mut builder = TokioResolver::builder_with_config(config, TokioRuntimeProvider::default());
    let opts = builder.options_mut();
    opts.timeout = QUERY_TIMEOUT;
    opts.attempts = 1;
    opts.cache_size = 0;
    opts.use_hosts_file = ResolveHosts::Never;
    opts.edns0 = true; // allow >512-byte UDP answers
    builder.build()
}

/// Query a single upstream resolver directly (no cache, single attempt) so
/// each server's own view of the record is what we measure. With `ecs` set
/// the query carries that client subnet and the third return value reports
/// whether the resolver honored it (see `QueryOutcome::ecs_honored`).
pub async fn query(
    server: IpAddr,
    domain: String,
    rtype: RecordType,
    ecs: Option<ClientSubnet>,
) -> (QueryResult, Duration, Option<bool>) {
    // ECS can't ride the high-level resolver (it has no per-query EDNS
    // hook), so those queries take the raw-message path instead.
    if let Some(subnet) = ecs {
        let start = Instant::now();
        let outcome = tokio::time::timeout(
            QUERY_TIMEOUT + Duration::from_secs(1),
            ecs_query(server, &domain, rtype, subnet),
        )
        .await
        .unwrap_or((QueryResult::Error("timeout".into()), None));
        return (outcome.0, start.elapsed(), outcome.1);
    }

    let resolver = match build_resolver(server) {
        Ok(resolver) => resolver,
        Err(err) => {
            return (
                QueryResult::Error(short_error(err.to_string())),
                Duration::ZERO,
                None,
            );
        }
    };

    let start = Instant::now();
    let lookup = tokio::time::timeout(
        QUERY_TIMEOUT + Duration::from_secs(1),
        resolver.lookup(domain.as_str(), rtype),
    )
    .await;
    let elapsed = start.elapsed();

    let result = match lookup {
        Err(_) => QueryResult::Error("timeout".into()),
        Ok(Err(err)) => match err {
            NetError::Timeout => QueryResult::Error("timeout".into()),
            // "Won't serve you" — a policy about us as a client, not a
            // statement about whether the record exists.
            NetError::Dns(DnsError::ResponseCode(ResponseCode::Refused)) => {
                QueryResult::Error("refused".into())
            }
            // hickory's error drops the EDE detail (e.g. "no reachable
            // authority"), so the classification is all we get.
            NetError::Dns(DnsError::ResponseCode(ResponseCode::ServFail)) => QueryResult::ServFail,
            NetError::Dns(DnsError::ResponseCode(code)) => QueryResult::Error(code.to_string()),
            NetError::Dns(DnsError::NoRecordsFound(no_records)) => {
                QueryResult::NoRecords(no_records.response_code.to_string())
            }
            other => QueryResult::Error(short_error(other.to_string())),
        },
        Ok(Ok(lookup)) => collect_answers(lookup.answers(), rtype),
    };

    (result, elapsed, None)
}

/// Fold an answer section into `Records`/`NoRecords`, shared by the resolver
/// path and the raw ECS path so both classify identically.
fn collect_answers(
    answers: &[hickory_resolver::proto::rr::Record],
    rtype: RecordType,
) -> QueryResult {
    let mut values: Vec<String> = Vec::new();
    let mut min_ttl = u32::MAX;
    for record in answers {
        min_ttl = min_ttl.min(record.ttl);
        // A lookup can carry other types too (e.g. the CNAME hops on
        // the way to an A record); label those so answers stay
        // comparable across resolvers.
        if record.record_type() == rtype {
            values.push(record.data.to_string());
        } else {
            values.push(format!("{} {}", record.record_type(), record.data));
        }
    }
    values.sort();
    values.dedup();
    if values.is_empty() {
        QueryResult::NoRecords("empty answer".into())
    } else {
        QueryResult::Records { values, min_ttl }
    }
}

/// Parse `--ecs`/config subnet syntax: `203.0.113.0/24`, `2001:db8::/56`, or
/// a bare address (full-length prefix, like dig's +subnet). Host bits beyond
/// the prefix are zeroed — RFC 7871 requires zero padding and some servers
/// FORMERR on violations. Scope is always sent as 0 (§7.1.2).
pub fn parse_ecs(s: &str) -> Result<ClientSubnet, String> {
    let (addr, prefix) = match s.split_once('/') {
        Some((addr, prefix)) => (
            addr,
            Some(
                prefix
                    .parse::<u8>()
                    .map_err(|_| format!("invalid prefix length {prefix:?}"))?,
            ),
        ),
        None => (s, None),
    };
    let addr: IpAddr = addr
        .parse()
        .map_err(|_| format!("invalid IP address {addr:?}"))?;
    let max = if addr.is_ipv4() { 32 } else { 128 };
    let prefix = prefix.unwrap_or(max);
    if prefix > max {
        return Err(format!("prefix /{prefix} too long for {addr} (max /{max})"));
    }
    let addr = match addr {
        IpAddr::V4(v4) => {
            let mask = (u64::MAX << (32 - u64::from(prefix))) as u32;
            IpAddr::V4((u32::from(v4) & mask).into())
        }
        IpAddr::V6(v6) => {
            let mask = if prefix == 0 {
                0
            } else {
                u128::MAX << (128 - u32::from(prefix))
            };
            IpAddr::V6((u128::from(v6) & mask).into())
        }
    };
    Ok(ClientSubnet::new(addr, prefix, 0))
}

/// `addr/prefix` display form (`ClientSubnet` has no `Display` of its own).
pub fn fmt_ecs(subnet: &ClientSubnet) -> String {
    format!("{}/{}", subnet.addr(), subnet.source_prefix())
}

/// Recursive query carrying the client subnet in an EDNS OPT record.
fn ecs_message(domain: &str, rtype: RecordType, subnet: ClientSubnet) -> Option<Message> {
    let mut message = Message::query();
    message.metadata.recursion_desired = true;
    message.add_query(Query::query(Name::from_str_relaxed(domain).ok()?, rtype));
    let mut edns = Edns::new();
    edns.set_max_payload(EDNS_PAYLOAD);
    edns.options_mut().insert(EdnsOption::Subnet(subnet));
    message.edns = Some(edns);
    Some(message)
}

/// Map a raw response to the same `QueryResult` the resolver path yields,
/// plus whether it echoed our ECS option (RFC 7871 §7.2.2 — the echo means
/// the resolver used the subnet; resolvers that deliberately ignore ECS,
/// like Cloudflare or Quad9, omit it). Only real answers get an echo
/// verdict: an error says nothing either way.
fn classify_ecs_response(response: &Message, rtype: RecordType) -> (QueryResult, Option<bool>) {
    let honored = response
        .edns
        .as_ref()
        .is_some_and(|edns| edns.option(EdnsCode::Subnet).is_some());
    match response.metadata.response_code {
        ResponseCode::NoError => (collect_answers(&response.answers, rtype), Some(honored)),
        ResponseCode::NXDomain => (
            QueryResult::NoRecords(ResponseCode::NXDomain.to_string()),
            Some(honored),
        ),
        ResponseCode::Refused => (QueryResult::Error("refused".into()), None),
        ResponseCode::ServFail => (QueryResult::ServFail, None),
        code => (QueryResult::Error(code.to_string()), None),
    }
}

async fn ecs_query(
    server: IpAddr,
    domain: &str,
    rtype: RecordType,
    subnet: ClientSubnet,
) -> (QueryResult, Option<bool>) {
    let Some(message) = ecs_message(domain, rtype, subnet) else {
        return (QueryResult::Error("invalid name".into()), None);
    };
    match exchange(server, &message).await {
        Ok(response) => classify_ecs_response(&response, rtype),
        Err(err) => (err, None),
    }
}

/// One request/response exchange: UDP first, falling back to TCP when the
/// answer comes back truncated — mirroring what the resolver path's
/// udp_and_tcp connection pair does.
async fn exchange(server: IpAddr, message: &Message) -> Result<Message, QueryResult> {
    let io_err = |err: std::io::Error| QueryResult::Error(short_error(err.to_string()));
    let request = message
        .to_vec()
        .map_err(|err| QueryResult::Error(short_error(err.to_string())))?;

    let bind: SocketAddr = match server {
        IpAddr::V4(_) => ([0, 0, 0, 0], 0).into(),
        IpAddr::V6(_) => (std::net::Ipv6Addr::UNSPECIFIED, 0).into(),
    };
    let socket = tokio::net::UdpSocket::bind(bind).await.map_err(io_err)?;
    // Connecting filters responses to this server's address.
    socket.connect((server, 53)).await.map_err(io_err)?;
    socket.send(&request).await.map_err(io_err)?;

    let deadline = Instant::now() + QUERY_TIMEOUT;
    let response = loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| QueryResult::Error("timeout".into()))?;
        let mut buf = [0u8; 4096];
        let len = tokio::time::timeout(remaining, socket.recv(&mut buf))
            .await
            .map_err(|_| QueryResult::Error("timeout".into()))?
            .map_err(io_err)?;
        // A stray or spoofed datagram with the wrong id: keep listening
        // until the deadline rather than failing the query on it.
        if let Ok(response) = Message::from_vec(&buf[..len])
            && response.metadata.id == message.metadata.id
        {
            break response;
        }
    };
    if !response.metadata.truncation {
        return Ok(response);
    }
    // Truncated: the full answer only fits over TCP. If TCP fails too, the
    // truncated UDP answer is still better than nothing.
    Ok(exchange_tcp(server, &request, message.metadata.id)
        .await
        .unwrap_or(response))
}

async fn exchange_tcp(server: IpAddr, request: &[u8], id: u16) -> Option<Message> {
    let mut stream =
        tokio::time::timeout(QUERY_TIMEOUT, tokio::net::TcpStream::connect((server, 53)))
            .await
            .ok()?
            .ok()?;
    // DNS-over-TCP frames every message with a 2-byte length prefix.
    let mut framed = Vec::with_capacity(request.len() + 2);
    framed.extend_from_slice(&u16::try_from(request.len()).ok()?.to_be_bytes());
    framed.extend_from_slice(request);
    tokio::time::timeout(QUERY_TIMEOUT, stream.write_all(&framed))
        .await
        .ok()?
        .ok()?;
    let mut len_buf = [0u8; 2];
    tokio::time::timeout(QUERY_TIMEOUT, stream.read_exact(&mut len_buf))
        .await
        .ok()?
        .ok()?;
    let mut buf = vec![0u8; usize::from(u16::from_be_bytes(len_buf))];
    tokio::time::timeout(QUERY_TIMEOUT, stream.read_exact(&mut buf))
        .await
        .ok()?
        .ok()?;
    let response = Message::from_vec(&buf).ok()?;
    (response.metadata.id == id).then_some(response)
}

/// IN TXT query returning each TXT character-string separately (a record's
/// strings are answers like `"prefix code"` entries — joining them would
/// destroy the structure). Used by the anycast site probes; None on any
/// failure, since a failed probe just means "site unknown".
pub async fn txt_strings(server: IpAddr, name: &str) -> Option<Vec<String>> {
    let resolver = build_resolver(server).ok()?;
    let lookup = tokio::time::timeout(
        QUERY_TIMEOUT + Duration::from_secs(1),
        resolver.lookup(name, RecordType::TXT),
    )
    .await
    .ok()?
    .ok()?;
    let strings: Vec<String> = lookup
        .answers()
        .iter()
        .filter_map(|record| match &record.data {
            RData::TXT(txt) => Some(&txt.txt_data),
            _ => None,
        })
        .flat_map(|data| data.iter())
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect();
    (!strings.is_empty()).then_some(strings)
}

/// CHAOS-class TXT query (`id.server` etc.), which the high-level resolver
/// can't express — hand-rolled over UDP. Answers are tiny, so no truncation
/// handling. None on any failure.
pub async fn chaos_txt(server: IpAddr, name: &str) -> Option<Vec<String>> {
    let mut query = Query::query(Name::from_str(name).ok()?, RecordType::TXT);
    query.set_query_class(DNSClass::CH);
    let mut message = Message::query();
    message.metadata.recursion_desired = false;
    message.add_query(query);
    let request = message.to_vec().ok()?;

    let bind: std::net::SocketAddr = match server {
        IpAddr::V4(_) => ([0, 0, 0, 0], 0).into(),
        IpAddr::V6(_) => (std::net::Ipv6Addr::UNSPECIFIED, 0).into(),
    };
    let socket = tokio::net::UdpSocket::bind(bind).await.ok()?;
    // Connecting filters responses to this server's address.
    socket.connect((server, 53)).await.ok()?;
    socket.send(&request).await.ok()?;

    let mut buf = [0u8; 4096];
    let len = tokio::time::timeout(QUERY_TIMEOUT, socket.recv(&mut buf))
        .await
        .ok()?
        .ok()?;
    let response = Message::from_vec(&buf[..len]).ok()?;
    if response.metadata.id != message.metadata.id {
        return None;
    }
    let strings: Vec<String> = response
        .answers
        .iter()
        .filter_map(|record| match &record.data {
            RData::TXT(txt) => Some(&txt.txt_data),
            _ => None,
        })
        .flat_map(|data| data.iter())
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect();
    (!strings.is_empty()).then_some(strings)
}

fn short_error(message: String) -> String {
    let lower = message.to_ascii_lowercase();
    if lower.contains("timed out") || lower.contains("timeout") {
        return "timeout".into();
    }
    if lower.contains("refused") {
        return "refused".into();
    }
    // Truncate in place, backing off to a char boundary at or below 48 bytes
    // (String::truncate panics mid-codepoint).
    let mut message = message;
    let mut end = message.len().min(48);
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    message.truncate(end);
    message
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_timeout_and_refused_messages() {
        assert_eq!(short_error("request timed out".into()), "timeout");
        assert_eq!(short_error("Connection REFUSED by peer".into()), "refused");
    }

    #[test]
    fn short_messages_pass_through() {
        assert_eq!(short_error("proto error".into()), "proto error");
    }

    #[test]
    fn parse_ecs_accepts_cidrs_and_bare_addresses() {
        let subnet = parse_ecs("203.0.113.0/24").unwrap();
        assert_eq!(fmt_ecs(&subnet), "203.0.113.0/24");
        assert_eq!(subnet.scope_prefix(), 0);
        // Bare address: full-length prefix, like dig's +subnet.
        assert_eq!(
            fmt_ecs(&parse_ecs("198.51.100.7").unwrap()),
            "198.51.100.7/32"
        );
        assert_eq!(
            fmt_ecs(&parse_ecs("2001:db8::1").unwrap()),
            "2001:db8::1/128"
        );
        assert_eq!(
            fmt_ecs(&parse_ecs("2001:db8::/56").unwrap()),
            "2001:db8::/56"
        );
    }

    #[test]
    fn parse_ecs_zeroes_host_bits() {
        // RFC 7871 §6: bits beyond the source prefix must be zero.
        assert_eq!(
            fmt_ecs(&parse_ecs("203.0.113.77/24").unwrap()),
            "203.0.113.0/24"
        );
        assert_eq!(
            fmt_ecs(&parse_ecs("10.255.255.255/20").unwrap()),
            "10.255.240.0/20"
        );
        assert_eq!(fmt_ecs(&parse_ecs("192.0.2.1/0").unwrap()), "0.0.0.0/0");
        assert_eq!(
            fmt_ecs(&parse_ecs("2001:db8:ffff:ffff::1/56").unwrap()),
            "2001:db8:ffff:ff00::/56"
        );
    }

    #[test]
    fn parse_ecs_rejects_garbage() {
        assert!(parse_ecs("not-an-ip").unwrap_err().contains("IP address"));
        assert!(parse_ecs("10.0.0.0/33").unwrap_err().contains("too long"));
        assert!(
            parse_ecs("2001:db8::/129")
                .unwrap_err()
                .contains("too long")
        );
        assert!(
            parse_ecs("10.0.0.0/x")
                .unwrap_err()
                .contains("prefix length")
        );
        assert!(parse_ecs("").unwrap_err().contains("IP address"));
    }

    #[test]
    fn ecs_message_carries_the_subnet_option() {
        let subnet = parse_ecs("203.0.113.0/24").unwrap();
        let message = ecs_message("example.com", RecordType::A, subnet).unwrap();
        assert!(message.metadata.recursion_desired);
        let edns = message.edns.as_ref().unwrap();
        assert_eq!(
            edns.option(EdnsCode::Subnet),
            Some(&EdnsOption::Subnet(subnet))
        );
    }

    /// A response as classify sees it: flip the id-generated query into a
    /// response with the given code, answers, and (optionally) an ECS echo.
    fn response(
        code: ResponseCode,
        answers: Vec<hickory_resolver::proto::rr::Record>,
        echo: bool,
    ) -> Message {
        let mut message = Message::response(0, hickory_resolver::proto::op::OpCode::Query);
        message.metadata.response_code = code;
        message.answers = answers;
        if echo {
            let mut edns = Edns::new();
            edns.options_mut()
                .insert(EdnsOption::Subnet(parse_ecs("203.0.113.0/24").unwrap()));
            message.edns = Some(edns);
        }
        message
    }

    fn a_record(ip: &str, ttl: u32) -> hickory_resolver::proto::rr::Record {
        use hickory_resolver::proto::rr::rdata::A;
        hickory_resolver::proto::rr::Record::from_rdata(
            Name::from_str("example.com.").unwrap(),
            ttl,
            RData::A(A::from_str(ip).unwrap()),
        )
    }

    #[test]
    fn ecs_echo_detection_only_applies_to_real_answers() {
        let (result, honored) = classify_ecs_response(
            &response(ResponseCode::NoError, vec![a_record("192.0.2.1", 60)], true),
            RecordType::A,
        );
        assert!(matches!(result, QueryResult::Records { .. }));
        assert_eq!(honored, Some(true));

        // No echo: the resolver ignored ECS (Cloudflare, Quad9, …).
        let (_, honored) = classify_ecs_response(
            &response(
                ResponseCode::NoError,
                vec![a_record("192.0.2.1", 60)],
                false,
            ),
            RecordType::A,
        );
        assert_eq!(honored, Some(false));

        let (result, honored) = classify_ecs_response(
            &response(ResponseCode::NXDomain, vec![], false),
            RecordType::A,
        );
        assert!(matches!(result, QueryResult::NoRecords(_)));
        assert_eq!(honored, Some(false));

        // Errors carry no echo verdict — they say nothing about the subnet.
        let (result, honored) = classify_ecs_response(
            &response(ResponseCode::Refused, vec![], false),
            RecordType::A,
        );
        assert!(matches!(result, QueryResult::Error(_)));
        assert_eq!(honored, None);
        let (result, honored) = classify_ecs_response(
            &response(ResponseCode::ServFail, vec![], false),
            RecordType::A,
        );
        assert!(matches!(result, QueryResult::ServFail));
        assert_eq!(honored, None);
    }

    #[test]
    fn ecs_classification_matches_the_resolver_path() {
        // Empty NoError answer → "empty answer", like the lookup path.
        let (result, _) = classify_ecs_response(
            &response(ResponseCode::NoError, vec![], true),
            RecordType::A,
        );
        assert!(matches!(result, QueryResult::NoRecords(code) if code == "empty answer"));

        // Min TTL across records, values sorted and deduped.
        let answers = vec![
            a_record("192.0.2.9", 300),
            a_record("192.0.2.1", 60),
            a_record("192.0.2.9", 300),
        ];
        let (result, _) = classify_ecs_response(
            &response(ResponseCode::NoError, answers, true),
            RecordType::A,
        );
        match result {
            QueryResult::Records { values, min_ttl } => {
                assert_eq!(values, vec!["192.0.2.1", "192.0.2.9"]);
                assert_eq!(min_ttl, 60);
            }
            other => panic!("expected records, got {other:?}"),
        }
    }

    #[test]
    fn truncation_respects_char_boundaries() {
        // A multi-byte char straddling the 48-byte cut used to panic.
        let message = format!("{}éxxxx", "x".repeat(47));
        assert_eq!(short_error(message), "x".repeat(47));

        let long_ascii = "e".repeat(80);
        assert_eq!(short_error(long_ascii), "e".repeat(48));
    }
}
