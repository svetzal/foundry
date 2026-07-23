//! Retry execution logic for task blocks.

use foundry_sdk::event::Event;
use foundry_sdk::task_block::{RetryPolicy, TaskBlock, TaskBlockResult};

/// Execute a block with retry logic, sleeping `policy.backoff` between attempts.
///
/// Returns the final `anyhow::Result<TaskBlockResult>` after all retry attempts
/// are exhausted or a successful result is obtained.
pub(crate) async fn execute_with_retry(
    block: &dyn TaskBlock,
    trigger: &Event,
    policy: RetryPolicy,
) -> anyhow::Result<TaskBlockResult> {
    let mut last_result: Option<anyhow::Result<TaskBlockResult>> = None;

    for attempt in 0..=policy.max_retries {
        if attempt > 0 {
            tracing::info!(attempt, max_retries = policy.max_retries, "retrying block");
            tokio::time::sleep(policy.backoff).await;
        }

        match block.execute(trigger).await {
            Ok(result) if result.success => {
                return Ok(result);
            }
            Ok(result) => {
                tracing::warn!(
                    attempt,
                    summary = %result.summary,
                    "block reported failure, will retry if attempts remain"
                );
                last_result = Some(Ok(result));
            }
            Err(err) => {
                tracing::error!(attempt, error = %err, "block execute error");
                last_result = Some(Err(err));
            }
        }
    }

    #[allow(
        clippy::expect_used,
        reason = "the `0..=policy.max_retries` loop always executes at least once and sets last_result on every iteration"
    )]
    last_result.expect("loop always sets last_result")
}
