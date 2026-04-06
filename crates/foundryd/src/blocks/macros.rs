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
