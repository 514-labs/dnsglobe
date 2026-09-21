use std::net::IpAddr;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::Query;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use dnsglobe::dns::{self, QueryResult};
use dnsglobe::resolvers;
use hickory_resolver::proto::rr::RecordType;
use serde::Deserialize;
use serde_json::json;
use tokio::task::JoinSet;

static INDEX_HTML: &str = include_str!("../static/index.html");
static VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Deserialize)]
struct WsParams {
    domain: String,
    #[serde(rename = "type", default = "default_rtype")]
    rtype: String,
}

fn default_rtype() -> String {
    "A".to_string()
}

async fn index() -> impl IntoResponse {
    Html(INDEX_HTML.replace("{{VERSION}}", VERSION))
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    Query(params): Query<WsParams>,
) -> Response {
    ws.on_upgrade(move |socket| handle_ws(socket, params))
}

async fn handle_ws(mut socket: WebSocket, params: WsParams) {
    let rtype = match params.rtype.to_uppercase().as_str() {
        "A" => RecordType::A,
        "AAAA" => RecordType::AAAA,
        "CNAME" => RecordType::CNAME,
        "MX" => RecordType::MX,
        "NS" => RecordType::NS,
        "TXT" => RecordType::TXT,
        "SOA" => RecordType::SOA,
        other => {
            let _ = socket
                .send(Message::Text(
                    json!({"error": format!("unknown record type: {other}")}).to_string().into(),
                ))
                .await;
            return;
        }
    };

    let resolvers = resolvers::defaults();
    let total = resolvers.len();

    // Send initial pending messages so the frontend can populate the table immediately.
    for (i, r) in resolvers.iter().enumerate() {
        let msg = json!({
            "index": i,
            "resolver": r.name,
            "ip": r.ip.to_string(),
            "location": r.location,
            "status": "pending",
            "answer": [],
            "ttl": null,
            "ping_ms": null,
            "lat": r.coords.map(|(lat, _)| lat),
            "lon": r.coords.map(|(_, lon)| lon),
            "total": total,
        });
        if socket.send(Message::Text(msg.to_string().into())).await.is_err() {
            return;
        }
    }

    let domain = params.domain.clone();
    let mut tasks: JoinSet<(usize, serde_json::Value)> = JoinSet::new();

    for (index, resolver) in resolvers.iter().enumerate() {
        let server: IpAddr = resolver.ip;
        let domain = domain.clone();
        let name = resolver.name.clone();
        let location = resolver.location.clone();
        let coords = resolver.coords;

        tasks.spawn(async move {
            let (result, elapsed, _ecs_honored) = dns::query(server, domain, rtype, None).await;
            let ping_ms = elapsed.as_millis() as u64;

            let (status, answer, ttl) = match result {
                QueryResult::Records { values, min_ttl } => {
                    ("ok", values, Some(min_ttl))
                }
                QueryResult::NoRecords(_) => ("nxdomain", vec![], None),
                QueryResult::ServFail => ("servfail", vec![], None),
                QueryResult::Error(_) => ("error", vec![], None),
            };

            let msg = json!({
                "index": index,
                "resolver": name,
                "ip": server.to_string(),
                "location": location,
                "status": status,
                "answer": answer,
                "ttl": ttl,
                "ping_ms": ping_ms,
                "lat": coords.map(|(lat, _)| lat),
                "lon": coords.map(|(_, lon)| lon),
                "total": 0,
            });
            (index, msg)
        });
    }

    // Stream results as they arrive.
    while let Some(result) = tasks.join_next().await {
        let Ok((_, msg)) = result else { continue };
        if socket.send(Message::Text(msg.to_string().into())).await.is_err() {
            return;
        }
    }

    // Signal completion.
    let done = json!({"done": true, "total": total});
    let _ = socket.send(Message::Text(done.to_string().into())).await;
}

#[tokio::main]
async fn main() {
    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8080);

    let app = Router::new()
        .route("/", get(index))
        .route("/ws", get(ws_handler));

    let addr = format!("0.0.0.0:{port}");
    println!("dnsglobe-web listening on http://{addr}");
    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
