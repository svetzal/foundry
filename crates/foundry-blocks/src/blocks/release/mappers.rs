use foundry_sdk::event::{Event, EventType};
use foundry_sdk::payload::MainBranchAuditedPayload;
use foundry_sdk::task_block::TaskBlockResult;
use foundry_sdk::work_block::OutputMapper;

use super::ReleaseOutput;

/// Closure type for producing extra payload fields from a trigger event.
type ExtraPayloadFn = Box<dyn Fn(&Event) -> serde_json::Value + Send + Sync>;

/// Maps [`ReleaseOutput`] into a [`TaskBlockResult`] with a `ReleaseCompleted` event.
///
/// Parameterized with `release_type` (e.g. "patch" or "manual") and optional
/// extra payload fields (e.g. CVE for vulnerability releases).
pub struct ReleaseOutputMapper {
    release_type: &'static str,
    /// Extra payload fields merged into every `ReleaseCompleted` event.
    extra_payload: Option<ExtraPayloadFn>,
}

impl ReleaseOutputMapper {
    pub fn new(release_type: &'static str) -> Self {
        Self {
            release_type,
            extra_payload: None,
        }
    }

    #[must_use]
    pub fn with_extra_payload(
        mut self,
        f: impl Fn(&Event) -> serde_json::Value + Send + Sync + 'static,
    ) -> Self {
        self.extra_payload = Some(Box::new(f));
        self
    }

    fn build_payload(
        &self,
        trigger: &Event,
        success: bool,
        new_tag: Option<&String>,
        dry_run: Option<bool>,
    ) -> serde_json::Value {
        let mut payload = serde_json::json!({
            "release": self.release_type,
            "new_tag": new_tag,
            "success": success,
        });

        if let Some(dr) = dry_run
            && let Some(base) = payload.as_object_mut()
        {
            base.insert("dry_run".to_string(), serde_json::json!(dr));
        }

        if let Some(extra) = &self.extra_payload
            && let (Some(base), Some(extra)) = (payload.as_object_mut(), extra(trigger).as_object())
        {
            for (k, v) in extra {
                base.insert(k.clone(), v.clone());
            }
        }

        payload
    }
}

fn release_completed_event(trigger: &Event, payload: serde_json::Value) -> Event {
    Event::new(EventType::ReleaseCompleted, trigger.project.clone(), trigger.throttle, payload)
}

impl OutputMapper<ReleaseOutput> for ReleaseOutputMapper {
    fn map(&self, output: ReleaseOutput, trigger: &Event) -> TaskBlockResult {
        let payload = self.build_payload(trigger, output.success, output.new_tag.as_ref(), None);

        TaskBlockResult {
            events: vec![release_completed_event(trigger, payload)],
            success: output.success,
            summary: output.summary,
            raw_output: output.raw_output,
            exit_code: output.exit_code,
            ..Default::default()
        }
    }

    fn dry_run_events(&self, trigger: &Event) -> Vec<Event> {
        vec![release_completed_event(
            trigger,
            self.build_payload(trigger, true, None, Some(true)),
        )]
    }
}

/// Dry-run mapper for the vulnerability release path that respects
/// the `dirty` self-filter — emits no events when dirty.
pub struct VulnReleaseMapper {
    inner: ReleaseOutputMapper,
}

impl VulnReleaseMapper {
    pub fn new() -> Self {
        Self {
            inner: ReleaseOutputMapper::new("patch").with_extra_payload(|trigger| {
                let cve = trigger
                    .parse_payload::<MainBranchAuditedPayload>()
                    .ok()
                    .map_or_else(|| "unknown".to_string(), |p| p.cve);
                serde_json::json!({ "cve": cve })
            }),
        }
    }
}

impl OutputMapper<ReleaseOutput> for VulnReleaseMapper {
    fn map(&self, output: ReleaseOutput, trigger: &Event) -> TaskBlockResult {
        self.inner.map(output, trigger)
    }

    fn dry_run_events(&self, trigger: &Event) -> Vec<Event> {
        if super::skips_when_dirty(trigger) {
            return vec![];
        }
        self.inner.dry_run_events(trigger)
    }
}

#[cfg(test)]
mod tests {
    use foundry_sdk::event::{Event, EventType};
    use foundry_sdk::throttle::Throttle;
    use foundry_sdk::work_block::OutputMapper;

    use super::*;

    fn make_trigger(payload: serde_json::Value) -> Event {
        Event::new(
            EventType::MainBranchAudited,
            "test-project".to_string(),
            Throttle::Full,
            payload,
        )
    }

    fn successful_output(tag: Option<&str>) -> ReleaseOutput {
        ReleaseOutput {
            success: true,
            new_tag: tag.map(str::to_string),
            summary: "Release done".to_string(),
            raw_output: None,
            exit_code: Some(0),
        }
    }

    fn failed_output() -> ReleaseOutput {
        ReleaseOutput {
            success: false,
            new_tag: None,
            summary: "Release failed".to_string(),
            raw_output: Some("stderr output".to_string()),
            exit_code: Some(1),
        }
    }

    // --- ReleaseOutputMapper ---

    #[test]
    fn map_emits_release_completed_event() {
        let mapper = ReleaseOutputMapper::new("patch");
        let trigger = make_trigger(serde_json::json!({}));
        let result = mapper.map(successful_output(Some("v1.2.3")), &trigger);

        assert!(result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::ReleaseCompleted);
        assert_eq!(result.events[0].project, "test-project");
    }

    #[test]
    fn map_payload_contains_release_type_and_tag() {
        let mapper = ReleaseOutputMapper::new("patch");
        let trigger = make_trigger(serde_json::json!({}));
        let result = mapper.map(successful_output(Some("v1.2.3")), &trigger);

        assert_eq!(result.events[0].payload["release"], "patch");
        assert_eq!(result.events[0].payload["new_tag"], "v1.2.3");
        assert_eq!(result.events[0].payload["success"], true);
    }

    #[test]
    fn map_payload_reflects_failure() {
        let mapper = ReleaseOutputMapper::new("manual");
        let trigger = make_trigger(serde_json::json!({}));
        let result = mapper.map(failed_output(), &trigger);

        assert!(!result.success);
        assert_eq!(result.events[0].payload["success"], false);
        assert!(result.events[0].payload["new_tag"].is_null());
    }

    #[test]
    fn dry_run_events_emits_release_completed_with_dry_run_flag() {
        let mapper = ReleaseOutputMapper::new("manual");
        let trigger = make_trigger(serde_json::json!({}));
        let events = mapper.dry_run_events(&trigger);

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, EventType::ReleaseCompleted);
        assert_eq!(events[0].payload["dry_run"], true);
        assert_eq!(events[0].payload["success"], true);
        assert_eq!(events[0].payload["release"], "manual");
    }

    #[test]
    fn with_extra_payload_merges_fields_into_map_result() {
        let mapper = ReleaseOutputMapper::new("patch")
            .with_extra_payload(|_| serde_json::json!({ "cve": "CVE-2026-1234" }));
        let trigger = make_trigger(serde_json::json!({}));
        let result = mapper.map(successful_output(None), &trigger);

        assert_eq!(result.events[0].payload["cve"], "CVE-2026-1234");
        assert_eq!(result.events[0].payload["release"], "patch");
    }

    #[test]
    fn with_extra_payload_merges_fields_into_dry_run_events() {
        let mapper = ReleaseOutputMapper::new("patch")
            .with_extra_payload(|_| serde_json::json!({ "extra": "value" }));
        let trigger = make_trigger(serde_json::json!({}));
        let events = mapper.dry_run_events(&trigger);

        assert_eq!(events[0].payload["extra"], "value");
        assert_eq!(events[0].payload["dry_run"], true);
    }

    #[test]
    fn dry_run_and_map_agree_on_release_type_and_extra_fields() {
        let mapper = ReleaseOutputMapper::new("patch")
            .with_extra_payload(|_| serde_json::json!({ "cve": "CVE-2026-0001" }));
        let trigger = make_trigger(serde_json::json!({}));

        let map_result = mapper.map(successful_output(Some("v1.0.0")), &trigger);
        let dry_events = mapper.dry_run_events(&trigger);

        assert_eq!(map_result.events.len(), 1);
        assert_eq!(dry_events.len(), 1);

        // Both paths must produce the same event type.
        assert_eq!(map_result.events[0].event_type, EventType::ReleaseCompleted);
        assert_eq!(dry_events[0].event_type, EventType::ReleaseCompleted);

        // Both must carry the same release type.
        assert_eq!(map_result.events[0].payload["release"], "patch");
        assert_eq!(dry_events[0].payload["release"], "patch");

        // Both must carry extra fields from the shared build_payload path.
        assert_eq!(map_result.events[0].payload["cve"], "CVE-2026-0001");
        assert_eq!(dry_events[0].payload["cve"], "CVE-2026-0001");

        // Only dry_run carries the dry_run flag; real map does not.
        assert!(map_result.events[0].payload.get("dry_run").is_none_or(|v| v.is_null()));
        assert_eq!(dry_events[0].payload["dry_run"], true);
    }

    // --- VulnReleaseMapper ---

    #[test]
    fn vuln_mapper_map_emits_release_completed_with_cve() {
        let mapper = VulnReleaseMapper::new();
        let trigger = make_trigger(
            serde_json::json!({ "cve": "CVE-2026-9999", "dirty": false, "vulnerable": true }),
        );
        let result = mapper.map(successful_output(Some("v0.1.1")), &trigger);

        assert!(result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::ReleaseCompleted);
        assert_eq!(result.events[0].payload["cve"], "CVE-2026-9999");
        assert_eq!(result.events[0].payload["release"], "patch");
    }

    #[test]
    fn vuln_dry_run_events_skips_when_dirty() {
        let mapper = VulnReleaseMapper::new();
        let trigger = make_trigger(serde_json::json!({ "dirty": true, "cve": "CVE-2026-1234" }));
        let events = mapper.dry_run_events(&trigger);
        assert!(events.is_empty());
    }

    #[test]
    fn adapter_filter_and_dry_run_events_both_skip_for_dirty_trigger() {
        let dirty_trigger =
            make_trigger(serde_json::json!({ "dirty": true, "cve": "CVE-2026-0001" }));
        assert!(
            super::super::skips_when_dirty(&dirty_trigger),
            "skips_when_dirty must return true for dirty trigger"
        );
        let mapper = VulnReleaseMapper::new();
        assert!(
            mapper.dry_run_events(&dirty_trigger).is_empty(),
            "dry_run_events must skip for dirty trigger"
        );
    }

    #[test]
    fn vuln_dry_run_events_emits_when_clean() {
        let mapper = VulnReleaseMapper::new();
        let trigger = make_trigger(
            serde_json::json!({ "dirty": false, "cve": "CVE-2026-1234", "vulnerable": false }),
        );
        let events = mapper.dry_run_events(&trigger);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, EventType::ReleaseCompleted);
        assert_eq!(events[0].payload["dry_run"], true);
    }

    #[test]
    fn vuln_dry_run_events_emits_when_payload_defaults_to_clean() {
        // `{}` with all #[serde(default)] fields → dirty=false → events emitted
        let mapper = VulnReleaseMapper::new();
        let trigger = make_trigger(serde_json::json!({}));
        let events = mapper.dry_run_events(&trigger);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].payload["dry_run"], true);
    }
}
