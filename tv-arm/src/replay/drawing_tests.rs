use super::draw_positions_on_local_chart;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tracing_subscriber::prelude::*;

async fn read_request(stream: &mut TcpStream) -> String {
    let mut bytes = Vec::new();
    let mut buffer = [0; 4096];
    loop {
        let count = stream.read(&mut buffer).await.expect("read request");
        assert!(count > 0, "request must finish before disconnecting");
        bytes.extend_from_slice(&buffer[..count]);
        let request = String::from_utf8(bytes.clone()).expect("UTF-8 request");
        if let Some((headers, body)) = request.split_once("\r\n\r\n") {
            let length = headers
                .lines()
                .filter_map(|line| line.split_once(':'))
                .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                .map(|(_, value)| value.trim().parse::<usize>().expect("body length"))
                .unwrap_or(0);
            if body.len() >= length {
                return request;
            }
        }
    }
}

async fn draw_fixture(positions: Value) -> Vec<String> {
    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(tracing_error::ErrorLayer::default())
        .with(tracing_subscriber::fmt::layer())
        .try_init()
        .ok();
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind chart");
    let url = format!("http://{}", listener.local_addr().expect("chart address"));
    let (sender, mut receiver) = mpsc::unbounded_channel();
    let server = tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.expect("chart request");
            let request = read_request(&mut stream).await;
            let body = if request.starts_with("GET ") {
                r#"[{"id":"replay-pos-short-10-gate-blocked"},{"id":"operator-neckline"}]"#
            } else {
                "{}"
            };
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            sender.send(request).expect("record chart request");
            stream.write_all(response.as_bytes()).await.expect("reply");
        }
    });
    let dir = tempfile::tempdir().expect("positions directory");
    let path = dir.path().join("positions.json");
    std::fs::write(
        &path,
        json!({
            "version": 1, "instrument": "EUR_USD", "granularity": "h1",
            "broker": "oanda", "positions": positions
        })
        .to_string(),
    )
    .expect("write positions");
    let consumed_path = path.clone();
    tokio::task::spawn_blocking(move || draw_positions_on_local_chart(&url, &consumed_path))
        .await
        .expect("drawing task")
        .expect("draw replay");
    server.abort();
    assert!(!path.exists(), "consumed positions file is removed");
    let mut requests = Vec::new();
    while let Ok(request) = receiver.try_recv() {
        requests.push(request);
    }
    assert!(
        requests
            .iter()
            .any(|r| r.starts_with("DELETE /drawings/one/replay-pos-short-10-gate-blocked?")),
        "a rerun removes the old rejected bracket"
    );
    assert!(
        !requests
            .iter()
            .any(|r| r.starts_with("DELETE /drawings/one/operator-neckline")),
        "operator drawings survive"
    );
    requests
        .into_iter()
        .filter(|r| r.starts_with("POST "))
        .collect()
}

fn position(taken: bool, outcome: &str, fill_at: i64, until: i64) -> Value {
    json!({
        "direction": "short", "fill_at": fill_at, "until": until,
        "entry_price": 1.2, "stop_loss": 1.21, "take_profit": 1.18,
        "outcome": outcome, "taken": taken
    })
}

#[tokio::test]
async fn draws_only_taken_positions_including_a_later_entry_after_exit() {
    let requests = draw_fixture(json!([
        position(true, "tp", 0, 20),
        position(false, "gate-blocked", 10, 60),
        position(false, "no-fill", 21, 60),
        position(false, "declined", 22, 60),
        position(true, "open", 30, 60)
    ]))
    .await;
    let ids: Vec<Value> = requests
        .iter()
        .map(|request| {
            let (_, body) = request.split_once("\r\n\r\n").expect("drawing body");
            serde_json::from_str::<Value>(body).expect("drawing JSON")["id"].clone()
        })
        .collect();
    assert_eq!(
        ids,
        vec![
            json!("replay-pos-short-0-tp"),
            json!("replay-pos-short-30-open")
        ]
    );
}

#[tokio::test]
async fn a_replay_without_fills_clears_old_brackets_and_draws_nothing() {
    let requests = draw_fixture(json!([
        position(false, "gate-blocked", 10, 60),
        position(false, "no-fill", 21, 60),
        position(false, "declined", 22, 60)
    ]))
    .await;
    assert!(requests.is_empty(), "no position was taken");
}

#[tokio::test]
async fn closed_positions_end_at_their_exit_candle_and_open_positions_at_window_end() {
    let cases = [
        ("sl", 0, 0),
        ("sl", 0, 10),
        ("tp", 20, 30),
        ("reversal", 40, 50),
        ("expiry", 60, 70),
        ("invalidation", 80, 90),
        ("open", 100, 150),
    ];
    let positions: Vec<Value> = cases
        .iter()
        .map(|(outcome, fill, exit)| position(true, outcome, *fill, *exit))
        .collect();
    let requests = draw_fixture(json!(positions)).await;
    assert_eq!(requests.len(), cases.len());
    for (request, (outcome, fill, exit)) in requests.iter().zip(cases) {
        let (_, body) = request.split_once("\r\n\r\n").expect("drawing body");
        let drawing: Value = serde_json::from_str(body).expect("drawing JSON");
        assert_eq!(
            drawing["options"]["widthSeconds"],
            json!(exit - fill),
            "{outcome} visible duration"
        );
        assert_eq!(
            drawing["anchors"][0]["time"],
            json!(fill),
            "{outcome} fill candle"
        );
        assert_eq!(
            drawing["anchors"][1]["time"],
            json!(exit),
            "{outcome} stop edge"
        );
        assert_eq!(
            drawing["anchors"][2]["time"],
            json!(exit),
            "{outcome} target edge"
        );
    }
}
