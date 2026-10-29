//! Authenticated, read-only account discovery and account-scoped broker quotes.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;
use subtle::ConstantTimeEq;
use trade_control_core::account::{AccountMetadata, MetadataError, MetadataStore};
use trade_control_core::broker::Broker;
use trade_control_core::intent::BrokerKind;
use trade_control_core::sig::parse_key_hex;

use crate::{PgMetadataStore, Secrets, acquire_oanda, acquire_tn};

struct Reads {
    accounts: PgMetadataStore,
    secrets: Secrets,
}

pub fn router(accounts: PgMetadataStore, secrets: Secrets) -> Router {
    Router::new()
        .route("/accounts", get(list))
        .route("/accounts/{name}/quote", get(quote))
        .with_state(Arc::new(Reads { accounts, secrets }))
}

fn authorized(headers: &HeaderMap, reads: &Reads) -> bool {
    let supplied = headers
        .get("x-diag-key")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| parse_key_hex(value).ok());
    let expected = parse_key_hex(&reads.secrets.signing_key).ok();
    match (supplied, expected) {
        (Some(supplied), Some(expected)) => bool::from(supplied.ct_eq(&expected)),
        _ => false,
    }
}

async fn list(State(reads): State<Arc<Reads>>, headers: HeaderMap) -> Response {
    if !authorized(&headers, &reads) {
        return failure(
            StatusCode::UNAUTHORIZED,
            "Account reads require the worker signing key",
        );
    }
    match reads.accounts.list().await {
        Ok(accounts) => Json(accounts).into_response(),
        Err(error) => metadata_failure(error),
    }
}

#[derive(Deserialize)]
struct QuoteQuery {
    instrument: String,
}

async fn quote(
    State(reads): State<Arc<Reads>>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Query(query): Query<QuoteQuery>,
) -> Response {
    if !authorized(&headers, &reads) {
        return failure(
            StatusCode::UNAUTHORIZED,
            "Account reads require the worker signing key",
        );
    }
    let meta = match reads.accounts.get(&name).await {
        Ok(meta) => meta,
        Err(error) => return metadata_failure(error),
    };
    if let Err(error) = validate_quote_account(&meta) {
        return failure(StatusCode::BAD_REQUEST, error);
    }
    let result = match meta.broker {
        BrokerKind::Oanda => match acquire_oanda(&meta, &reads.secrets) {
            Ok(broker) => broker.get_quote(&query.instrument).await,
            Err(error) => {
                tracing::warn!(account = name, %error, "Account broker unavailable");
                return failure(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Selected account broker unavailable",
                );
            }
        },
        BrokerKind::TradeNation => match acquire_tn(&meta).await {
            Ok(broker) => broker.get_quote(&query.instrument).await,
            Err(error) => {
                tracing::warn!(account = name, %error, "Account broker unavailable");
                return failure(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Selected account broker unavailable",
                );
            }
        },
        _ => return failure(StatusCode::BAD_REQUEST, "Unsupported quote account broker"),
    };
    match result {
        Ok(quote) => Json(json!({"account":meta.name, "instrument":query.instrument,
            "kind":meta.kind, "bid":quote.bid, "ask":quote.ask,
            "source": if meta.broker == BrokerKind::TradeNation { "chart-minute" } else { "broker-quote" }}))
        .into_response(),
        Err(error) => {
            tracing::warn!(account = name, ?error, "Account quote read failed");
            failure(
                StatusCode::BAD_GATEWAY,
                "Quote unavailable for the selected account and instrument",
            )
        }
    }
}

fn validate_quote_account(meta: &AccountMetadata) -> Result<(), &'static str> {
    if meta.broker == BrokerKind::TradeNation {
        return Ok(());
    }
    if meta.broker != BrokerKind::Oanda {
        return Err("This quote endpoint supports OANDA and TradeNation accounts only");
    }
    if meta.oanda_account_id.as_deref().is_none_or(str::is_empty) {
        return Err("The selected OANDA account has no broker account ID");
    }
    Ok(())
}

fn metadata_failure(error: MetadataError) -> Response {
    match error {
        MetadataError::NotFound(_) => failure(StatusCode::NOT_FOUND, "Account not found"),
        error => {
            tracing::warn!(%error, "Account registry read failed");
            failure(
                StatusCode::SERVICE_UNAVAILABLE,
                "Account registry unavailable",
            )
        }
    }
}

fn failure(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({"error":message}))).into_response()
}

#[cfg(test)]
mod tests;
