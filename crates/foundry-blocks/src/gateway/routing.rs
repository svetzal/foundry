//! `RoutingAgentGateway` — dispatches an [`AgentRequest`] to a concrete agent
//! backend based on the request's per-request [`AgentProvider`] override,
//! falling back to a process-level default.
//!
//! This is the single [`AgentGateway`] the daemon clones into every task block.
//! Blocks stay provider-agnostic: they build an `AgentRequest` (optionally
//! carrying a `provider` pulled from the run's chain context) and call
//! `invoke`. The router is the *one* place that knows the full provider set, so
//! adding a backend is a one-line registration here plus a new gateway impl —
//! no block changes.
//!
//! Selection precedence: `request.provider` → router default → error if neither
//! resolves to a registered backend.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, RwLock};

use anyhow::{Result, anyhow};

use super::{AgentFailureMetadata, AgentGateway, AgentProvider, AgentRequest, AgentResponse};

/// An [`AgentGateway`] that routes to one of several registered backends.
pub struct RoutingAgentGateway {
    gateways: HashMap<AgentProvider, Arc<dyn AgentGateway>>,
    default: AgentProvider,
    breakers: Arc<RwLock<HashMap<AgentProvider, AgentFailureMetadata>>>,
}

impl RoutingAgentGateway {
    /// Create a router with the given backends and default provider.
    ///
    /// The `default` is used when a request carries no `provider` override (or
    /// carries one with no registered backend). It should itself be present in
    /// `gateways`; if it is not, requests that fall through to it fail with an
    /// "no agent gateway registered" error at invoke time.
    pub fn new(
        gateways: HashMap<AgentProvider, Arc<dyn AgentGateway>>,
        default: AgentProvider,
    ) -> Self {
        Self {
            gateways,
            default,
            breakers: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Resolve which provider a request should run on: its override if present,
    /// otherwise the router default.
    fn resolve(&self, request: &AgentRequest) -> AgentProvider {
        request.provider.unwrap_or(self.default)
    }

    fn breaker_for(&self, provider: AgentProvider) -> Option<AgentFailureMetadata> {
        self.breakers.read().ok()?.get(&provider).cloned()
    }

    fn open_breaker(&self, provider: AgentProvider, failure: &AgentFailureMetadata) {
        if !failure.is_terminal_provider_failure() {
            return;
        }
        if let Ok(mut breakers) = self.breakers.write() {
            breakers.insert(provider, failure.clone());
        }
    }
}

impl AgentGateway for RoutingAgentGateway {
    fn invoke<'a>(
        &'a self,
        request: &'a AgentRequest,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<AgentResponse>> + Send + 'a>> {
        let requested = self.resolve(request);
        // Fall back to the default backend if the requested one isn't registered.
        let (provider, gateway) = if let Some(g) = self.gateways.get(&requested) {
            (requested, Some(Arc::clone(g)))
        } else {
            tracing::warn!(
                requested = %requested,
                default = %self.default,
                "requested agent provider not registered; falling back to default"
            );
            (self.default, self.gateways.get(&self.default).map(Arc::clone))
        };
        Box::pin(async move {
            match gateway {
                Some(gateway) => {
                    if let Some(open_failure) = self.breaker_for(provider) {
                        tracing::warn!(
                            provider = %provider,
                            project = %request.project,
                            summary = %open_failure.stop_summary(),
                            "agent provider circuit breaker is open; skipping invocation"
                        );
                        let message = match open_failure.message.as_deref() {
                            Some(message) if !message.trim().is_empty() => {
                                format!("{message} (circuit breaker open for provider {provider})")
                            }
                            _ => format!(
                                "{} for provider {provider}; circuit breaker is open",
                                open_failure.summary_label()
                            ),
                        };
                        let failure = AgentFailureMetadata {
                            message: Some(message.clone()),
                            ..open_failure
                        };
                        return Ok(AgentResponse::failure_with_metadata(message, 1, failure));
                    }

                    tracing::debug!(provider = %provider, project = %request.project, "routing agent invocation");
                    let response = gateway.invoke(request).await?;
                    if let Some(ref failure) = response.failure {
                        self.open_breaker(provider, failure);
                    }
                    Ok(response)
                }
                None => Err(anyhow!(
                    "no agent gateway registered for provider '{provider}' (and no default available)"
                )),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::fakes::FakeAgentGateway;
    use foundry_sdk::gateway::{
        AgentAccess, AgentFailureKind, AgentFailureMetadata, ModelTier, ReasoningEffort,
    };
    use std::path::PathBuf;
    use std::time::Duration;

    fn request_with(provider: Option<AgentProvider>) -> AgentRequest {
        AgentRequest {
            prompt: "hi".to_string(),
            project: "demo".to_string(),
            working_dir: PathBuf::from("/tmp"),
            access: AgentAccess::Full,
            tier: ModelTier::Balanced,
            effort: ReasoningEffort::Medium,
            agent_file: None,
            provider,
            env: Vec::new(),
            timeout: Duration::from_secs(5),
        }
    }

    fn router_with_all() -> (
        RoutingAgentGateway,
        Arc<FakeAgentGateway>,
        Arc<FakeAgentGateway>,
        Arc<FakeAgentGateway>,
    ) {
        let claude = FakeAgentGateway::success_with("from-claude");
        let opencode = FakeAgentGateway::success_with("from-opencode");
        let codex = FakeAgentGateway::success_with("from-codex");
        let mut map: HashMap<AgentProvider, Arc<dyn AgentGateway>> = HashMap::new();
        map.insert(AgentProvider::Claude, claude.clone());
        map.insert(AgentProvider::Opencode, opencode.clone());
        map.insert(AgentProvider::Codex, codex.clone());
        (RoutingAgentGateway::new(map, AgentProvider::Claude), claude, opencode, codex)
    }

    #[tokio::test]
    async fn routes_to_default_when_no_override() {
        let (router, _claude, _oc, _cx) = router_with_all();
        let resp = router.invoke(&request_with(None)).await.unwrap();
        assert_eq!(resp.stdout, "from-claude");
    }

    #[tokio::test]
    async fn routes_to_requested_override() {
        let (router, _claude, _oc, _cx) = router_with_all();
        let resp = router.invoke(&request_with(Some(AgentProvider::Codex))).await.unwrap();
        assert_eq!(resp.stdout, "from-codex");

        let resp = router.invoke(&request_with(Some(AgentProvider::Opencode))).await.unwrap();
        assert_eq!(resp.stdout, "from-opencode");
    }

    #[tokio::test]
    async fn override_only_hits_the_selected_backend() {
        let (router, claude, opencode, codex) = router_with_all();
        let _ = router.invoke(&request_with(Some(AgentProvider::Codex))).await.unwrap();
        assert_eq!(claude.invocations().len(), 0);
        assert_eq!(opencode.invocations().len(), 0);
        assert_eq!(codex.invocations().len(), 1);
    }

    #[tokio::test]
    async fn falls_back_to_default_when_requested_unregistered() {
        // Only claude registered; request codex → falls back to claude default.
        let claude = FakeAgentGateway::success_with("from-claude");
        let mut map: HashMap<AgentProvider, Arc<dyn AgentGateway>> = HashMap::new();
        map.insert(AgentProvider::Claude, claude.clone());
        let router = RoutingAgentGateway::new(map, AgentProvider::Claude);

        let resp = router.invoke(&request_with(Some(AgentProvider::Codex))).await.unwrap();
        assert_eq!(resp.stdout, "from-claude");
        assert_eq!(claude.invocations().len(), 1);
    }

    #[tokio::test]
    async fn errors_when_no_backend_and_default_missing() {
        let map: HashMap<AgentProvider, Arc<dyn AgentGateway>> = HashMap::new();
        let router = RoutingAgentGateway::new(map, AgentProvider::Claude);
        let result = router.invoke(&request_with(None)).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn routing_circuit_breaker_skips_subsequent_calls_after_terminal_failure() {
        let terminal_failure = AgentFailureMetadata::new(AgentProvider::Claude)
            .terminal(AgentFailureKind::AccountLimit)
            .with_api_error_status(429)
            .with_message(
                "You've hit your monthly spend limit - raise it at claude.ai/settings/usage",
            );
        let claude = FakeAgentGateway::sequence(vec![AgentResponse::failure_with_metadata(
            "monthly spend limit",
            1,
            terminal_failure,
        )]);
        let mut map: HashMap<AgentProvider, Arc<dyn AgentGateway>> = HashMap::new();
        map.insert(AgentProvider::Claude, claude.clone());
        let router = RoutingAgentGateway::new(map, AgentProvider::Claude);

        let first = router.invoke(&request_with(None)).await.unwrap();
        assert!(!first.success);
        assert_eq!(
            first.failure.as_ref().and_then(|failure| failure.failure_kind),
            Some(AgentFailureKind::AccountLimit)
        );

        let second = router.invoke(&request_with(None)).await.unwrap();
        assert!(!second.success);
        assert_eq!(claude.invocations().len(), 1, "second call should be skipped");
        assert_eq!(
            second.failure.as_ref().and_then(|failure| failure.failure_kind),
            Some(AgentFailureKind::AccountLimit)
        );
        assert!(
            second
                .failure
                .as_ref()
                .and_then(|failure| failure.message.as_deref())
                .is_some_and(|message| message.contains("circuit breaker open")),
            "breaker-open message should be explicit"
        );
    }
}
