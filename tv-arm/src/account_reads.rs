//! Read account metadata and OANDA quotes using the worker's own credentials.

use std::time::Duration;

use color_eyre::eyre::{Result, WrapErr, eyre};
use reqwest::header::{HeaderMap, HeaderValue};
use serde::Deserialize;
use trade_control_core::account::AccountMetadata;
use trade_control_core::sig::KEY_LEN;

#[derive(Clone)]
pub struct AccountReads {
    endpoint: reqwest::Url,
    client: reqwest::Client,
}

impl AccountReads {
    pub fn new(worker: &str, key: &[u8; KEY_LEN]) -> Result<Self> {
        let endpoint = reqwest::Url::parse(worker)?;
        if !matches!(endpoint.scheme(), "http" | "https") || endpoint.host_str().is_none() {
            return Err(eyre!("Account reads require an HTTP(S) worker URL"));
        }
        let mut authorization = HeaderValue::from_str(&hex::encode(key))?;
        authorization.set_sensitive(true);
        let mut headers = HeaderMap::new();
        headers.insert("x-diag-key", authorization);
        let client = reqwest::Client::builder()
            .default_headers(headers)
            .timeout(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Self { endpoint, client })
    }

    pub async fn list(&self) -> Result<Vec<AccountMetadata>> {
        self.read(self.url(&["accounts"])?).await
    }

    pub async fn quote(&self, account: &str, instrument: &str) -> Result<(f64, f64)> {
        let mut url = self.url(&["accounts", account, "quote"])?;
        url.query_pairs_mut().append_pair("instrument", instrument);
        let quote: AccountQuote = self.read(url).await?;
        if quote.account != account || quote.instrument != instrument {
            return Err(eyre!(
                "Worker returned a quote for a different account or instrument"
            ));
        }
        Ok((quote.bid, quote.ask))
    }

    fn url(&self, segments: &[&str]) -> Result<reqwest::Url> {
        let mut url = self.endpoint.clone();
        url.set_query(None);
        url.set_fragment(None);
        url.path_segments_mut()
            .map_err(|()| eyre!("Invalid worker URL"))?
            .pop_if_empty()
            .extend(segments);
        Ok(url)
    }

    async fn read<T: serde::de::DeserializeOwned>(&self, url: reqwest::Url) -> Result<T> {
        let response = self
            .client
            .get(url)
            .send()
            .await
            .wrap_err("Read worker accounts")?;
        let status = response.status();
        if !status.is_success() {
            return Err(eyre!(
                "Worker account read returned {status}: {}",
                response.text().await?
            ));
        }
        response
            .json()
            .await
            .wrap_err("Decode worker account response")
    }
}

#[derive(Deserialize)]
struct AccountQuote {
    account: String,
    instrument: String,
    bid: f64,
    ask: f64,
}

#[cfg(test)]
mod tests;
