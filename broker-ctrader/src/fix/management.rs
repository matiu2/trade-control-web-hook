use super::evidence::{AccountLock, Mutation};
use super::*;
use ctrader_fix::trading::{CancelOrder, MutationOutcome, OrderStatus};
use serde_json::json;

impl FixBroker {
    pub(super) async fn live_quote(&self, instrument: &str) -> Result<Quote> {
        let _guard = self.gate.lock().await;
        let mut client = ctrader_fix::market_data::Client::connect(self.quote.clone()).await?;
        let result = async {
            let symbols = client.symbols().await?;
            client
                .subscribe(mapping::resolve(&symbols, instrument)?)
                .await?;
            let quote = client.next_quote().await?;
            Ok(Quote {
                bid: quote.bid,
                ask: quote.ask,
            })
        }
        .await;
        let logout = client.logout().await;
        result.and_then(|quote| logout.map(|()| quote))
    }
    pub(super) async fn positions(&self, account: &str) -> Result<Vec<OpenPosition>> {
        self.account_matches(account)?;
        let _guard = self.gate.lock().await;
        let symbols = self.symbols_unlocked().await?;
        let mut client = Client::connect(self.trade.clone()).await?;
        let result = async {
            client
                .positions()
                .await?
                .iter()
                .map(|p| mapping::position(&symbols, p))
                .collect()
        }
        .await;
        self.finish_read(&mut client, result).await
    }
    pub(super) async fn pending(&self, account: &str) -> Result<Vec<PendingOrder>> {
        self.account_matches(account)?;
        let _guard = self.gate.lock().await;
        let symbols = self.symbols_unlocked().await?;
        let mut client = Client::connect(self.trade.clone()).await?;
        let result = async {
            client
                .orders()
                .await?
                .iter()
                .filter(|r| pending_report(r))
                .map(|r| mapping::pending(&symbols, r))
                .collect()
        }
        .await;
        self.finish_read(&mut client, result).await
    }
    pub(super) async fn attempt(
        &self,
        instrument: &str,
        order: &str,
        position: Option<&str>,
    ) -> Result<AttemptState> {
        let _guard = self.gate.lock().await;
        let symbols = self.symbols_unlocked().await?;
        let symbol = mapping::resolve(&symbols, instrument)?.id;
        let mut client = Client::connect(self.trade.clone()).await?;
        let result = async {
            let orders = client.orders().await?;
            let positions = client.positions().await?;
            mapping::attempt(symbol, order, position, &orders, &positions)
        }
        .await;
        self.finish_read(&mut client, result).await
    }
    pub(super) async fn cancel(&self, account: &str, id: &str) -> Result<()> {
        self.account_matches(account)?;
        ensure!(
            !id.is_empty(),
            "cTrader FIX cancel order ID is empty; no mutation sent"
        );
        let _guard = self.gate.lock().await;
        let lock = AccountLock::acquire(self)?;
        let mut client = Client::connect(self.trade.clone()).await?;
        let mut journals = Vec::new();
        let result = async {
            let orders = client.orders().await?;
            let order = orders
                .iter()
                .find(|order| order.order_id == id && pending_report(order))
                .ok_or_else(|| {
                    eyre!(
                        "cTrader FIX pending order not found; absence does not confirm cancellation"
                    )
                })?;
            self.cancel_exact(&mut client, &lock, order, &mut journals)
                .await
        }
        .await;
        evidence::finish(self, &mut client, &mut journals, result).await
    }
    async fn cancel_exact(
        &self,
        client: &mut Client,
        lock: &AccountLock,
        order: &ExecutionReport,
        journals: &mut Vec<Mutation>,
    ) -> Result<()> {
        let original=order.client_order_id.as_ref().filter(|id|!id.is_empty())
            .ok_or_else(||eyre!("cTrader FIX cancellation requires the broker's original client order ID; no mutation sent"))?;
        let id = Mutation::id();
        let request = CancelOrder {
            client_order_id: id.clone(),
            original_client_order_id: original.clone(),
            order_id: order.order_id.clone(),
        };
        let journal = Mutation::prepare(
            self,
            lock,
            id,
            json!({"operation":"cancel","order_id":order.order_id,"original_client_order_id":original}),
        )?;
        journals.push(journal);
        let journal = journals
            .last_mut()
            .ok_or_else(|| eyre!("cTrader FIX mutation journal unavailable"))?;
        let outcome = client.cancel(request).await;
        journal.outcome(&outcome, &client.take_events())?;
        let report = accepted(outcome?)?;
        ensure!(
            report.order_id == order.order_id && report.status == OrderStatus::Cancelled,
            "cTrader FIX cancellation is not positively confirmed; reconcile before retry"
        );
        let remaining = client.orders().await?;
        journal.record(
            "verification",
            &json!({"orders":remaining,"observed_events":client.take_events()}),
        )?;
        ensure!(
            !remaining
                .iter()
                .any(|r| r.order_id == order.order_id && pending_report(r)),
            "cTrader FIX order remains pending after cancellation; reconcile before retry"
        );
        journal.resolve(&json!({"order_id":report.order_id,"status":"cancelled"}))?;
        Ok(())
    }
    pub(super) async fn cancel_instrument(&self, instrument: &str) -> Result<usize> {
        let _guard = self.gate.lock().await;
        let lock = AccountLock::acquire(self)?;
        let symbols = self.symbols_unlocked().await?;
        let symbol = mapping::resolve(&symbols, instrument)?.id;
        let mut client = Client::connect(self.trade.clone()).await?;
        let mut journals = Vec::new();
        let result=async {
            let orders=client.orders().await?;
            let selected:Vec<_>=orders.iter().filter(|r|r.symbol_id==Some(symbol) && pending_report(r)).collect();
            let mut count=0;
            for order in selected {
                if let Err(error)=self.cancel_exact(&mut client,&lock,order,&mut journals).await {
                    tracing::error!(account=%self.name,instrument,cancelled=count,%error,"cTrader FIX bulk cancellation stopped; no further mutations submitted");
                    return Ok(count);
                }
                count+=1;
            }
            Ok(count)
        }.await;
        evidence::finish(self, &mut client, &mut journals, result).await
    }
    pub(super) async fn close(&self, instrument: &str) -> Result<CloseOutcome> {
        let _guard = self.gate.lock().await;
        let lock = AccountLock::acquire(self)?;
        let symbols = self.symbols_unlocked().await?;
        let symbol = mapping::resolve(&symbols, instrument)?.id;
        let mut client = Client::connect(self.trade.clone()).await?;
        let mut journals = Vec::new();
        let result = async {
            let positions = client.positions().await?;
            let selected: Vec<_> = positions.iter().filter(|p| p.symbol_id == symbol).collect();
            if selected.is_empty() {
                return Ok(CloseOutcome::NothingOpen);
            }
            for position in &selected {
                self.close_exact(&mut client, &lock, position, &mut journals)
                    .await?;
            }
            let remaining = client.positions().await?;
            ensure!(
                !remaining
                    .iter()
                    .any(|position| position.symbol_id == symbol),
                "cTrader FIX instrument is not flat after closing the observed positions"
            );
            Ok(CloseOutcome::Closed(selected.len()))
        }
        .await;
        evidence::finish(self, &mut client, &mut journals, result).await
    }
    async fn close_exact(
        &self,
        client: &mut Client,
        lock: &AccountLock,
        position: &Position,
        journals: &mut Vec<Mutation>,
    ) -> Result<()> {
        let id = Mutation::id();
        let request = mapping::close_order(position, id.clone())?;
        let journal = Mutation::prepare(
            self,
            lock,
            id,
            json!({"operation":"close","position_id":position.position_id,
            "symbol_id":request.symbol_id,"side":request.side,"quantity":request.quantity}),
        )?;
        journals.push(journal);
        let journal = journals
            .last_mut()
            .ok_or_else(|| eyre!("cTrader FIX mutation journal unavailable"))?;
        let outcome = client.place(request).await;
        journal.outcome(&outcome, &client.take_events())?;
        let report = accepted(outcome?)?;
        let remaining = client.positions().await?;
        journal.record(
            "verification",
            &json!({"positions":remaining,"observed_events":client.take_events()}),
        )?;
        mapping::confirmed_close(position, &report, &remaining)?;
        journal.resolve(&json!({"position_id":position.position_id,"closing_order":report}))?;
        Ok(())
    }
}
fn pending_report(report: &ExecutionReport) -> bool {
    matches!(
        report.status,
        OrderStatus::New | OrderStatus::PartiallyFilled
    ) && (report.limit_price.is_some() || report.stop_price.is_some())
}
fn accepted(outcome: MutationOutcome) -> Result<Box<ExecutionReport>> {
    match outcome {
        MutationOutcome::Accepted(report) => Ok(report),
        MutationOutcome::Rejected(rejection) => Err(eyre!(
            "cTrader FIX management request rejected: {}",
            rejection.reason
        )),
        MutationOutcome::Unknown {
            client_order_id, ..
        } => Err(eyre!(
            "cTrader FIX mutation {client_order_id} outcome is unknown; reconcile its durable audit evidence before further mutations"
        )),
    }
}
