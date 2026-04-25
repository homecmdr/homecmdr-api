use tokio::sync::broadcast;

use crate::event::Event;

// Thin wrapper around a tokio broadcast channel that delivers Events to every
// active subscriber.  All parts of the system — adapters, scene runner,
// automation runner, SSE stream — share a single cloned handle to this bus.
#[derive(Debug, Clone)]
pub struct EventBus {
    sender: broadcast::Sender<Event>,
}

impl EventBus {
    /// Create a new bus with a ring-buffer of `capacity` slots.
    /// Slow subscribers that fall behind will start dropping old events once
    /// the buffer is full.
    pub fn new(capacity: usize) -> Self {
        let (sender, _) = broadcast::channel(capacity);
        Self { sender }
    }

    /// Broadcast `event` to every current subscriber.
    /// If nobody is listening the event is silently discarded.
    pub fn publish(&self, event: Event) {
        let _ = self.sender.send(event);
    }

    /// Get a new receiver that will see all events published after this call.
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.sender.subscribe()
    }
}
