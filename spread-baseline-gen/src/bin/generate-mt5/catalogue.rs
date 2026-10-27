//! Read the catalogue from the same account-pinned bridge as calibration data.
use color_eyre::{Result, eyre::Context};
use mt5_data_source::Mt5DataSource;

pub async fn read(source: &Mt5DataSource) -> Result<Vec<String>> {
    source
        .all_instruments()
        .await
        .wrap_err("MT5 calibration desktop catalogue")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use mt5_data_source::Mt5DataSource;
    use std::{path::PathBuf, time::Duration};
    use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

    async fn respond(directory: PathBuf, login: u64) {
        let request = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let mut entries = tokio::fs::read_dir(&directory).await.unwrap();
                while let Some(entry) = entries.next_entry().await.unwrap() {
                    if entry.path().extension().is_some_and(|ext| ext == "request") {
                        return entry.path();
                    }
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        let payload = tokio::fs::read_to_string(&request).await.unwrap();
        assert!(payload.contains("\n26790640\nFivePercentOnline-Real\ncatalogue\n"));
        let now = Utc::now().timestamp();
        let response = serde_json::json!({
            "version": "MT5_READ_ONLY_V1",
            "request_id": request.file_stem().unwrap().to_str().unwrap(),
            "status": "ok",
            "identity": {"login": login, "server": "FivePercentOnline-Real",
                "connected": true, "utc_time": now, "server_time": now + 10800},
            "symbols": ["EURUSD", "DOGUSD"]
        });
        let temporary = request.with_extension("response.tmp");
        tokio::fs::write(&temporary, serde_json::to_vec(&response).unwrap())
            .await
            .unwrap();
        tokio::fs::rename(temporary, request.with_extension("response"))
            .await
            .unwrap();
    }

    async fn catalogue(login: u64) -> color_eyre::Result<Vec<String>> {
        tracing_subscriber::registry()
            .with(tracing_subscriber::EnvFilter::from_default_env())
            .with(tracing_error::ErrorLayer::default())
            .with(tracing_subscriber::fmt::layer().with_test_writer())
            .try_init()
            .ok();
        let directory = tempfile::tempdir().unwrap();
        let source = Mt5DataSource::new(
            directory.path().into(),
            26790640,
            "FivePercentOnline-Real".into(),
            "+03:00".parse().unwrap(),
            Duration::from_secs(2),
        )?;
        let server = tokio::spawn(respond(directory.path().into(), login));
        let result = read(&source).await;
        server.await.unwrap();
        result
    }

    #[tokio::test]
    async fn real_catalogue_uses_desktop_without_a_web_login() {
        assert_eq!(catalogue(26790640).await.unwrap(), ["EURUSD", "DOGUSD"]);
    }

    #[tokio::test]
    async fn another_accounts_catalogue_is_rejected() {
        let error = catalogue(12590470).await.unwrap_err();
        assert!(format!("{error:#}").contains("account/server mismatch"));
    }
}
