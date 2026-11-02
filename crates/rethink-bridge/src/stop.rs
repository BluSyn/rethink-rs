//! Stop signal shared by every task of one bridged session.

use std::sync::Arc;
use tokio::sync::watch;

/// Cloneable; once [`StopSignal::stop`] has been called, every current and future
/// [`StopSignal::stopped`] resolves.
#[derive(Clone)]
pub struct StopSignal(Arc<watch::Sender<bool>>);

impl Default for StopSignal {
    fn default() -> Self {
        Self::new()
    }
}

impl StopSignal {
    pub fn new() -> Self {
        Self(Arc::new(watch::Sender::new(false)))
    }

    pub fn stop(&self) {
        self.0.send_replace(true);
    }

    pub fn is_stopped(&self) -> bool {
        *self.0.borrow()
    }

    pub async fn stopped(&self) {
        let mut rx = self.0.subscribe();
        // The sender lives in `self`, so this only returns once stopped.
        let _ = rx.wait_for(|stopped| *stopped).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn stop_wakes_waiters_before_and_after_the_fact() {
        let stop = StopSignal::new();
        assert!(!stop.is_stopped());
        let waiter = tokio::spawn({
            let stop = stop.clone();
            async move { stop.stopped().await }
        });
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(!waiter.is_finished());

        stop.clone().stop();
        tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .expect("a waiter is woken by stop()")
            .unwrap();
        assert!(stop.is_stopped());
        tokio::time::timeout(Duration::from_secs(1), stop.stopped())
            .await
            .expect("stopped() returns at once after stop()");
    }
}
