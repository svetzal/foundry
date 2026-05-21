//! In-flight scatter/gather coordination state.
//!
//! When a task block declares a [`Scatter`], the engine mints a `gather_id`,
//! registers a [`GatherGroup`] here, and dispatches the children. Every
//! delivered event is then offered to the store; when a group's
//! [`GatherPolicy`] is satisfied the store builds the reduce event and the
//! engine delivers it.
//!
//! For now a `GatherStore` is scoped to a single `Engine::process` call —
//! the synchronous regime where a scatter and its gather complete within one
//! traversal. A durable, daemon-scoped store (gathers spanning calls and
//! restarts) is a later step behind this same interface.
//!
//! [`Scatter`]: foundry_core::scatter::Scatter

use std::collections::{HashMap, HashSet};

use foundry_core::event::{Event, EventType};
use foundry_core::payload::{GatherCompletedPayload, GatheredChild};
use foundry_core::scatter::{GatherPolicy, GatherSpec};
use foundry_core::throttle::Throttle;

/// One open fan-out group: the children expected, those arrived so far, and
/// everything needed to synthesize the reduce event when satisfied.
pub(crate) struct GatherGroup {
    gather_id: String,
    /// The group this scatter was nested inside, if any. The synthesized
    /// reduce event inherits this `gather_id` so control returns to the
    /// outer group.
    parent_gather_id: Option<String>,
    expected: usize,
    on: Vec<EventType>,
    policy: GatherPolicy,
    reduce_event_type: EventType,
    reduce_project: String,
    context: serde_json::Value,
    // Scatter-trigger context, replayed onto the reduce event so it sits as
    // a peer of the event that opened the scatter.
    trace_id: Option<String>,
    span_id: Option<String>,
    parent_span_id: Option<String>,
    throttle: Throttle,
    // Accumulating arrival state.
    seen: HashSet<String>,
    arrived: Vec<GatheredChild>,
}

impl GatherGroup {
    /// Build a group from a freshly minted `gather_id`, the scattered child
    /// count, the block's [`GatherSpec`], and the scatter-triggering event.
    pub(crate) fn new(
        gather_id: String,
        expected: usize,
        gather: GatherSpec,
        trigger: &Event,
    ) -> Self {
        Self {
            gather_id,
            parent_gather_id: trigger.gather_id.clone(),
            expected,
            on: gather.on,
            policy: gather.policy,
            reduce_event_type: gather.reduce_event_type,
            reduce_project: gather.reduce_project,
            context: gather.context,
            trace_id: trigger.trace_id.clone(),
            span_id: trigger.span_id.clone(),
            parent_span_id: trigger.parent_span_id.clone(),
            throttle: trigger.throttle,
            seen: HashSet::new(),
            arrived: Vec::new(),
        }
    }

    fn is_satisfied(&self) -> bool {
        self.policy.is_satisfied(self.arrived.len(), self.expected)
    }

    /// Consume the group and synthesize its reduce event. `caused_by` is the
    /// `id` of the completion that satisfied the group, or `None` for an
    /// empty scatter that was satisfied on registration.
    fn into_reduce_event(self, caused_by: Option<String>) -> Event {
        let payload = GatherCompletedPayload {
            gather_id: self.gather_id,
            expected: self.expected,
            arrived: self.arrived.len(),
            context: self.context,
            children: self.arrived,
        };
        let payload = Event::serialize_payload(&payload)
            .expect("GatherCompletedPayload is infallibly serializable");
        Event::new(self.reduce_event_type, self.reduce_project, self.throttle, payload)
            .with_trace_id(self.trace_id)
            .with_span_ids(self.span_id, self.parent_span_id)
            .with_causation_id(caused_by)
            .with_gather_id(self.parent_gather_id)
    }
}

/// Read the conventional `success` boolean from an event payload, if present.
fn payload_success(event: &Event) -> Option<bool> {
    event.payload.get("success").and_then(serde_json::Value::as_bool)
}

/// The set of open gather groups for one `process()` traversal.
pub(crate) struct GatherStore {
    groups: HashMap<String, GatherGroup>,
}

impl GatherStore {
    pub(crate) fn new() -> Self {
        Self {
            groups: HashMap::new(),
        }
    }

    /// Register a new gather group. Returns the reduce event immediately when
    /// the group is already satisfied — i.e. an empty scatter (zero children)
    /// or a `Count(0)` policy.
    pub(crate) fn open(&mut self, group: GatherGroup) -> Option<Event> {
        if group.is_satisfied() {
            return Some(group.into_reduce_event(None));
        }
        self.groups.insert(group.gather_id.clone(), group);
        None
    }

    /// Offer a delivered event to any matching open group.
    ///
    /// An event counts toward a group when its `gather_id` names that group
    /// and its `event_type` is in the group's `on` set. Arrivals are
    /// deduplicated by `Event::id`, so a replayed or redelivered completion
    /// is never counted twice. When the arrival satisfies the group's
    /// policy, the group is removed and its reduce event is returned.
    pub(crate) fn record(&mut self, event: &Event) -> Option<Event> {
        let gather_id = event.gather_id.as_deref()?;
        let group = self.groups.get_mut(gather_id)?;

        if !group.on.contains(&event.event_type) {
            return None;
        }
        if !group.seen.insert(event.id.clone()) {
            return None; // already counted — idempotent under replay/redelivery
        }
        group.arrived.push(GatheredChild {
            event_id: event.id.clone(),
            event_type: event.event_type.clone(),
            project: event.project.clone(),
            success: payload_success(event),
            payload: event.payload.clone(),
        });

        if group.is_satisfied() {
            let group = self.groups.remove(gather_id).expect("group was just present");
            return Some(group.into_reduce_event(Some(event.id.clone())));
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use foundry_core::scatter::Scatter;

    fn scatter_trigger() -> Event {
        Event::new(
            EventType::MaintenanceCycleStarted,
            "system".to_string(),
            Throttle::Full,
            serde_json::json!({}),
        )
        .with_trace_id(Some("0123456789abcdef0123456789abcdef".to_string()))
        .with_span_ids(Some("0123456789abcdef".to_string()), None)
    }

    fn completion(gather_id: &str, project: &str, success: bool) -> Event {
        Event::new(
            EventType::ProjectRunCompleted,
            project.to_string(),
            Throttle::Full,
            serde_json::json!({ "success": success }),
        )
        .with_gather_id(Some(gather_id.to_string()))
    }

    fn all_group(gather_id: &str, expected: usize, trigger: &Event) -> GatherGroup {
        let spec = Scatter::all(
            vec![],
            vec![EventType::ProjectRunCompleted],
            EventType::MaintenanceCycleCompleted,
            "system",
        )
        .gather;
        GatherGroup::new(gather_id.to_string(), expected, spec, trigger)
    }

    #[test]
    fn open_returns_reduce_immediately_for_empty_scatter() {
        let trigger = scatter_trigger();
        let mut store = GatherStore::new();
        let reduce = store.open(all_group("gth_empty", 0, &trigger));
        let reduce = reduce.expect("empty scatter satisfies its gather at once");
        assert_eq!(reduce.event_type, EventType::MaintenanceCycleCompleted);
        let payload: GatherCompletedPayload = reduce.parse_payload().unwrap();
        assert_eq!(payload.expected, 0);
        assert_eq!(payload.arrived, 0);
        assert!(payload.children.is_empty());
        // Empty scatter has no satisfying completion.
        assert!(reduce.causation_id.is_none());
    }

    #[test]
    fn record_synthesizes_reduce_when_all_children_arrive() {
        let trigger = scatter_trigger();
        let mut store = GatherStore::new();
        assert!(store.open(all_group("gth_1", 2, &trigger)).is_none());

        assert!(
            store.record(&completion("gth_1", "alpha", true)).is_none(),
            "first of two children does not satisfy an All gather",
        );
        let last = completion("gth_1", "beta", false);
        let reduce = store.record(&last).expect("second child satisfies the gather");

        assert_eq!(reduce.event_type, EventType::MaintenanceCycleCompleted);
        assert_eq!(reduce.project, "system");
        assert_eq!(reduce.causation_id.as_ref(), Some(&last.id));
        let payload: GatherCompletedPayload = reduce.parse_payload().unwrap();
        assert_eq!(payload.expected, 2);
        assert_eq!(payload.arrived, 2);
        assert_eq!(payload.children.len(), 2);
        assert_eq!(payload.children[0].success, Some(true));
        assert_eq!(payload.children[1].success, Some(false));
    }

    #[test]
    fn record_ignores_event_types_not_in_the_on_set() {
        let trigger = scatter_trigger();
        let mut store = GatherStore::new();
        store.open(all_group("gth_2", 1, &trigger));

        let unrelated = Event::new(
            EventType::ProjectChangesPushed,
            "alpha".to_string(),
            Throttle::Full,
            serde_json::json!({}),
        )
        .with_gather_id(Some("gth_2".to_string()));
        assert!(store.record(&unrelated).is_none(), "non-`on` event must not count");

        // The real completion still satisfies the group.
        assert!(store.record(&completion("gth_2", "alpha", true)).is_some());
    }

    #[test]
    fn record_deduplicates_repeated_arrivals_by_event_id() {
        let trigger = scatter_trigger();
        let mut store = GatherStore::new();
        store.open(all_group("gth_3", 2, &trigger));

        let child = completion("gth_3", "alpha", true);
        assert!(store.record(&child).is_none());
        // The very same event arriving again must not count a second time.
        assert!(store.record(&child).is_none(), "duplicate event id must not advance the gather",);
    }

    #[test]
    fn record_returns_none_for_events_without_a_gather_id() {
        let trigger = scatter_trigger();
        let mut store = GatherStore::new();
        store.open(all_group("gth_4", 1, &trigger));

        let no_group = Event::new(
            EventType::ProjectRunCompleted,
            "alpha".to_string(),
            Throttle::Full,
            serde_json::json!({ "success": true }),
        );
        assert!(store.record(&no_group).is_none());
    }

    #[test]
    fn record_ignores_arrivals_after_the_group_is_satisfied() {
        let trigger = scatter_trigger();
        let mut store = GatherStore::new();
        store.open(all_group("gth_5", 1, &trigger));

        assert!(store.record(&completion("gth_5", "alpha", true)).is_some());
        // The group has been removed; a late straggler matches nothing.
        assert!(store.record(&completion("gth_5", "beta", true)).is_none());
    }

    #[test]
    fn reduce_event_inherits_parent_gather_id_for_nesting() {
        let trigger = scatter_trigger().with_gather_id(Some("gth_outer".to_string()));
        let mut store = GatherStore::new();
        store.open(all_group("gth_inner", 1, &trigger));

        let reduce = store.record(&completion("gth_inner", "alpha", true)).unwrap();
        assert_eq!(
            reduce.gather_id.as_deref(),
            Some("gth_outer"),
            "the reduce event rejoins the outer gather group",
        );
    }

    #[test]
    fn count_policy_satisfies_before_all_children_arrive() {
        let trigger = scatter_trigger();
        let spec = GatherSpec {
            on: vec![EventType::ProjectRunCompleted],
            policy: GatherPolicy::Count(1),
            reduce_event_type: EventType::MaintenanceCycleCompleted,
            reduce_project: "system".to_string(),
            context: serde_json::Value::Null,
        };
        let group = GatherGroup::new("gth_6".to_string(), 3, spec, &trigger);
        let mut store = GatherStore::new();
        store.open(group);

        let reduce = store.record(&completion("gth_6", "alpha", true));
        assert!(reduce.is_some(), "Count(1) satisfies on the first arrival");
        let payload: GatherCompletedPayload = reduce.unwrap().parse_payload().unwrap();
        assert_eq!(payload.expected, 3);
        assert_eq!(payload.arrived, 1);
    }
}
