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
