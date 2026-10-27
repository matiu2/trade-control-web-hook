//! Signal gates share account-scoped spread clocks with dispatch and fills.
//!
//! Preparation belongs to the whole plan: if matching rules name several
//! accounts, any account's active clock suppresses preparation. No account wins
//! by rule order. This does not infer broker trading-session or market hours.
use std::{borrow::Cow, collections::BTreeSet};

use chrono::{DateTime, Utc};
use trade_control_core::{intent::Intent, spread_blackout};

use crate::{Granularity, TradePlan};

type BlockWindow = (DateTime<Utc>, DateTime<Utc>);

fn keys(plan: &TradePlan) -> BTreeSet<Cow<'_, str>> {
    let keys: BTreeSet<_> = plan
        .rules
        .iter()
        .filter(|rule| rule.intent.instrument == plan.instrument)
        .map(|rule| {
            spread_blackout::spread_lookup_key(
                &rule.intent.instrument,
                rule.intent.account.as_deref(),
            )
        })
        .collect();
    if keys.is_empty() {
        BTreeSet::from([Cow::Borrowed(plan.instrument.as_str())])
    } else {
        keys
    }
}

pub fn suppress_plan(plan: &TradePlan, at: DateTime<Utc>) -> bool {
    keys(plan)
        .iter()
        .any(|key| spread_blackout::suppress_on_spread_hour(key, at, plan.granularity))
}

pub fn suppress_intent(intent: &Intent, at: DateTime<Utc>, granularity: Granularity) -> bool {
    spread_blackout::suppress_on_spread_hour(
        &spread_blackout::spread_lookup_key(&intent.instrument, intent.account.as_deref()),
        at,
        granularity,
    )
}

/// Union the bounds of active blocks for the replay trace, matching suppression.
/// Lead-in suppression can precede a reported block and legitimately yield None.
pub fn block_window(plan: &TradePlan, at: DateTime<Utc>) -> Option<BlockWindow> {
    keys(plan)
        .iter()
        .filter_map(|key| spread_blackout::spread_block_window(key, at))
        .reduce(|(from, to), (next_from, next_to)| (from.min(next_from), to.max(next_to)))
}

#[cfg(test)]
mod tests;
