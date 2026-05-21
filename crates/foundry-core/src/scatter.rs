//! Scatter/gather (map/reduce) coordination primitives.
//!
//! A task block declares a [`Scatter`] in its [`TaskBlockResult`] when it
//! wants to fan work out into several child events and have a single reduce
//! event synthesized once those children complete. The block never polls or
//! holds coordination state — it *declares* the fan-out and the engine owns
//! the bookkeeping.
//!
//! [`TaskBlockResult`]: crate::task_block::TaskBlockResult

use crate::event::{Event, EventType};

/// A fan-out declared by a task block.
///
/// The engine, on receiving a `Scatter`, mints a fresh `gather_id`, stamps it
/// onto every child, dispatches the children, and registers a gather group.
/// Once the group's [`GatherPolicy`] is satisfied it synthesizes a reduce
/// event of type [`GatherSpec::reduce_event_type`].
#[derive(Debug)]
pub struct Scatter {
    /// The fan-out events to dispatch. Each is stamped with the group's
    /// freshly minted `gather_id` before dispatch. An empty list satisfies
    /// the gather immediately.
    pub children: Vec<Event>,
    /// How and when the reduce event is synthesized.
    pub gather: GatherSpec,
}

impl Scatter {
    /// Construct a scatter that waits for *all* children before reducing.
    pub fn all(
        children: Vec<Event>,
        on: Vec<EventType>,
        reduce_event_type: EventType,
        reduce_project: impl Into<String>,
    ) -> Self {
        Self {
            children,
            gather: GatherSpec {
                on,
                policy: GatherPolicy::All,
                reduce_event_type,
                reduce_project: reduce_project.into(),
                context: serde_json::Value::Null,
            },
        }
    }

    /// Attach verbatim block context to be echoed into the reduce payload.
    #[must_use]
    pub fn with_context(mut self, context: serde_json::Value) -> Self {
        self.gather.context = context;
        self
    }
}

/// Describes how a gather group completes and what it emits when it does.
#[derive(Debug)]
pub struct GatherSpec {
    /// Event types that count as a child completion. An event counts toward
    /// the group when its `gather_id` matches the group and its `event_type`
    /// is in this list.
    pub on: Vec<EventType>,
    /// When the group is satisfied.
    pub policy: GatherPolicy,
    /// The event type the engine synthesizes when the group is satisfied.
    pub reduce_event_type: EventType,
    /// The `project` of the synthesized reduce event. Fan-out children may
    /// each carry a different project, so the reduce project is explicit
    /// rather than inherited from a child.
    pub reduce_project: String,
    /// Opaque block-supplied context, echoed verbatim into the reduce
    /// event's payload so the reduce block can recover its own state.
    pub context: serde_json::Value,
}

/// When a gather group is considered satisfied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GatherPolicy {
    /// Wait for every child to complete.
    All,
    /// Wait for the first `n` completions to arrive. `Count(1)` is "first
    /// wins"; `Count(n)` where `n >= expected` behaves like [`All`].
    ///
    /// [`All`]: GatherPolicy::All
    Count(usize),
}

impl GatherPolicy {
    /// The number of arrivals required to satisfy the policy, given how many
    /// children were scattered.
    #[must_use]
    pub fn threshold(self, expected: usize) -> usize {
        match self {
            GatherPolicy::All => expected,
            GatherPolicy::Count(n) => n.min(expected),
        }
    }

    /// Whether `arrived` completions satisfy the policy for `expected`
    /// children.
    #[must_use]
    pub fn is_satisfied(self, arrived: usize, expected: usize) -> bool {
        arrived >= self.threshold(expected)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_threshold_is_the_child_count() {
        assert_eq!(GatherPolicy::All.threshold(3), 3);
        assert_eq!(GatherPolicy::All.threshold(0), 0);
    }

    #[test]
    fn count_threshold_is_capped_at_child_count() {
        assert_eq!(GatherPolicy::Count(2).threshold(5), 2);
        // A count larger than the child set degrades to "all".
        assert_eq!(GatherPolicy::Count(9).threshold(3), 3);
    }

    #[test]
    fn all_is_satisfied_only_when_every_child_arrives() {
        assert!(!GatherPolicy::All.is_satisfied(2, 3));
        assert!(GatherPolicy::All.is_satisfied(3, 3));
    }

    #[test]
    fn empty_scatter_is_immediately_satisfied() {
        // Zero children: threshold is zero, so zero arrivals satisfy it.
        assert!(GatherPolicy::All.is_satisfied(0, 0));
        assert!(GatherPolicy::Count(1).is_satisfied(0, 0));
    }

    #[test]
    fn count_is_satisfied_at_the_requested_number() {
        assert!(!GatherPolicy::Count(2).is_satisfied(1, 5));
        assert!(GatherPolicy::Count(2).is_satisfied(2, 5));
        assert!(GatherPolicy::Count(2).is_satisfied(3, 5));
    }

    #[test]
    fn scatter_all_builds_an_all_policy_spec() {
        let scatter = Scatter::all(
            vec![],
            vec![EventType::ProjectRunCompleted],
            EventType::MaintenanceCycleCompleted,
            "system",
        );
        assert_eq!(scatter.gather.policy, GatherPolicy::All);
        assert_eq!(scatter.gather.reduce_project, "system");
        assert_eq!(scatter.gather.context, serde_json::Value::Null);
    }

    #[test]
    fn with_context_attaches_block_context() {
        let scatter = Scatter::all(
            vec![],
            vec![EventType::ProjectRunCompleted],
            EventType::MaintenanceCycleCompleted,
            "system",
        )
        .with_context(serde_json::json!({ "goal_id": "goal_123" }));
        assert_eq!(scatter.gather.context["goal_id"], "goal_123");
    }
}
