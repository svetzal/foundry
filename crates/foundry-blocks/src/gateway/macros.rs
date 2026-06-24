/// Generates the full newtype wrapper for a CLI-backed [`AgentGateway`].
///
/// Every CLI-backed gateway follows the same pattern: a newtype wrapping
/// `CliAgentGateway<Adapter>`, a backwards-compatible `new` constructor, a
/// `new_with_streaming` full constructor, a `with_models` builder, and a
/// forwarding `impl AgentGateway`.  Only the struct name and adapter type vary.
///
/// # Usage
///
/// ```ignore
/// cli_agent_gateway! {
///     /// Doc comment for the gateway struct.
///     FooAgentGateway, FooAdapter
/// }
/// ```
macro_rules! cli_agent_gateway {
    ($(#[$meta:meta])* $name:ident, $adapter:ident) => {
        $(#[$meta])*
        pub struct $name(CliAgentGateway<$adapter>);

        impl $name {
            /// Backwards-compatible constructor: default session log dir, default
            /// stream runner, and a broadcast channel with no external receivers.
            pub fn new(shell: Arc<dyn ShellGateway>) -> Self {
                let (event_tx, _) = broadcast::channel(16);
                Self::new_with_streaming(
                    shell,
                    Arc::new(ProcessAgentStreamRunner),
                    foundry_sdk::paths::agent_sessions_dir(),
                    event_tx,
                )
            }

            pub fn new_with_streaming(
                shell: Arc<dyn ShellGateway>,
                stream_runner: Arc<dyn AgentStreamRunner>,
                session_log_dir: PathBuf,
                event_tx: broadcast::Sender<Event>,
            ) -> Self {
                Self(CliAgentGateway::new_with_adapter(
                    shell,
                    stream_runner,
                    session_log_dir,
                    event_tx,
                    $adapter,
                ))
            }

            /// Override the resolved tier/effort maps (from the agent config).
            #[must_use]
            pub fn with_models(self, models: ProviderModels) -> Self {
                Self(self.0.with_models(models))
            }
        }

        impl AgentGateway for $name {
            fn invoke<'a>(
                &'a self,
                request: &'a AgentRequest,
            ) -> Pin<Box<dyn std::future::Future<Output = Result<AgentResponse>> + Send + 'a>> {
                self.0.invoke(request)
            }
        }
    };
}
