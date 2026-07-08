//! Event propagation: scatter/gather dispatch and event delivery.

use foundry_sdk::event::{Event, mint_gather_id};
use foundry_sdk::scatter::Scatter;

use crate::emit::EventEmitter;
use crate::gather_store::{GatherGroup, GatherStore};
use crate::tracing_context::SpanStamper;

/// Mutable state threaded through a single [`crate::engine::Engine::process`] traversal.
///
/// Bundling these together keeps the per-traversal signatures small and
/// names what they are: the running record of every event seen, the queue of
/// events still to dispatch, and the open gather groups for in-flight
/// scatters.
pub(crate) struct ProcessState {
    /// Every event seen this traversal, in production order — the basis of
    /// the returned [`foundry_sdk::trace::ProcessResult`].
    pub(crate) all_events: Vec<Event>,
    /// Events awaiting dispatch.
    pub(crate) queue: Vec<Event>,
    /// Open scatter/gather groups for the duration of this traversal.
    pub(crate) gather_store: GatherStore,
}

impl ProcessState {
    /// Initialise a fresh traversal state from the root event.
    pub(crate) fn new(root_event: Event) -> Self {
        Self {
            all_events: vec![root_event.clone()],
            queue: vec![root_event],
            gather_store: GatherStore::new(),
        }
    }
}

/// Propagates events through the engine: stamps context, persists, broadcasts,
/// and manages scatter/gather dispatch.
pub(crate) struct Propagator<'a> {
    emitter: &'a EventEmitter,
    stamper: &'a SpanStamper,
}

impl<'a> Propagator<'a> {
    pub(crate) fn new(emitter: &'a EventEmitter, stamper: &'a SpanStamper) -> Self {
        Self { emitter, stamper }
    }

    /// Offer a delivered event to the gather store. If it satisfies a gather
    /// group, persist, broadcast, and enqueue the synthesized reduce event —
    /// then recurse, since a reduce event may itself satisfy an outer
    /// (nested) group.
    pub(crate) fn deliver_reduce_if_satisfied(&self, event: &Event, state: &mut ProcessState) {
        let Some(reduce) = state.gather_store.record(event) else {
            return;
        };
        tracing::info!(
            gather_id = reduce.gather_id.as_deref().unwrap_or("-"),
            reduce_event = %reduce.event_type,
            "gather satisfied — synthesizing reduce event"
        );
        self.emitter.persist_one(&reduce);
        state.all_events.push(reduce.clone());
        state.queue.push(reduce.clone());
        self.deliver_reduce_if_satisfied(&reduce, state);
    }

    /// Propagate trace IDs, stamp OTel-shaped span context, persist to JSONL,
    /// broadcast to Watch subscribers, and optionally deliver to the processing
    /// queue. Delivered events are offered to the gather store, which may
    /// synthesize reduce events. Returns collected event IDs and payloads for
    /// the [`foundry_sdk::trace::BlockExecution`] record.
    pub(crate) fn persist_and_broadcast_events(
        &self,
        events: Vec<Event>,
        trigger: &Event,
        block_span_id: &str,
        state: &mut ProcessState,
        deliver: bool,
    ) -> (Vec<String>, Vec<serde_json::Value>) {
        let mut emitted_ids = Vec::new();
        let mut emitted_payloads = Vec::new();
        for mut emitted in events {
            self.stamper.stamp_context(&mut emitted, trigger, block_span_id);
            self.emitter.persist_one(&emitted);
            emitted_ids.push(emitted.id.clone());
            emitted_payloads.push(emitted.payload.clone());
            state.all_events.push(emitted.clone());
            if deliver {
                state.queue.push(emitted.clone());
                self.deliver_reduce_if_satisfied(&emitted, state);
            } else {
                tracing::info!(event_type = %emitted.event_type, "event logged but delivery throttled");
            }
        }
        (emitted_ids, emitted_payloads)
    }

    /// Open a gather group from a block's [`Scatter`] declaration: mint a
    /// fresh `gather_id`, stamp every child with it, register the group, and
    /// dispatch the children. Returns the child event IDs for the
    /// [`foundry_sdk::trace::BlockExecution`] record. An empty scatter satisfies its
    /// gather at once and its reduce event is delivered immediately.
    pub(crate) fn dispatch_scatter(
        &self,
        scatter: Scatter,
        trigger: &Event,
        block_span_id: &str,
        state: &mut ProcessState,
        deliver: bool,
    ) -> Vec<String> {
        let Scatter {
            mut children,
            gather,
        } = scatter;
        let gather_id = mint_gather_id();
        // A scatter opens a NEW group: override any inherited gather_id so the
        // children belong to this group rather than an enclosing one.
        for child in &mut children {
            child.gather_id = Some(gather_id.clone());
        }
        let group = GatherGroup::new(gather_id.clone(), children.len(), gather, trigger);
        let immediate = state.gather_store.open(group);
        let child_count = children.len();
        let (child_ids, _payloads) =
            self.persist_and_broadcast_events(children, trigger, block_span_id, state, deliver);
        tracing::info!(gather_id = %gather_id, children = child_count, "scatter dispatched");
        // An empty scatter is satisfied on registration — deliver its reduce.
        if let Some(reduce) = immediate
            && deliver
        {
            self.emitter.persist_one(&reduce);
            state.all_events.push(reduce.clone());
            state.queue.push(reduce.clone());
            self.deliver_reduce_if_satisfied(&reduce, state);
        }
        child_ids
    }
}
