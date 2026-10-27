//! Shared native live account discovery and private credential loading.
use color_eyre::{
    Result,
    eyre::{Context, ensure, eyre},
};
use oanda_client::OandaClient;
use std::{
    path::{Path, PathBuf},
    time::Duration,
};
use trade_control_core::{
    account::{AccountKind, AccountMetadata},
    intent::BrokerKind,
    sig::parse_key_hex,
};

pub struct Profile {
    pub account_id: String,
    pub client: std::sync::Arc<OandaClient>,
}

pub fn destination() -> Result<(String, PathBuf)> {
    let url = std::env::var("LOCAL_CHART_TRADE_WORKER_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:8788".into());
    let path = std::env::var_os("LOCAL_CHART_TRADE_KEY_FILE")
        .map(PathBuf::from)
        .unwrap_or(config_dir()?.join("trade-control/key.hex"));
    Ok((url, path))
}

pub async fn load(worker_url: &str, key_file: &Path) -> Result<Profile> {
    let mut url = reqwest::Url::parse(worker_url)?;
    ensure!(
        matches!(url.scheme(), "http" | "https") && url.host_str().is_some(),
        "Invalid OANDA account discovery URL"
    );
    url.path_segments_mut()
        .map_err(|_| eyre!("Invalid worker URL"))?
        .pop_if_empty()
        .push("accounts");
    let key = tokio::fs::read_to_string(key_file).await?;
    parse_key_hex(&key)?;
    let mut header = reqwest::header::HeaderValue::from_str(key.trim())?;
    header.set_sensitive(true);
    let reader = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(3))
        .timeout(Duration::from_secs(30))
        .build()?;
    let response = reader
        .get(url)
        .header("X-Diag-Key", header)
        .send()
        .await?
        .error_for_status()?;
    let accounts: Vec<AccountMetadata> = serde_json::from_str(&response.text().await?)?;
    let account = accounts
        .into_iter()
        .find(|row| row.name == instrument_lookup::oanda_live::ACCOUNT)
        .ok_or_else(|| eyre!("Register oanda-live on the configured trade worker"))?;
    ensure!(
        account.kind == AccountKind::Live && account.broker == BrokerKind::Oanda,
        "oanda-live must identify a live OANDA account"
    );
    let account_id = account
        .oanda_account_id
        .ok_or_else(|| eyre!("oanda-live needs its broker account ID"))?;
    let token = match std::env::var("OANDA_LIVE_TOKEN") {
        Ok(token) if !token.trim().is_empty() => token,
        _ => {
            let path = std::env::var_os("LOCAL_CHART_OANDA_LIVE_ENV_FILE")
                .map(PathBuf::from)
                .unwrap_or(config_dir()?.join("trade-control/worker-secrets.env"));
            let text = tokio::fs::read_to_string(path)
                .await
                .wrap_err("Read OANDA live credential file")?;
            token_from_env(&text).ok_or_else(|| eyre!("OANDA live token is missing"))?
        }
    };
    Ok(Profile {
        client: std::sync::Arc::new(OandaClient::new_live(token).with_candle_account(&account_id)),
        account_id,
    })
}

fn config_dir() -> Result<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .ok_or_else(|| eyre!("No config directory"))
}

fn token_from_env(text: &str) -> Option<String> {
    text.lines()
        .filter_map(|line| {
            let (key, value) = line
                .trim()
                .strip_prefix("export ")
                .unwrap_or(line.trim())
                .split_once('=')?;
            matches!(key.trim(), "OANDA_TOKEN" | "OANDA_API_KEY")
                .then(|| value.trim().trim_matches(['\'', '"']).to_string())
        })
        .find(|token| !token.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn logging() {
        use tracing_subscriber::prelude::*;
        tracing_subscriber::registry()
            .with(tracing_error::ErrorLayer::default())
            .with(tracing_subscriber::EnvFilter::from_default_env())
            .with(tracing_subscriber::fmt::layer().with_test_writer())
            .try_init()
            .ok();
    }
    #[tokio::test]
    async fn discovery_authenticates_and_rejects_a_demo_identity_before_loading_credentials() {
        logging();
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let key = "ab".repeat(32);
        let expected = key.clone();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut bytes = [0; 4096];
            let len = socket.read(&mut bytes).unwrap();
            let request = String::from_utf8_lossy(&bytes[..len]).to_ascii_lowercase();
            assert!(request.starts_with("get /accounts http/1.1"));
            assert!(request.contains(&format!("x-diag-key: {expected}")));
            let body = r#"[{"name":"oanda-live","kind":"demo","broker":"oanda","oanda_account_id":"001-test"}]"#;
            write!(
                socket,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        });
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("key.hex");
        std::fs::write(&path, key).unwrap();
        let error = load(&format!("http://{address}"), &path)
            .await
            .err()
            .unwrap();
        assert!(error.to_string().contains("live OANDA account"));
        server.join().unwrap();
    }

    #[test]
    fn live_token_file_never_reads_the_ambient_demo_token() {
        logging();
        assert_eq!(
            token_from_env("OTHER=unrelated\nexport OANDA_TOKEN='test-value'"),
            Some("test-value".into())
        );
        assert_eq!(token_from_env("OANDA_TOKEN=\nOTHER=unrelated"), None);
    }
}
