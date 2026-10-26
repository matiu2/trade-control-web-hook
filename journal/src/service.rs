//! Signed worker journal access with complete audit evidence preserved.
//!
//! Construct and call this blocking client outside an async runtime worker
//! (for example, inside `tokio::task::spawn_blocking`). Destination and key
//! path must come from trusted server configuration, never browser input.

#[cfg(test)]
mod deletion_tests;
mod evidence;
#[cfg(test)]
mod tests;

use std::path::Path;
use std::time::Duration;

use chrono::Utc;
use color_eyre::eyre::{Result, WrapErr, eyre};
use reqwest::Url;
use reqwest::blocking::Client;
use trade_control_cli::{
    build_plan_delete_intent, build_plan_list_intent, build_plan_show_intent,
    build_plan_timeline_intent, wrap_signed,
};
use trade_control_core::intent::Intent;
use trade_control_core::sig::{KEY_LEN, parse_key_hex};

use crate::plan::{PlanRow, parse_plan_list};
pub use evidence::{RecordedTimeline, ReplayComparison, StoredPlan, compare_timeline_with_replay};

/// A native worker journal client. Private signing material is never serialized or
/// included in `Debug` output; clones reuse the bounded HTTP connection pool.
#[derive(Clone)]
pub struct JournalClient {
    endpoint: Url,
    key: [u8; KEY_LEN],
    client: Client,
}

impl JournalClient {
    pub fn new(worker: &str, key_path: impl AsRef<Path>) -> Result<Self> {
        let endpoint = worker_url(worker)?;
        let key_text = std::fs::read_to_string(key_path.as_ref())
            .wrap_err("read journal worker signing key file")?;
        let key = parse_key_hex(&key_text).wrap_err("parse journal worker signing key")?;
        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .wrap_err("build journal worker HTTP client")?;
        Ok(Self {
            endpoint,
            key,
            client,
        })
    }

    /// Include archives to browse completed trades as well as active plans.
    pub fn list(&self, include_archived: bool) -> Result<Vec<PlanRow>> {
        let intent = build_plan_list_intent(Utc::now(), &request_suffix()?, include_archived);
        parse_list_response(&self.post(&intent)?)
    }

    /// Read one exact trade. An ambiguous account-scoped match is an error.
    pub fn show(&self, trade_id: &str) -> Result<StoredPlan> {
        validate_trade_id(trade_id)?;
        let intent = build_plan_show_intent(trade_id, Utc::now(), &request_suffix()?);
        let raw = serde_yaml::from_str(&self.post(&intent)?)
            .wrap_err("parse journal plan-show worker response")?;
        StoredPlan::from_value(trade_id, raw)
    }

    /// Read all available inbound requests, engine ticks, and cron notes.
    pub fn timeline(&self, trade_id: &str) -> Result<RecordedTimeline> {
        validate_trade_id(trade_id)?;
        let intent = build_plan_timeline_intent(trade_id, Utc::now(), &request_suffix()?);
        let raw = serde_yaml::from_str(&self.post(&intent)?)
            .wrap_err("parse journal timeline worker response")?;
        RecordedTimeline::from_value(raw)
    }

    /// Delete matching live and archived plans and state across account scopes.
    /// The worker does not cancel broker orders or close positions.
    pub fn delete(&self, trade_id: &str) -> Result<String> {
        validate_trade_id(trade_id)?;
        let intent = build_plan_delete_intent(trade_id, Utc::now(), &request_suffix()?);
        parse_delete_response(trade_id, self.post(&intent)?)
    }

    fn post(&self, intent: &Intent) -> Result<String> {
        let body =
            wrap_signed(intent, &self.key, Utc::now()).wrap_err("sign journal worker request")?;
        tracing::debug!(action = ?intent.action, "sending journal worker request");
        let response = self
            .client
            .post(self.endpoint.clone())
            .header(reqwest::header::CONTENT_TYPE, "text/plain")
            .body(body)
            .send()
            .wrap_err("send journal worker request")?;
        let status = response.status();
        let text = response.text().wrap_err("read journal worker response")?;
        if !status.is_success() {
            return Err(eyre!("journal worker returned {status}: {text}"));
        }
        Ok(text)
    }
}

fn parse_delete_response(trade_id: &str, body: String) -> Result<String> {
    let prefix = format!("plan-deleted: {trade_id} (");
    let count = body
        .strip_prefix(&prefix)
        .and_then(|text| text.strip_suffix(')'));
    if count.is_some_and(|count| {
        count == "noop"
            || (count.bytes().all(|byte| byte.is_ascii_digit())
                && !count.starts_with('0')
                && count.parse::<usize>().is_ok_and(|count| count > 0))
    }) {
        return Ok(body);
    }
    Err(eyre!(
        "journal worker returned an invalid plan-delete response: {body}"
    ))
}

fn parse_list_response(body: &str) -> Result<Vec<PlanRow>> {
    if body.trim().is_empty() {
        return Err(eyre!("journal worker returned an empty plan-list body"));
    }
    parse_plan_list(body)
}

fn worker_url(raw: &str) -> Result<Url> {
    let url = Url::parse(raw).wrap_err("parse explicit journal worker URL")?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(eyre!("journal worker URL must be HTTP or HTTPS"));
    }
    if !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() {
        return Err(eyre!(
            "journal worker URL must not contain credentials or a fragment"
        ));
    }
    Ok(url)
}

fn validate_trade_id(trade_id: &str) -> Result<()> {
    if trade_id.trim().is_empty() {
        return Err(eyre!("journal trade_id must not be empty"));
    }
    Ok(())
}

fn request_suffix() -> Result<String> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).map_err(|error| eyre!("journal request nonce: {error}"))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}
