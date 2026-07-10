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
            Err(e) => return Box::pin(async move { Err(e.into()) }),
        }
    };
}

/// Early-return a project registry lookup from `self.registry`.
///
/// Acquires a short-lived read guard, clones the entry, and releases the guard
/// before the `Box::pin(async move { ... })` boundary so the lock is never held
/// across an `.await` point.
///
/// Expands to a `match` that returns the `ProjectEntry` on success or
/// returns `Ok(result)` (a not-found failure `TaskBlockResult`) on failure.
/// Requires `self.registry: Arc<RwLock<Registry>>` and that the calling module
/// has `require_project` visible as `super::require_project`.
///
/// **Design note — `execute()`-resident domain skip**: the "project not in registry" path
/// returns `TaskBlockResult::project_not_found`, which is an honest block-level failure result
/// rather than a domain event. Per AGENTS.md, this makes it the sanctioned
/// `execute()`-resident guard for the not-found case: it does not emit a domain event that
/// downstream blocks consume, so it does **not** carry a `// Domain skip:` comment and is
/// not a candidate for promotion into `accepts()`. There is exactly one call site that needs
/// this decision documented — here — rather than at the 16+ individual call sites across the
/// codebase.
///
/// # Usage
///
/// ```ignore
/// let entry = require_project!(self, project);
/// ```
macro_rules! require_project {
    ($self:expr, $project:expr) => {{
        let _registry_guard = match super::read_registry(&$self.registry) {
            Ok(g) => g,
            Err(e) => return Box::pin(async move { Err(e) }),
        };
        match super::require_project(&_registry_guard, &$project) {
            Ok(e) => e,
            Err(result) => return Box::pin(async { Ok(result) }),
        }
    }};
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
        Box::pin(async { Ok(foundry_sdk::task_block::TaskBlockResult::success($msg, vec![])) })
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
/// - `pub struct MyBlock { registry: Arc<RwLock<Registry>>, agent: Arc<dyn AgentGateway> }`
/// - `impl MyBlock { pub fn new(agent: Arc<dyn AgentGateway>, registry: Arc<RwLock<Registry>>) -> Self }`
///
/// Requires `Registry` and `AgentGateway` to be in scope at the call site.
macro_rules! agent_block_new {
    ($(#[$meta:meta])* $vis:vis struct $name:ident) => {
        $(#[$meta])*
        $vis struct $name {
            registry: ::std::sync::Arc<::std::sync::RwLock<Registry>>,
            agent: ::std::sync::Arc<dyn AgentGateway>,
        }

        impl $name {
            pub fn new(
                agent: ::std::sync::Arc<dyn AgentGateway>,
                registry: ::std::sync::Arc<::std::sync::RwLock<Registry>>,
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
        foundry_sdk::event::Event::new(
            $event_type,
            $project.to_string(),
            foundry_sdk::throttle::Throttle::Full,
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
                foundry_sdk::task_block::BlockKind::$kind,
            );
        }

        #[test]
        fn sinks_on_expected() {
            assert_eq!(
                { $block_expr }.sinks_on(),
                &[$(foundry_sdk::event::EventType::$event),+],
            );
        }
    };
}

/// Generates the struct definition, `pub fn new(agent, registry)` constructor,
/// and `#[cfg(test)] pub(super) fn with_gateways(agent, registry, shell)` test
/// constructor for agent-driven execution blocks that hold `registry`, `agent`,
/// and `shell` gateways.
///
/// Requires `Registry`, `AgentGateway`, `ShellGateway`, and `ProcessShellGateway`
/// to be in scope at the call site.
///
/// # Usage
/// ```ignore
/// agent_execution_block! {
///     /// Doc comment
///     pub struct MyBlock
/// }
/// ```
macro_rules! agent_execution_block {
    ($(#[$meta:meta])* $vis:vis struct $name:ident) => {
        $(#[$meta])*
        $vis struct $name {
            registry: ::std::sync::Arc<::std::sync::RwLock<Registry>>,
            agent: ::std::sync::Arc<dyn AgentGateway>,
            shell: ::std::sync::Arc<dyn ShellGateway>,
        }

        impl $name {
            pub fn new(
                agent: ::std::sync::Arc<dyn AgentGateway>,
                registry: ::std::sync::Arc<::std::sync::RwLock<Registry>>,
            ) -> Self {
                Self {
                    registry,
                    agent,
                    shell: ::std::sync::Arc::new(ProcessShellGateway),
                }
            }

            #[cfg(any(test, feature = "test-support"))]
            pub fn with_gateways(
                agent: ::std::sync::Arc<dyn AgentGateway>,
                registry: ::std::sync::Arc<::std::sync::RwLock<Registry>>,
                shell: ::std::sync::Arc<dyn ShellGateway>,
            ) -> Self {
                Self { registry, agent, shell }
            }
        }
    };
}

/// Infallible `writeln!` into a `String`.
///
/// Wraps `std::writeln!` for the common pattern of writing to a `String`
/// (which implements `std::fmt::Write` and never returns `Err`). Requires
/// `std::fmt::Write` to be in scope at the call site.
///
/// # Usage
///
/// ```ignore
/// use std::fmt::Write as _;
/// let mut out = String::new();
/// wln!(out);                       // blank line
/// wln!(out, "# {}", heading);      // formatted line
/// ```
#[macro_export]
macro_rules! wln {
    ($dst:expr) => {
        ::std::writeln!($dst).expect("write to String never fails")
    };
    ($dst:expr, $($arg:tt)*) => {
        ::std::writeln!($dst, $($arg)*).expect("write to String never fails")
    };
}

/// Generates the complete `execute()` body for a digest-writer block.
///
/// All four digest-writing blocks share the same `execute()` skeleton: parse the
/// trigger payload, clone `project` and `throttle` from the trigger, clone the
/// listed `self` fields, then `Box::pin` an async block that calls the
/// module-local `write()` function. The only variation is the payload type and
/// which `self` fields to clone.
///
/// # Usage
///
/// ```ignore
/// fn execute(&self, trigger: &Event) -> foundry_sdk::task_block::BlockFuture<'_> {
///     digest_block_execute!(self, trigger, MyPayload, [field1, field2])
/// }
/// ```
///
/// `self` must be passed explicitly because `macro_rules!` macros cannot
/// capture the `self` keyword implicitly. The module-local free function
/// must be named `write` and must accept
/// `(project: &str, throttle: Throttle, payload: &MyPayload, &field1, &field2, ...)`.
macro_rules! digest_block_execute {
    ($self:ident, $trigger:expr, $Payload:ty, [$($field:ident),+ $(,)?]) => {{
        let p = parse_payload!($trigger, $Payload);
        let project = $trigger.project.clone();
        let throttle = $trigger.throttle;
        $(let $field = $self.$field.clone();)+
        Box::pin(async move { write(&project, throttle, &p, $(&$field),+) })
    }};
}

/// Generates the `Box::pin(async move { super::emit_result(...) })` tail for
/// simple Observer `execute()` bodies.
///
/// Use when the `execute` body is: extract project/throttle, derive a value,
/// optionally log, then emit exactly one event via `super::emit_result`. The
/// macro replaces the five-line `Box::pin(async move { super::emit_result(...) })`
/// with a single call, keeping the per-block logic visible above it.
///
/// # Usage
///
/// ```ignore
/// fn execute(&self, trigger: &Event) -> foundry_sdk::task_block::BlockFuture<'_> {
///     let project = trigger.project.clone();
///     let throttle = trigger.throttle;
///     let value = /* ... derived expression ... */;
///     emit_observer!(project, throttle, format!("msg: {value}"), SomeEventType,
///         SomePayload { field: value })
/// }
/// ```
macro_rules! emit_observer {
    ($project:ident, $throttle:ident, $msg:expr, $event_type:ident, $payload:expr $(,)?) => {
        Box::pin(async move {
            super::emit_result(
                $msg,
                foundry_sdk::event::EventType::$event_type,
                &$project,
                $throttle,
                &$payload,
            )
        })
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
/// - `pub struct MyBlock { registry: Arc<RwLock<Registry>>, field: Arc<dyn Trait>, ... }`
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
            registry: ::std::sync::Arc<::std::sync::RwLock<Registry>>,
            $gw_field: ::std::sync::Arc<dyn $gw_trait>,
        }

        impl $name {
            pub fn new(registry: ::std::sync::Arc<::std::sync::RwLock<Registry>>) -> Self {
                Self {
                    registry,
                    $gw_field: ::std::sync::Arc::new($gw_default),
                }
            }

            #[cfg(any(test, feature = "test-support"))]
            pub fn with_gateways(
                registry: ::std::sync::Arc<::std::sync::RwLock<Registry>>,
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
            registry: ::std::sync::Arc<::std::sync::RwLock<Registry>>,
            $($gw_field: ::std::sync::Arc<dyn $gw_trait>),+
        }

        impl $name {
            pub fn new(registry: ::std::sync::Arc<::std::sync::RwLock<Registry>>) -> Self {
                Self {
                    registry,
                    $($gw_field: ::std::sync::Arc::new($gw_default)),+
                }
            }

            #[cfg(any(test, feature = "test-support"))]
            pub fn with_gateways(
                registry: ::std::sync::Arc<::std::sync::RwLock<Registry>>,
                $($gw_field: ::std::sync::Arc<dyn $gw_trait>),+
            ) -> Self {
                Self { registry, $($gw_field),+ }
            }
        }
    };
}
