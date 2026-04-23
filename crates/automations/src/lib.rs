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
