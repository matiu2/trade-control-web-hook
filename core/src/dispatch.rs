//! Broker-dispatch + control-result carriers, shared by the wasm worker and
//! the offline replay.
//!
//! The five `ActionResult`-returning dispatch functions (`run_action`,
//! `run_enter`, `run_close`, `run_invalidate`, `run_veto_with_broker`) and the
//! [`ActionResult`] enum live here so the Cloudflare Worker AND the native
//! replay tools call the *same* trade-critical gates — they can't drift
//! (`[[strategy_changes_in_both_replayer_and_worker]]`). Everything here is
//! generic over `<B: Broker, S: StateStore>` and worker-free: it logs via plain
//! `tracing::{info,error}!` (the worker's `ConsoleSubscriber` tees those into
//! its R2 recording buffer), and takes a resolved [`DispatchConfig`] rather than
//! a worker `Env`.
//!
//! [`ControlResult`] is the control-action sibling carrier (status + body) for
//! the prep/pause/register/plan path.

mod action;
mod action_result;
mod close;
mod control_result;
mod enter;
mod invalidate;
mod shared;
mod veto;

pub use action::*;
pub use action_result::*;
pub use close::*;
pub use control_result::*;
pub use enter::*;
pub use invalidate::*;
pub use shared::*;
pub use veto::*;
