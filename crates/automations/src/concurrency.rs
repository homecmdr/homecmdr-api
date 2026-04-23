use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::sync::atomic::AtomicBool;

use homecmdr_core::model::AttributeValue;

// ── per-automation concurrency tracking ──────────────────────────────────────

#[derive(Debug, Default)]
pub(crate) struct PerAutomationConcurrency {
    pub(crate) active: usize,
    /// Restart mode: cancel token for the currently running execution.
    pub(crate) cancel: Option<Arc<AtomicBool>>,
    /// Queued mode: pending triggers that haven't started yet.
    pub(crate) queue: VecDeque<PendingExecution>,
}

#[derive(Debug)]
pub(crate) struct PendingExecution {
    pub(crate) event: AttributeValue,
}

pub(crate) type ConcurrencyMap = Arc<Mutex<HashMap<String, PerAutomationConcurrency>>>;

pub(crate) enum SpawnDecision {
    Spawn { cancel: Arc<AtomicBool> },
    Queue,
    Drop,
}
