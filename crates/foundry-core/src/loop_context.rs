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
