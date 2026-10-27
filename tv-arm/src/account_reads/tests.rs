use super::*;
use std::io::{Read, Write};
use std::net::TcpListener;

fn mock(body: &'static str) -> (String, std::thread::JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let thread = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut request = [0; 4096];
        let length = socket.read(&mut request).unwrap();
        write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        String::from_utf8(request[..length].to_vec()).unwrap()
    });
    (url, thread)
}

#[tokio::test]
async fn lists_demo_and_live_metadata_from_the_configured_worker() {
    let (url, thread) = mock(
        r#"[{"name":"oanda-live","broker":"oanda","kind":"live","oanda_account_id":"001-test"}]"#,
    );
    let accounts = AccountReads::new(&url, &[0xab; 32])
        .unwrap()
        .list()
        .await
        .unwrap();
    assert_eq!(accounts[0].name, "oanda-live");
    assert!(accounts[0].kind.is_live());
    let request = thread.join().unwrap();
    assert!(request.starts_with("GET /accounts HTTP/1.1"));
    assert!(request.contains(&format!("x-diag-key: {}", "ab".repeat(32))));
}

#[tokio::test]
async fn quote_uses_the_exact_named_account_and_rejects_a_mismatched_reply() {
    let body = r#"{"account":"oanda-live","instrument":"EUR_USD","bid":1.1,"ask":1.1002}"#;
    let (url, thread) = mock(body);
    let quote = AccountReads::new(&url, &[0xab; 32])
        .unwrap()
        .quote("oanda-live", "EUR_USD")
        .await
        .unwrap();
    assert_eq!(quote, (1.1, 1.1002));
    assert!(
        thread
            .join()
            .unwrap()
            .starts_with("GET /accounts/oanda-live/quote?instrument=EUR_USD HTTP/1.1")
    );
    let (url, thread) = mock(body);
    assert!(
        AccountReads::new(&url, &[0xab; 32])
            .unwrap()
            .quote("different-account", "EUR_USD")
            .await
            .is_err()
    );
    thread.join().unwrap();
}

#[test]
fn refuses_non_http_worker_addresses() {
    assert!(AccountReads::new("file:///tmp/secrets", &[0; 32]).is_err());
}
