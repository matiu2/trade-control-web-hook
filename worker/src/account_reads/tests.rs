use super::*;
use axum::body::Body;
use axum::http::Request;
use tower::ServiceExt;

fn test_router() -> Router {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .connect_lazy("postgresql://unused:unused@127.0.0.1:1/unused")
        .unwrap();
    router(
        PgMetadataStore::from_pool(pool),
        Secrets {
            signing_key: "ab".repeat(32),
            admin_key: "unused".into(),
            max_risk_pct: 1.0,
            max_open_positions: 3.0,
            oanda_api_key: None,
            oanda_live: false,
        },
    )
}

#[tokio::test]
async fn account_reads_require_authentication_before_accessing_the_database() {
    for path in ["/accounts", "/accounts/oanda-live/quote?instrument=EUR_USD"] {
        for key in [None, Some("wrong")] {
            let mut request = Request::builder().uri(path);
            if let Some(key) = key {
                request = request.header("x-diag-key", key);
            }
            let response = test_router()
                .oneshot(request.body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        }
    }
}

#[test]
fn quote_selection_requires_the_named_oanda_account() {
    use trade_control_core::account::{AccountKind, AccountMetadata};
    let mut meta = AccountMetadata::new("oanda-live", BrokerKind::Oanda, AccountKind::Live);
    meta.oanda_account_id = Some("001-011-test-001".into());
    assert!(validate_quote_account(&meta).is_ok());
    meta.broker = BrokerKind::TradeNation;
    meta.oanda_account_id = None;
    assert!(validate_quote_account(&meta).is_ok());
    meta.broker = BrokerKind::Oanda;
    meta.oanda_account_id = None;
    assert!(validate_quote_account(&meta).is_err());
}

#[test]
fn tradenation_quotes_accept_both_registered_account_kinds() {
    use trade_control_core::account::{AccountKind, AccountMetadata};
    for (name, kind) in [
        ("reversals", AccountKind::Demo),
        ("tradenation-live", AccountKind::Live),
    ] {
        let meta = AccountMetadata::new(name, BrokerKind::TradeNation, kind);
        assert!(validate_quote_account(&meta).is_ok());
    }
    let unsupported = AccountMetadata::new("other", BrokerKind::Ibkr, AccountKind::Live);
    assert!(validate_quote_account(&unsupported).is_err());
}
