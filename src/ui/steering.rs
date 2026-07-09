//! Tier-3 mid-run user-message queue (steering + follow-up).
//!
//! The TUI input loop enqueues what the user types while a turn is running --
//! Enter queues a steering message, Alt+Enter a follow-up -- and the bare agent
//! drains them at its injection points through the Tier-1 [`SteeringSource`]
//! seam. The queue is shared (`Rc`) between the loop (which enqueues) and the
//! harness (which the turn drains through). Mirrors pi's per-`Agent` steering /
//! follow-up queues, but the drain policy lives here, not in the loop.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;

use crate::nexus::SteeringSource;

/// FIFO queue of steering and follow-up messages.
///
/// Interior mutability (`RefCell`) lets one `Rc<SteeringQueue>` be shared across
/// the single-threaded runtime: the `tokio::select!` loop interleaves enqueue
/// (input arm) and drain (turn future) only at await points, and neither holds a
/// borrow across `.await`, so the `RefCell` never double-borrows.
///
/// Drain policy: each poll yields ALL queued messages of that kind in FIFO
/// order. A terminal user normally queues one at a time; draining all keeps the
/// model from trailing several queued instructions. Nexus is policy-neutral, so
/// a one-at-a-time variant would live here, not in the loop.
#[derive(Default)]
pub(crate) struct SteeringQueue {
    steering: RefCell<VecDeque<String>>,
    follow_up: RefCell<VecDeque<String>>,
    /// A `/settings` typed mid-turn: a UI navigation intent, not model input
    /// (issue #489). The input arm sets it instead of steering the command
    /// into the turn; the loop drains it at the next safe boundary and opens
    /// the settings picker there. Kept beside the message queues because both
    /// are shared (`Rc`) between the input arm and the loop.
    settings_request: Cell<bool>,
}

impl SteeringQueue {
    /// Queue a steering message, injected before the next provider request.
    pub(crate) fn enqueue_steering(&self, text: String) {
        self.steering.borrow_mut().push_back(text);
    }

    /// Queue a follow-up message, injected when the agent would otherwise stop.
    pub(crate) fn enqueue_follow_up(&self, text: String) {
        self.follow_up.borrow_mut().push_back(text);
    }

    /// Record that the user asked to open settings mid-turn. Idempotent: a
    /// second request before the boundary drains is a no-op.
    pub(crate) fn request_settings(&self) {
        self.settings_request.set(true);
    }

    /// Take the pending settings request, clearing it. Returns whether one was
    /// queued so the loop opens the settings picker exactly once at the
    /// boundary. Independent of [`clear`]: a UI navigation intent survives a
    /// turn cancel so the picker still opens after the turn ends.
    pub(crate) fn take_settings(&self) -> bool {
        self.settings_request.replace(false)
    }

    /// Count of all queued messages (both kinds), for a "queued" indicator.
    pub(crate) fn len(&self) -> usize {
        self.steering.borrow().len() + self.follow_up.borrow().len()
    }

    /// Drop every queued message. Called on Ctrl-C so aborting a turn also
    /// discards what the user queued, matching pi clearing its queues on abort.
    pub(crate) fn clear(&self) {
        self.steering.borrow_mut().clear();
        self.follow_up.borrow_mut().clear();
    }
}

impl SteeringSource for SteeringQueue {
    fn take_steering(&self) -> Vec<String> {
        self.steering.borrow_mut().drain(..).collect()
    }

    fn take_follow_up(&self) -> Vec<String> {
        self.follow_up.borrow_mut().drain(..).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drains_steering_fifo_and_empties() {
        let queue = SteeringQueue::default();
        queue.enqueue_steering("first".to_string());
        queue.enqueue_steering("second".to_string());
        assert_eq!(queue.len(), 2);

        assert_eq!(queue.take_steering(), vec!["first", "second"]);
        // Drained: a second poll yields nothing and the queue is empty.
        assert!(queue.take_steering().is_empty());
        assert_eq!(queue.len(), 0);
    }

    #[test]
    fn steering_and_follow_up_are_independent() {
        let queue = SteeringQueue::default();
        queue.enqueue_steering("steer".to_string());
        queue.enqueue_follow_up("later".to_string());

        // Draining steering leaves the follow-up queued, and vice versa.
        assert_eq!(queue.take_steering(), vec!["steer"]);
        assert_eq!(queue.len(), 1);
        assert_eq!(queue.take_follow_up(), vec!["later"]);
        assert_eq!(queue.len(), 0);
    }

    #[test]
    fn clear_drops_both_queues() {
        let queue = SteeringQueue::default();
        queue.enqueue_steering("a".to_string());
        queue.enqueue_follow_up("b".to_string());
        queue.clear();
        assert_eq!(queue.len(), 0);
        assert!(queue.take_steering().is_empty());
        assert!(queue.take_follow_up().is_empty());
    }
}
