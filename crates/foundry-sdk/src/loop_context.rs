/// Forward a specific set of named fields from `source` to `target`.
///
/// Only copies fields that are present in `source` — fields absent from
/// `source` leave `target` unchanged.
pub fn forward_payload_fields(
    source: &serde_json::Value,
    target: &mut serde_json::Value,
    keys: &[&str],
) {
    for key in keys {
        if let Some(val) = source.get(*key) {
            target[*key] = val.clone();
        }
    }
}

/// Forward all standard chain-context fields from `source` to `target`.
///
/// Copies `actions`, `prompt`, `gates`, and `audit_name` (if present), then
/// calls [`forward_loop_context`] to carry nested-loop state. Use this in
/// every block that constructs event payloads and needs to propagate the
/// full iteration context to downstream blocks.
pub fn forward_chain_context(source: &serde_json::Value, target: &mut serde_json::Value) {
    forward_payload_fields(source, target, &["actions", "prompt", "gates", "audit_name"]);
    forward_loop_context(source, target);
}

/// Forward `loop_context` from a trigger payload to an emitted payload.
///
/// No-op if the source has no `loop_context` field. This must be called
/// by every block in the iterate chain that constructs event payloads, so
/// that loop state survives the full chain from entry to completion.
pub fn forward_loop_context(source: &serde_json::Value, target: &mut serde_json::Value) {
    if let Some(ctx) = source.get("loop_context") {
        target["loop_context"] = ctx.clone();
    }
}

/// Returns `true` if the payload carries a `loop_context`, indicating
/// this event is part of a nested loop rather than a standalone run.
pub fn has_loop_context(payload: &serde_json::Value) -> bool {
    payload.get("loop_context").is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forward_payload_fields_copies_present_keys() {
        let source = serde_json::json!({
            "actions": {"maintain": true},
            "prompt": "do the thing",
            "gates": [],
        });
        let mut target = serde_json::json!({ "project": "test" });

        forward_payload_fields(&source, &mut target, &["actions", "prompt", "audit_name"]);

        assert_eq!(target["actions"]["maintain"], true, "actions should be copied");
        assert_eq!(target["prompt"], "do the thing", "prompt should be copied");
        assert!(
            target.get("audit_name").is_none(),
            "audit_name absent in source should not appear"
        );
        assert!(target.get("gates").is_none(), "gates not in key list should not be copied");
    }

    #[test]
    fn forward_chain_context_copies_all_standard_fields_and_loop_context() {
        let source = serde_json::json!({
            "actions": {"maintain": true},
            "prompt": "do the thing",
            "gates": [{"name": "fmt"}],
            "audit_name": "fix-drY",
            "loop_context": {"strategic": {"iteration": 2}},
            "unrelated": "noise",
        });
        let mut target = serde_json::json!({ "project": "test" });

        forward_chain_context(&source, &mut target);

        assert_eq!(target["actions"]["maintain"], true);
        assert_eq!(target["prompt"], "do the thing");
        assert_eq!(target["gates"][0]["name"], "fmt");
        assert_eq!(target["audit_name"], "fix-drY");
        assert_eq!(target["loop_context"]["strategic"]["iteration"], 2);
        assert!(target.get("unrelated").is_none(), "unrelated fields must not be copied");
    }

    #[test]
    fn forward_chain_context_is_noop_when_fields_absent() {
        let source = serde_json::json!({ "project": "test" });
        let mut target = serde_json::json!({ "project": "test" });

        forward_chain_context(&source, &mut target);

        assert!(target.get("actions").is_none());
        assert!(target.get("loop_context").is_none());
    }

    #[test]
    fn forward_copies_loop_context_when_present() {
        let source = serde_json::json!({
            "project": "test",
            "loop_context": {
                "strategic": { "iteration": 1, "max": 5 }
            }
        });
        let mut target = serde_json::json!({ "project": "test" });

        forward_loop_context(&source, &mut target);

        assert_eq!(
            target["loop_context"]["strategic"]["iteration"], 1,
            "loop_context should be copied to target"
        );
    }

    #[test]
    fn forward_is_noop_when_absent() {
        let source = serde_json::json!({ "project": "test" });
        let mut target = serde_json::json!({ "project": "test" });

        forward_loop_context(&source, &mut target);

        assert!(
            target.get("loop_context").is_none(),
            "target should not gain loop_context when source lacks it"
        );
    }

    #[test]
    fn has_loop_context_returns_true_when_present() {
        let payload = serde_json::json!({
            "loop_context": { "strategic": { "iteration": 1 } }
        });
        assert!(has_loop_context(&payload));
    }

    #[test]
    fn has_loop_context_returns_false_when_absent() {
        let payload = serde_json::json!({ "project": "test" });
        assert!(!has_loop_context(&payload));
    }
}
