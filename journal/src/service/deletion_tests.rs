use std::io::{Read, Write};

use super::*;
use trade_control_core::incoming::parse_and_verify;
use trade_control_core::intent::Action;

fn init_tracing() {
    use tracing_subscriber::prelude::*;
    tracing_subscriber::registry()
        .with(tracing_error::ErrorLayer::default())
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(tracing_subscriber::fmt::layer().with_test_writer())
        .try_init()
        .ok();
}

pub(super) fn mock_worker(
    status: &str,
    body: &str,
) -> (JournalClient, std::thread::JoinHandle<String>) {
    init_tracing();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
    let status = status.to_owned();
    let body = body.to_owned();
    let worker = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut bytes = Vec::new();
        let mut buffer = [0_u8; 8192];
        let offset = loop {
            let count = stream.read(&mut buffer).unwrap();
            assert!(count > 0);
            bytes.extend_from_slice(&buffer[..count]);
            if let Some(offset) = bytes.windows(4).position(|value| value == b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&bytes[..offset]).to_ascii_lowercase();
                let length = headers
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length:"))
                    .unwrap()
                    .trim()
                    .parse::<usize>()
                    .unwrap();
                if bytes.len() >= offset + 4 + length {
                    break offset + 4;
                }
            }
        };
        write!(
            stream,
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
        String::from_utf8(bytes[offset..].to_vec()).unwrap()
    });
    let client = JournalClient {
        endpoint,
        key: [7; KEY_LEN],
        client: Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap(),
    };
    (client, worker)
}

#[test]
fn delete_sends_verified_plan_delete_for_the_exact_selected_id() {
    let id = "trade-exact-selected-123";
    let expected = format!("plan-deleted: {id} (2)");
    let (client, worker) = mock_worker("200 OK", &expected);
    assert_eq!(client.delete(id).unwrap(), expected);
    let body = worker.join().unwrap();
    let verified = parse_and_verify(&body, &[7; KEY_LEN], Utc::now()).unwrap();
    assert_eq!(verified.intent.action, Action::PlanDelete);
    assert_eq!(verified.intent.trade_id.as_deref(), Some(id));
    assert!(!body.contains(&"07".repeat(KEY_LEN)));
}

#[test]
fn delete_accepts_a_valid_noop_and_preserves_worker_failure() {
    let (client, worker) = mock_worker("200 OK", "plan-deleted: selected (noop)");
    assert_eq!(
        client.delete("selected").unwrap(),
        "plan-deleted: selected (noop)"
    );
    worker.join().unwrap();
    let (client, worker) = mock_worker("403 Forbidden", "signature rejected");
    let error = client.delete("selected").unwrap_err().to_string();
    assert!(error.contains("403"));
    assert!(error.contains("signature rejected"));
    worker.join().unwrap();
}

#[test]
fn delete_rejects_malformed_or_wrong_id_success_bodies() {
    [
        "",
        "[]",
        "state error",
        "plan-deleted: other (1)",
        "plan-deleted: selected (0)",
        "plan-deleted: selected (bogus)",
        "plan-deleted: selected (+1)",
        "plan-deleted: selected (1)\nextra",
        "plan-deleted: selected (1) trailing",
    ]
    .into_iter()
    .for_each(|body| {
        let (client, worker) = mock_worker("200 OK", body);
        assert!(client.delete("selected").is_err(), "{body}");
        worker.join().unwrap();
    });
}

#[test]
fn delete_rejects_empty_trade_id_before_sending_a_request() {
    init_tracing();
    let client = JournalClient {
        endpoint: Url::parse("http://127.0.0.1:1").unwrap(),
        key: [7; KEY_LEN],
        client: Client::builder().build().unwrap(),
    };
    assert!(
        client
            .delete(" \t")
            .unwrap_err()
            .to_string()
            .contains("must not be empty")
    );
}
