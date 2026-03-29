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

/// Generates a struct definition with `registry` and one gateway field, a
/// `pub fn new(registry)` constructor that wires the production gateway default,
/// and a `#[cfg(test)]` test constructor named `with_<field>`.
///
/// Intended for blocks that follow the "registry + one gateway with production
/// default" pattern. Multi-gateway blocks, injected-gateway blocks, and blocks
/// with extra constructor logic should remain hand-written.
///
/// # Usage
///
/// ```ignore
/// task_block_new! {
///     /// Optional doc comment or attributes
///     pub struct MyBlock {
///         shell: ShellGateway = crate::gateway::ProcessShellGateway
///     }
/// }
/// ```
///
/// This expands to:
/// - `pub struct MyBlock { registry: Arc<Registry>, shell: Arc<dyn ShellGateway> }`
/// - `impl MyBlock { pub fn new(registry) -> Self { ... } }`
/// - `#[cfg(test)] fn with_shell(registry, shell) -> Self { ... }`
macro_rules! task_block_new {
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
            pastey::paste! {
                fn [<with_ $gw_field>](
                    registry: ::std::sync::Arc<Registry>,
                    $gw_field: ::std::sync::Arc<dyn $gw_trait>,
                ) -> Self {
                    Self { registry, $gw_field }
                }
            }
        }
    };
}
