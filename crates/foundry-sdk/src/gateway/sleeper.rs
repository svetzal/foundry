use std::pin::Pin;
use std::time::Duration;

/// Waits for a duration before a block tries again.
///
/// Time is an I/O boundary like any other: a block that pauses between
/// attempts takes a `Sleeper`, so a test can replace real time with a double
/// that records each pause and returns at once
/// (`fakes::RecordingSleeper` under the `test-support` feature).
pub trait Sleeper: Send + Sync {
    fn sleep(
        &self,
        duration: Duration,
    ) -> Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>>;
}
