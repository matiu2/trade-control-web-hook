//! Entry-only spread widening constrained by the setup's price boundaries.

use super::{EntryLevelVeto, Resolved, SlWiden, widen_sl_to_spread_floor};

/// Widening would put the stop at or beyond a setup boundary.
#[derive(Debug, Clone, PartialEq)]
pub struct SlWidenVeto {
    pub widened_stop_loss: f64,
    pub veto: EntryLevelVeto,
}

/// Apply the entry spread floor, checking the rounded stop against price vetos.
/// Open-position stop management does not use this entry-only decision.
/// A boundary violation takes precedence over the recoverable below-min-R
/// result, so a forbidden entry is aborted rather than parked by this gate.
pub fn widen_entry_sl_to_spread_floor(
    resolved: &Resolved,
    spread_price: f64,
    tick_size: f64,
    entry_level_vetos: &[EntryLevelVeto],
) -> Result<SlWiden, SlWidenVeto> {
    let widening = widen_sl_to_spread_floor(
        resolved.entry.reference_price(),
        resolved.stop_loss,
        resolved.take_profit,
        spread_price,
        resolved.min_r,
        tick_size,
    );
    let widened_stop_loss = match widening {
        SlWiden::Unchanged => return Ok(widening),
        SlWiden::Widened { new_stop_loss, .. } => new_stop_loss,
        SlWiden::Reject {
            widened_stop_loss, ..
        } => widened_stop_loss,
    };
    if let Some(veto) = entry_level_vetos
        .iter()
        .find(|veto| veto.is_past(widened_stop_loss))
    {
        return Err(SlWidenVeto {
            widened_stop_loss,
            veto: veto.clone(),
        });
    }
    Ok(widening)
}

#[cfg(test)]
mod tests;
