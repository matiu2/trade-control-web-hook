//! Exact-ID diagnostics and closure; a probe must never close another position.
use super::*;
use std::time::Duration;
use tokio::time::{sleep, timeout};

mod proof;

impl CtraderBroker {
    /// Native account snapshot for read-only diagnostics on the verified demo companion.
    pub async fn diagnostic_snapshot(&self) -> Result<Value> {
        self.hybrid_backend()?.data.snapshot().await
    }

    /// Fresh Open API quote, without creating a competing FIX quote session.
    pub async fn diagnostic_quote(&self, instrument: &str) -> Result<Quote> {
        metadata::quote(&self.hybrid_backend()?.data, instrument).await
    }

    /// Full native contract metadata used to prepare an integration probe.
    pub async fn diagnostic_instrument(&self, instrument: &str) -> Result<Value> {
        Ok(
            metadata::instrument(&self.hybrid_backend()?.data, instrument)
                .await?
                .raw,
        )
    }

    /// Native order/deal evidence, preserving the broker's response fields.
    pub async fn diagnostic_order(&self, order_id: &str) -> Result<Value> {
        let id = proof::positive_id(order_id)?;
        let broker = &self.hybrid_backend()?.data;
        let detail = broker.read(2181, 2182, json!({"orderId":id})).await?;
        ensure!(
            wire::integer(&detail["order"], "orderId")? == id,
            "diagnostic order ID mismatch"
        );
        Ok(detail)
    }

    /// Close only the full position proven to originate from this opening order.
    /// No close request is replayed; unresolved execution retains the account interlock.
    pub async fn close_exact_position(
        &self,
        order_id: &str,
        position_id: &str,
        expected_client_order_id: &str,
    ) -> Result<Value> {
        let opening = proof::positive_id(order_id)?;
        let position = proof::positive_id(position_id)?;
        let hybrid = self.hybrid_backend()?;
        let mut guard = hybrid
            .management
            .companion_write(
                "open-api-close-exact-position",
                json!({"opening_order_id":opening,"position_id":position,"opening_client_order_id":expected_client_order_id}),
            )
            .await?;
        let mut sent = false;
        let result = timeout(
            Duration::from_secs(20),
            close_once(
                &hybrid.data,
                opening,
                position,
                expected_client_order_id,
                &mut sent,
                |phase, value| guard.note(phase, value),
            ),
        )
        .await;
        let result = match result {
            Ok(result) => result,
            Err(_) => Err(eyre!(
                "exact position close verification timed out; reconcile before retry"
            )),
        };
        let settled = result.is_ok() || !sent;
        guard.record(
            &json!({"opening_order_id":opening,"position_id":position,
            "confirmed_closed":result.is_ok(),"evidence":result.as_ref().ok(),
            "error":result.as_ref().err().map(|error|format!("{error:#}"))}),
            settled,
        )?;
        let mut evidence = result?;
        let instrument = evidence["instrument"]
            .as_str()
            .ok_or_else(|| eyre!("close proof has no instrument"))?;
        // A reporting lookup failure cannot turn positively proved closure into unknown execution.
        match timeout(
            Duration::from_secs(15),
            hybrid
                .data
                .lookup_trade_exit(instrument, order_id, Some(position_id)),
        )
        .await
        {
            Ok(Ok(exit)) => evidence["history"] = serde_json::to_value(exit)?,
            Ok(Err(error)) => evidence["history_error"] = json!(format!("{error:?}")),
            Err(_) => evidence["history_error"] = json!("closed-trade reporting lookup timed out"),
        }
        Ok(evidence)
    }
}

async fn close_once(
    broker: &OpenApiBroker,
    opening: i64,
    position: i64,
    client: &str,
    sent: &mut bool,
    mut note: impl FnMut(&str, &Value) -> Result<()>,
) -> Result<Value> {
    let detail = broker.read(2181, 2182, json!({"orderId":opening})).await?;
    let before = broker.snapshot().await?;
    note(
        "exact-close-before",
        &json!({"opening_order":detail,"snapshot":before}),
    )?;
    let target = proof::opening(
        &detail,
        &before,
        broker.account_id(),
        opening,
        position,
        client,
    )?;
    let symbols = metadata::symbols(broker).await?;
    let instrument = management::symbol_name(&symbols, target.symbol)?;
    *sent = true;
    let response = match broker
        .mutate(2111, json!({"positionId":position,"volume":target.volume}))
        .await
    {
        Ok(response) => response,
        Err(
            error @ (ctrader_api::RequestError::NotSent(_)
            | ctrader_api::RequestError::Rejected { .. }),
        ) => {
            *sent = false;
            return Err(error.into());
        }
        Err(error) => return Err(error.into()),
    };
    note("exact-close-response", &response)?;
    proof::account(&response, broker.account_id())?;
    let closing = wire::integer(&response["order"], "orderId")?;
    ensure!(
        closing > 0 && closing != opening,
        "close response has invalid closing order ID"
    );
    let close_detail = loop {
        let detail = broker.read(2181, 2182, json!({"orderId":closing})).await?;
        note("exact-close-order", &detail)?;
        if proof::closed(&detail, broker.account_id(), closing, position, &target)? {
            break detail;
        }
        sleep(Duration::from_millis(200)).await;
    };
    let after = broker.snapshot().await?;
    note("exact-close-after", &after)?;
    proof::absent(&after, broker.account_id(), position)?;
    Ok(
        json!({"instrument":instrument,"opening_order_id":opening,"position_id":position,
        "opening_order":detail,"before":before,"close_response":response,"closing_order":close_detail,
        "after":after,"confirmed_closed":true}),
    )
}
