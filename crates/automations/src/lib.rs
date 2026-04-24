//! The automations crate — load Lua automation files, watch for trigger events,
//! and execute the right automation at the right time.
//!
//! The main entry points are:
//! - [`AutomationCatalog`] — loads `.lua` files from a directory and provides
//!   the list and execute APIs used by the HTTP layer.
//! - [`AutomationRunner`] — spawns background tasks that watch for triggers
//!   (device events, scheduled times, intervals) and call into the catalog.
//! - [`AutomationController`] — a cheap, cloneable handle to the running
//!   catalog that the HTTP handlers use to inspect and control automations.

mod catalog;
mod concurrency;
mod conditions;
mod events;
mod runner;
mod schedule;
mod state;
mod triggers;
mod types;

pub use catalog::AutomationCatalog;
pub use runner::{AutomationController, AutomationExecutionObserver, AutomationRunner};
pub use types::{AutomationExecutionResult, AutomationSummary, ReloadError, TriggerContext};

#[cfg(test)]
mod tests;
