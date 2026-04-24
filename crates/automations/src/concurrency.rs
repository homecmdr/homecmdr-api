//! Per-automation concurrency tracking.
//!
//! Automation Lua files can declare a `mode` field that controls what happens
//! when a new trigger fires while a previous execution is still running.
//! Supported modes: `parallel` (run up to N at once), `single` (skip if busy),
//! `queued` (buffer up to N pending triggers), and `restart` (cancel the
//! current run and start a new one).
//!
//! This module holds the in-memory state table used by the runner to enforce
//! those policies.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::sync::atomic::AtomicBool;

use homecmdr_core::model::AttributeValue;

// ── per-automation concurrency tracking ──────────────────────────────────────
// Each automation gets one `PerAutomationConcurrency` entry in the shared map.
// The entry is created on first use and lives for the lifetime of the runner.
// All mutations are protected by the `Mutex` on `ConcurrencyMap`.

/// Live concurrency state for a single automation.
#[derive(Debug, Default)]
pub(crate) struct PerAutomationConcurrency {
    pub(crate) active: usize,
    /// Restart mode: cancel token for the currently running execution.
    pub(crate) cancel: Option<Arc<AtomicBool>>,
    /// Queued mode: pending triggers that haven't started yet.
    pub(crate) queue: VecDeque<PendingExecution>,
}

/// A trigger that arrived while the automation was busy and is waiting to run.
#[derive(Debug)]
pub(crate) struct PendingExecution {
    pub(crate) event: AttributeValue,
}

/// Shared map from automation id to its live concurrency state.
pub(crate) type ConcurrencyMap = Arc<Mutex<HashMap<String, PerAutomationConcurrency>>>;

/// The decision returned by the concurrency check inside
/// [`spawn_automation_execution`].
pub(crate) enum SpawnDecision {
    Spawn { cancel: Arc<AtomicBool> },
    Queue,
    Drop,
}
