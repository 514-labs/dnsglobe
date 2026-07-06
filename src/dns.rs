use std::net::IpAddr;
use std::time::{Duration, Instant};

use hickory_resolver::TokioResolver;
use hickory_resolver::config::{NameServerConfig, ResolveHosts, ResolverConfig};
use hickory_resolver::net::runtime::TokioRuntimeProvider;
use hickory_resolver::net::{DnsError, NetError};
use hickory_resolver::proto::op::ResponseCode;
use hickory_resolver::proto::rr::{RData, RecordType};

const QUERY_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug, Clone)]
pub enum QueryResult {
    /// Record values (rdata strings) and the minimum TTL seen.
    Records { values: Vec<String>, min_ttl: u32 },
    /// The server answered that the record does not exist (NXDOMAIN or
    /// NOERROR with an empty answer section). This is a real propagation
    /// signal — the server's view is "nothing there" — so it counts toward
    /// the responding total.
    NoRecords(String),
    /// No usable answer: timeout, network error, or the server refused to
    /// serve us (REFUSED/SERVFAIL). Says nothing about propagation, so these
    /// are excluded from the percentage.
    Error(String),
}

#[derive(Debug)]
pub struct QueryOutcome {
    pub resolver_index: usize,
    pub generation: u64,
    pub result: QueryResult,
    pub elapsed: Duration,
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
/// each server's own view of the record is what we measure.
pub async fn query(server: IpAddr, domain: String, rtype: RecordType) -> (QueryResult, Duration) {
    let resolver = match build_resolver(server) {
        Ok(resolver) => resolver,
        Err(err) => {
            return (
                QueryResult::Error(short_error(err.to_string())),
                Duration::ZERO,
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
            // "Won't serve you" / "couldn't resolve" — not a statement about
            // whether the record exists.
            NetError::Dns(DnsError::ResponseCode(ResponseCode::Refused)) => {
                QueryResult::Error("refused".into())
            }
            NetError::Dns(DnsError::ResponseCode(code)) => QueryResult::Error(code.to_string()),
            NetError::Dns(DnsError::NoRecordsFound(no_records)) => {
                QueryResult::NoRecords(no_records.response_code.to_string())
            }
            other => QueryResult::Error(short_error(other.to_string())),
        },
        Ok(Ok(lookup)) => {
            let mut values: Vec<String> = Vec::new();
            let mut min_ttl = u32::MAX;
            for record in lookup.answers() {
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
    };

    (result, elapsed)
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
    use hickory_resolver::proto::op::{Message, Query};
    use hickory_resolver::proto::rr::{DNSClass, Name};
    use std::str::FromStr;

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
    use super::short_error;

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
    fn truncation_respects_char_boundaries() {
        // A multi-byte char straddling the 48-byte cut used to panic.
        let message = format!("{}éxxxx", "x".repeat(47));
        assert_eq!(short_error(message), "x".repeat(47));

        let long_ascii = "e".repeat(80);
        assert_eq!(short_error(long_ascii), "e".repeat(48));
    }
}
