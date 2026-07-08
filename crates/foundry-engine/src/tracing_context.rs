//! Span and causation context stamping for emitted events.

use std::collections::HashSet;

use foundry_sdk::event::{Event, EventType};

/// Stamps OTel-shaped span context and causal provenance onto emitted events.
///
/// All stamping is "set if unset", so a block may emit an event with explicit
/// context and that context is preserved.
///
/// # Causation
///
/// The emitted event's `causation_id` is set to the triggering event's `id` —
/// recording the direct causal edge in the event graph, independent of the
/// observability span structure.
///
/// # Gather membership
///
/// The emitted event inherits the trigger's `gather_id` verbatim — the same
/// propagation rule as `trace_id`. This carries fan-out group membership all
/// the way down a scattered child's sub-workflow so the terminal completion
/// event still identifies its gather.
///
/// # Span rules
///
/// - **Default** (non-opener events): the emitted event is a peer of the
///   trigger — it inherits the trigger's `trace_id`, `span_id` (the active
///   workflow span), and `parent_span_id`.
/// - **Span opener** (e.g. `ProjectIterationRequested`): the emitted event opens a
///   new workflow span — it inherits the trigger's `trace_id`, receives a
///   freshly minted `span_id`, and is parented to the emitting block's
///   `block_span_id`.
pub(crate) struct SpanStamper {
    span_openers: HashSet<EventType>,
}

impl Default for SpanStamper {
    fn default() -> Self {
        Self::new()
    }
}

impl SpanStamper {
    pub(crate) fn new() -> Self {
        Self {
            span_openers: HashSet::new(),
        }
    }

    /// Register additional event types that open a new trace span when emitted.
    ///
    /// Built-in openers (see [`EventType::is_span_opener`]) are always
    /// recognized; this is how a contributor-defined workflow declares that its
    /// own root event (typically an [`EventType::Custom`] `*_requested`) should
    /// open a span.
    pub(crate) fn with_openers(mut self, openers: impl IntoIterator<Item = EventType>) -> Self {
        self.span_openers.extend(openers);
        self
    }

    /// Whether an emitted event of this type should open a new workflow span —
    /// either a built-in opener or one registered via [`SpanStamper::with_openers`].
    pub(crate) fn opens_span(&self, event_type: &EventType) -> bool {
        event_type.is_span_opener() || self.span_openers.contains(event_type)
    }

    /// Apply causal and OTel-shaped tracing context to an emitted event.
    pub(crate) fn stamp_context(&self, emitted: &mut Event, trigger: &Event, block_span_id: &str) {
        use foundry_sdk::event::mint_span_id;

        if emitted.causation_id.is_none() {
            emitted.causation_id = Some(trigger.id.clone());
        }

        if emitted.trace_id.is_none() {
            emitted.trace_id.clone_from(&trigger.trace_id);
        }

        if emitted.gather_id.is_none() {
            emitted.gather_id.clone_from(&trigger.gather_id);
        }

        if self.opens_span(&emitted.event_type) {
            // New workflow span: child of the emitting block's span.
            if emitted.span_id.is_none() {
                emitted.span_id = Some(mint_span_id());
            }
            if emitted.parent_span_id.is_none() {
                emitted.parent_span_id = Some(block_span_id.to_string());
            }
        } else {
            // Default: peer of trigger, attached to the same workflow span.
            if emitted.span_id.is_none() {
                emitted.span_id.clone_from(&trigger.span_id);
            }
            if emitted.parent_span_id.is_none() {
                emitted.parent_span_id.clone_from(&trigger.parent_span_id);
            }
        }
    }
}
