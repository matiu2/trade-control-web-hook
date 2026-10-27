use super::*;
use std::io::{Read, Write};
use std::net::TcpListener;

use clap::Parser;

use crate::api::{ArmContext, Execution};
use crate::args::BrokerArg;

#[test]
fn oanda_spread_uses_the_gui_worker_and_selected_live_account() {
    check_worker_spread(
        Broker::Oanda,
        BrokerArg::Oanda,
        "oanda-live",
        "EUR_USD",
        "EUR_USD",
    );
}

#[test]
fn tradenation_spread_uses_the_selected_live_or_practice_account_on_the_worker() {
    for account in ["tradenation-live", "reversals"] {
        check_worker_spread(
            Broker::TradeNation,
            BrokerArg::TradeNation,
            account,
            "EUR/USD",
            "EUR%2FUSD",
        );
    }
}

fn check_worker_spread(
    broker: Broker,
    argument: BrokerArg,
    account: &'static str,
    instrument: &'static str,
    encoded: &'static str,
) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let worker_url = format!("http://{}", listener.local_addr().unwrap());
    let server = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        let mut request = [0; 4096];
        let count = socket.read(&mut request).unwrap();
        let request = String::from_utf8(request[..count].to_vec()).unwrap();
        assert!(request.starts_with(&format!(
            "GET /accounts/{account}/quote?instrument={encoded} HTTP/1.1"
        )));
        assert!(request.contains(&format!("x-diag-key: {}", "ab".repeat(32))));
        let body =
            serde_json::json!({"account":account,"instrument":instrument,"bid":1.1,"ask":1.1002})
                .to_string();
        write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
    });
    let directory = tempfile::tempdir().unwrap();
    let key_file = directory.path().join("key.hex");
    std::fs::write(&key_file, "ab".repeat(32)).unwrap();
    let mut args = Args::try_parse_from(["tv-arm"]).unwrap();
    args.execution = Some(Execution::new(ArmContext {
        worker_url,
        key_file,
        broker: argument,
        account_id: None,
    }));
    let spread = read_spread_blocking(&args, broker, instrument, 0.0001, account).unwrap();
    assert!((spread - 2.0).abs() < 1e-8);
    server.join().unwrap();
}
