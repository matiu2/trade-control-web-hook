use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tracing_subscriber::prelude::*;

async fn response_server(status: &str, body: &str, delay: Duration) -> String {
    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(tracing_error::ErrorLayer::default())
        .with(tracing_subscriber::fmt::layer())
        .try_init()
        .ok();
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mock");
    let address = listener.local_addr().expect("mock address");
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("one request");
        let mut request = Vec::new();
        let mut buffer = [0; 2048];
        while !request.ends_with(b"signed-payload") {
            let count = stream.read(&mut buffer).await.expect("read request");
            assert!(count > 0, "request must contain the signed payload");
            request.extend_from_slice(&buffer[..count]);
        }
        tokio::time::sleep(delay).await;
        stream.write_all(response.as_bytes()).await.expect("reply");
    });
    format!("http://{address}/")
}

#[tokio::test]
async fn waits_past_the_old_timeout_without_resending_the_entry() {
    let webhook = response_server("200 OK", "dry-run accepted", Duration::from_secs(21)).await;
    let response = post_intent_to(&webhook, "signed-payload".into())
        .await
        .expect("delayed broker validation must complete");
    assert_eq!(response, "dry-run accepted");
}

#[tokio::test]
async fn preserves_worker_rejection_details() {
    let webhook = response_server("400 Bad Request", "entry rejected", Duration::ZERO).await;
    let error = post_intent_to(&webhook, "signed-payload".into())
        .await
        .expect_err("worker rejection")
        .to_string();
    assert!(
        error.contains("HTTP 400") && error.contains("entry rejected"),
        "{error}"
    );
}
