//! Chain-agnostic core of BorrowGuard: position math and the rebalance
//! controller. No Solana dependencies, so the backtester and the live bot
//! share exactly the same decision logic.

pub mod controller;
pub mod position;
pub mod venue;

pub use controller::{Action, Params, ParamsError};
pub use position::{leverage_from_ltv, ltv_from_leverage, Position};
pub use venue::{Snapshot, Venue};
