use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Chain context — propagated through the iterate / maintenance chain
// ---------------------------------------------------------------------------

/// Optional context fields that propagate through the iterate chain.
///
/// Every block that builds an outgoing payload must forward these fields
/// unchanged so downstream blocks can see them. Use `#[serde(flatten)]`
/// when embedding in a payload struct so these fields appear at the top level.
///
/// The fields mirror those copied by `forward_chain_context`:
/// `actions`, `prompt`, `gates`, `audit_name`, and `loop_context`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChainContext {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actions: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gates: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audit_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub loop_context: Option<serde_json::Value>,
    /// Per-request agent provider override (`"claude"` | `"opencode"` |
    /// `"codex"`). Set on the entry request and forwarded unchanged through the
    /// chain so every agent invocation in the run uses the same backend. Absent
    /// means "use the daemon's default provider".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_provider: Option<String>,
    /// Campaign that owns this task run, when dispatched by a campaign.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub campaign: Option<String>,
    /// Which cycle of that campaign this run belongs to, 1-based. Carried
    /// alongside `campaign` so an event says not just which campaign it serves
    /// but which attempt — see [`super::LoopContext::campaign_cycle`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub campaign_cycle: Option<u64>,
    /// Isolated task worktree prepared by the executor. Absent before execution.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_worktree: Option<String>,
    /// Durable branch associated with the isolated task worktree.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_branch: Option<String>,
    /// Git ref from which a continuation task should start.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_ref: Option<String>,
}

impl ChainContext {
    /// Extract chain context fields from a JSON payload object.
    pub fn extract_from(payload: &serde_json::Value) -> Self {
        Self {
            actions: payload.get("actions").cloned(),
            prompt: payload.get("prompt").cloned(),
            gates: payload.get("gates").cloned(),
            audit_name: payload
                .get("audit_name")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string),
            loop_context: payload.get("loop_context").cloned(),
            agent_provider: payload
                .get("agent_provider")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string),
            campaign: payload
                .get("campaign")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string),
            campaign_cycle: payload.get("campaign_cycle").and_then(serde_json::Value::as_u64),
            task_worktree: payload
                .get("task_worktree")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string),
            task_branch: payload
                .get("task_branch")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string),
            base_ref: payload
                .get("base_ref")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string),
        }
    }

    /// Merge chain context fields into a mutable JSON payload object.
    ///
    /// Only fields that are `Some` are written; existing fields are overwritten.
    pub fn merge_into(&self, target: &mut serde_json::Value) {
        if let Some(v) = &self.actions {
            target["actions"] = v.clone();
        }
        if let Some(v) = &self.prompt {
            target["prompt"] = v.clone();
        }
        if let Some(v) = &self.gates {
            target["gates"] = v.clone();
        }
        if let Some(v) = &self.audit_name {
            target["audit_name"] = serde_json::json!(v);
        }
        if let Some(v) = &self.loop_context {
            target["loop_context"] = v.clone();
        }
        if let Some(v) = &self.agent_provider {
            target["agent_provider"] = serde_json::json!(v);
        }
        if let Some(v) = &self.campaign {
            target["campaign"] = serde_json::json!(v);
        }
        if let Some(v) = &self.campaign_cycle {
            target["campaign_cycle"] = serde_json::json!(v);
        }
        if let Some(v) = &self.task_worktree {
            target["task_worktree"] = serde_json::json!(v);
        }
        if let Some(v) = &self.task_branch {
            target["task_branch"] = serde_json::json!(v);
        }
        if let Some(v) = &self.base_ref {
            target["base_ref"] = serde_json::json!(v);
        }
    }
}

/// Subset of `ChainContext` carrying only `loop_context` and `actions`.
///
/// Used by blocks that call `forward_loop_context` (not the full chain context):
/// `execute_plan`, `run_verify_gates`, `retry_execution`, `direct_prompt`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoopContext {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub loop_context: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actions: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub campaign: Option<String>,
    /// Which cycle of its campaign this task belongs to, 1-based.
    ///
    /// Every task-side event already carries `campaign`, so grouping events to
    /// a campaign always worked; what did not was telling one cycle from the
    /// next. Only `CampaignAdvanceCompleted` recorded `cycles_completed`, so a
    /// consumer had to sort on time and treat each `CampaignAdvanceRequested`
    /// as a boundary — a guess that breaks on concurrent campaigns in one
    /// project or on out-of-order arrival. `None` for non-campaign tasks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub campaign_cycle: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_worktree: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_ref: Option<String>,
}

impl LoopContext {
    /// Extract loop context fields from a JSON payload object.
    pub fn extract_from(payload: &serde_json::Value) -> Self {
        Self {
            loop_context: payload.get("loop_context").cloned(),
            actions: payload.get("actions").cloned(),
            prompt: payload.get("prompt").cloned(),
            agent_provider: payload
                .get("agent_provider")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string),
            campaign: payload
                .get("campaign")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string),
            campaign_cycle: payload.get("campaign_cycle").and_then(serde_json::Value::as_u64),
            task_worktree: payload
                .get("task_worktree")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string),
            task_branch: payload
                .get("task_branch")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string),
            base_ref: payload
                .get("base_ref")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string),
        }
    }
}

// ---------------------------------------------------------------------------
// Strategic loop context types
// ---------------------------------------------------------------------------

fn default_strategic_max() -> u64 {
    5
}

/// Typed sub-fields of `loop_context.strategic`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StrategicContext {
    #[serde(default)]
    pub iteration: u64,
    #[serde(default = "default_strategic_max")]
    pub max: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_area: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_areas: Option<u64>,
}

/// Typed `loop_context` payload used by the strategic loop.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StrategicLoopContext {
    pub strategic: StrategicContext,
}

/// A single improvement area from a strategic assessment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AreaEntry {
    pub area: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub severity: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    /// Captures any additional fields the AI assessment may include.
    #[serde(flatten)]
    pub extra: std::collections::HashMap<String, serde_json::Value>,
}

// ---------------------------------------------------------------------------
// Scatter/gather coordination payloads
// ---------------------------------------------------------------------------

/// One completed child of a gather group, as recorded for the reduce step.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatheredChild {
    /// The `id` of the child's completion event.
    pub event_id: String,
    /// The completion event's type.
    pub event_type: crate::event::EventType,
    /// The completion event's project.
    pub project: String,
    /// The `success` flag read from the completion payload, if it carried
    /// one. Foundry payloads report boolean results under the `success` key.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub success: Option<bool>,
    /// The completion event's full payload.
    pub payload: serde_json::Value,
}

/// Payload for the reduce event the engine synthesizes when a gather group
/// is satisfied.
///
/// A reduce block sinks on the gather's `reduce_event_type` and parses this
/// payload to decide what the mix of child outcomes means. The engine never
/// interprets child success/failure itself — it only counts arrivals.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatherCompletedPayload {
    /// The fan-out group this reduce belongs to.
    pub gather_id: String,
    /// How many children were scattered.
    pub expected: usize,
    /// How many completions had arrived when the policy was satisfied.
    pub arrived: usize,
    /// Verbatim context supplied by the scattering block via `GatherSpec`.
    pub context: serde_json::Value,
    /// The completed children, in arrival order.
    pub children: Vec<GatheredChild>,
}

#[cfg(test)]
mod tests {
    use super::{ChainContext, LoopContext};

    /// `campaign_cycle` must survive every hop `campaign` survives, or an event
    /// can name its campaign but not which attempt produced it.
    #[test]
    fn chain_context_carries_the_campaign_cycle_wherever_it_carries_the_campaign() {
        let source = serde_json::json!({
            "campaign": "c",
            "campaign_cycle": 3,
            "agent_provider": "codex",
        });

        let extracted = ChainContext::extract_from(&source);
        assert_eq!(extracted.campaign.as_deref(), Some("c"));
        assert_eq!(extracted.campaign_cycle, Some(3));

        let mut target = serde_json::json!({ "project": "p" });
        extracted.merge_into(&mut target);
        assert_eq!(target["campaign"], "c");
        assert_eq!(
            target["campaign_cycle"], 3,
            "merge_into dropped the cycle, so it would vanish at the first chain hop"
        );
    }

    #[test]
    fn loop_context_extracts_the_campaign_cycle() {
        let source = serde_json::json!({ "campaign": "c", "campaign_cycle": 2 });
        let extracted = LoopContext::extract_from(&source);
        assert_eq!(extracted.campaign.as_deref(), Some("c"));
        assert_eq!(extracted.campaign_cycle, Some(2));
    }

    /// A non-campaign task has no cycle, and the field must stay absent rather
    /// than serialize as a misleading zero.
    #[test]
    fn absent_campaign_cycle_is_omitted_entirely() {
        let context = LoopContext::default();
        let json = serde_json::to_value(&context).unwrap();
        assert!(json.get("campaign_cycle").is_none());

        let extracted = LoopContext::extract_from(&serde_json::json!({ "project": "p" }));
        assert_eq!(extracted.campaign_cycle, None);
    }

    /// Events written before this field existed must keep deserializing.
    #[test]
    fn context_without_a_campaign_cycle_still_deserializes() {
        let legacy = serde_json::json!({ "campaign": "c", "task_branch": "foundry-task/x" });
        let context: LoopContext = serde_json::from_value(legacy).unwrap();
        assert_eq!(context.campaign.as_deref(), Some("c"));
        assert_eq!(context.campaign_cycle, None);
    }
}
