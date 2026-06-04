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
    ) -> serde_json::Value {
        let mut payload = serde_json::json!({
            "release": self.release_type,
            "new_tag": new_tag,
            "success": success,
        });

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

impl OutputMapper<ReleaseOutput> for ReleaseOutputMapper {
    fn map(&self, output: ReleaseOutput, trigger: &Event) -> TaskBlockResult {
        let payload = self.build_payload(trigger, output.success, output.new_tag.as_ref());

        TaskBlockResult {
            events: vec![Event::new(
                EventType::ReleaseCompleted,
                trigger.project.clone(),
                trigger.throttle,
                payload,
            )],
            success: output.success,
            summary: output.summary,
            raw_output: output.raw_output,
            exit_code: output.exit_code,
            ..Default::default()
        }
    }

    fn dry_run_events(&self, trigger: &Event) -> Vec<Event> {
        let mut payload = serde_json::json!({
            "release": self.release_type,
            "success": true,
            "dry_run": true,
        });

        if let Some(extra) = &self.extra_payload
            && let (Some(base), Some(extra)) = (payload.as_object_mut(), extra(trigger).as_object())
        {
            for (k, v) in extra {
                base.insert(k.clone(), v.clone());
            }
        }

        vec![Event::new(
            EventType::ReleaseCompleted,
            trigger.project.clone(),
            trigger.throttle,
            payload,
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
        // Respect the self-filter: skip when dirty.
        let dirty =
            trigger.parse_payload::<MainBranchAuditedPayload>().ok().is_none_or(|p| p.dirty);
        if dirty {
            return vec![];
        }
        self.inner.dry_run_events(trigger)
    }
}
