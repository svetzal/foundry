/// Generates `name()`, `kind()`, and `sinks_on()` method bodies inside an
/// `impl TaskBlock for X { ... }` block.
///
/// # Usage
///
/// ```ignore
/// impl TaskBlock for MyBlock {
///     task_block_meta! {
///         name: "My Block",
///         kind: Observer,
///         sinks_on: [SomeEvent, AnotherEvent],
///     }
///
///     fn execute(&self, trigger: &Event) -> ... { ... }
/// }
/// ```
macro_rules! task_block_meta {
    (name: $name:expr, kind: $kind:ident, sinks_on: [$($event:ident),+ $(,)?] $(,)?) => {
        fn name(&self) -> &'static str {
            $name
        }

        fn kind(&self) -> BlockKind {
            BlockKind::$kind
        }

        fn sinks_on(&self) -> &[EventType] {
            &[$(EventType::$event),+]
        }
    };
}

/// Early-return a deserialized payload from the trigger event.
///
/// Expands to a `match` that returns the deserialized value on success or
/// propagates the parse error as `Err(e)` (wrapped in a `Box::pin(async move)`)
/// on failure.  Must be called inside `execute()` before the `Box::pin(async
/// move { … })` boundary.
///
/// # Usage
///
/// ```ignore
/// let p = parse_payload!(trigger, MyPayload);
/// ```
macro_rules! parse_payload {
    ($trigger:expr, $Payload:ty) => {
        match $trigger.parse_payload::<$Payload>() {
            Ok(p) => p,
            Err(e) => return Box::pin(async move { Err(e) }),
        }
    };
}

/// Early-return a project registry lookup from `self.registry`.
///
/// Expands to a `match` that returns the `ProjectEntry` on success or
/// returns `Ok(result)` (a not-found failure `TaskBlockResult`) on failure.
/// Requires `self.registry` and that the calling module has `require_project`
/// visible as `super::require_project`.
///
/// # Usage
///
/// ```ignore
/// let entry = require_project!(self, project);
/// ```
macro_rules! require_project {
    ($self:expr, $project:expr) => {
        match super::require_project(&$self.registry, &$project) {
            Ok(e) => e,
            Err(result) => return Box::pin(async { Ok(result) }),
        }
    };
}

/// Return a skipped-success result from `execute()`.
///
/// Expands to `Box::pin(async { Ok(TaskBlockResult::success(msg, vec![])) })`.
/// Use with `return skip!(...)`.
///
/// # Usage
///
/// ```ignore
/// return skip!("Skipped: not applicable");
/// ```
macro_rules! skip {
    ($msg:expr) => {
        Box::pin(async { Ok(foundry_core::task_block::TaskBlockResult::success($msg, vec![])) })
    };
}

/// Generates a struct definition with `registry` and `agent` fields, and a
/// `pub fn new(agent, registry)` constructor for the common "registry + injected
/// agent" pattern used by blocks that call an `AgentGateway`.
///
/// # Usage
///
/// ```ignore
/// agent_block_new!(pub struct MyBlock);
/// ```
///
/// Expands to:
/// - `pub struct MyBlock { registry: Arc<Registry>, agent: Arc<dyn AgentGateway> }`
/// - `impl MyBlock { pub fn new(agent: Arc<dyn AgentGateway>, registry: Arc<Registry>) -> Self }`
///
/// Requires `Registry` and `AgentGateway` to be in scope at the call site.
macro_rules! agent_block_new {
    ($(#[$meta:meta])* $vis:vis struct $name:ident) => {
        $(#[$meta])*
        $vis struct $name {
            registry: ::std::sync::Arc<Registry>,
            agent: ::std::sync::Arc<dyn AgentGateway>,
        }

        impl $name {
            pub fn new(
                agent: ::std::sync::Arc<dyn AgentGateway>,
                registry: ::std::sync::Arc<Registry>,
            ) -> Self {
                Self { registry, agent }
            }
        }
    };
}

/// Build a test `Event` with `Throttle::Full` from an inline JSON literal.
///
/// Replaces the 10–15-line per-module trigger factory functions.
///
/// # Usage
/// ```ignore
/// let trigger = test_event!(EventType::PlanCompleted, "my-project", {
///     "project": "my-project",
///     "plan": "1. Extract helper",
///     "principle": "DRY",
///     "workflow": "iterate",
/// });
/// ```
#[cfg(test)]
macro_rules! test_event {
    ($event_type:expr, $project:expr, { $($json:tt)* }) => {
        foundry_core::event::Event::new(
            $event_type,
            $project.to_string(),
            foundry_core::throttle::Throttle::Full,
            serde_json::json!({ $($json)* }),
        )
    };
}

/// Generate `kind_is` and `sinks_on_expected` property tests for a `TaskBlock`.
///
/// Invoke at module level inside a `#[cfg(test)] mod tests { ... }` block.
/// The `$block_expr` is evaluated once per generated test.
///
/// # Usage
/// ```ignore
/// assert_block_meta!(
///     ExecutePlan::new(FakeAgentGateway::success(), empty_registry()),
///     kind: Mutator,
///     sinks_on: [PlanCompleted],
/// );
/// ```
#[cfg(test)]
macro_rules! assert_block_meta {
    (
        $block_expr:expr,
        kind: $kind:ident,
        sinks_on: [$($event:ident),+ $(,)?] $(,)?
    ) => {
        #[test]
        fn kind_is() {
            assert_eq!(
                { $block_expr }.kind(),
                foundry_core::task_block::BlockKind::$kind,
            );
        }

        #[test]
        fn sinks_on_expected() {
            assert_eq!(
                { $block_expr }.sinks_on(),
                &[$(foundry_core::event::EventType::$event),+],
            );
        }
    };
}

/// Generates a struct definition with `registry` and one or more gateway fields,
/// a `pub fn new(registry)` constructor that wires the production gateway
/// defaults, and a `#[cfg(test)]` test constructor.
///
/// **Single-gateway form** — test constructor is named `with_gateways(registry, gw)`:
///
/// ```ignore
/// task_block_new! {
///     pub struct MyBlock {
///         shell: ShellGateway = crate::gateway::ProcessShellGateway
///     }
/// }
/// ```
///
/// **Multi-gateway form** — test constructor is `with_gateways(registry, gw1, gw2, ...)`:
///
/// ```ignore
/// task_block_new! {
///     pub struct MyBlock {
///         shell: ShellGateway = crate::gateway::ProcessShellGateway,
///         scanner: ScannerGateway = crate::gateway::ProcessScannerGateway,
///     }
/// }
/// ```
///
/// Both forms expand to:
/// - `pub struct MyBlock { registry: Arc<Registry>, field: Arc<dyn Trait>, ... }`
/// - `impl MyBlock { pub fn new(registry) -> Self { ... } }`
/// - `#[cfg(test)] fn with_gateways(registry, field, ...) -> Self { ... }`
///
/// Intended for blocks that follow the "registry + gateways with production
/// defaults" pattern. Blocks with non-standard constructors or extra constructor
/// logic should remain hand-written.
macro_rules! task_block_new {
    // Single-gateway variant — test constructor named `with_gateways`.
    (
        $(#[$meta:meta])*
        $vis:vis struct $name:ident {
            $gw_field:ident: $gw_trait:path = $gw_default:expr $(,)?
        }
    ) => {
        $(#[$meta])*
        $vis struct $name {
            registry: ::std::sync::Arc<Registry>,
            $gw_field: ::std::sync::Arc<dyn $gw_trait>,
        }

        impl $name {
            pub fn new(registry: ::std::sync::Arc<Registry>) -> Self {
                Self {
                    registry,
                    $gw_field: ::std::sync::Arc::new($gw_default),
                }
            }

            #[cfg(test)]
            fn with_gateways(
                registry: ::std::sync::Arc<Registry>,
                $gw_field: ::std::sync::Arc<dyn $gw_trait>,
            ) -> Self {
                Self { registry, $gw_field }
            }
        }
    };

    // Multi-gateway variant (2+ gateways) — test constructor named `with_gateways`.
    (
        $(#[$meta:meta])*
        $vis:vis struct $name:ident {
            $($gw_field:ident: $gw_trait:path = $gw_default:expr),+ $(,)?
        }
    ) => {
        $(#[$meta])*
        $vis struct $name {
            registry: ::std::sync::Arc<Registry>,
            $($gw_field: ::std::sync::Arc<dyn $gw_trait>),+
        }

        impl $name {
            pub fn new(registry: ::std::sync::Arc<Registry>) -> Self {
                Self {
                    registry,
                    $($gw_field: ::std::sync::Arc::new($gw_default)),+
                }
            }

            #[cfg(test)]
            fn with_gateways(
                registry: ::std::sync::Arc<Registry>,
                $($gw_field: ::std::sync::Arc<dyn $gw_trait>),+
            ) -> Self {
                Self { registry, $($gw_field),+ }
            }
        }
    };
}
