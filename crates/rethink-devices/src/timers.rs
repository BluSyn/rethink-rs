//! One-shot delayed tasks (JavaScript's `setTimeout`) for handlers that must wait between
//! commands. Handlers take them as `Arc<dyn Timers>` so tests can drive the clock.

use std::time::Duration;

pub trait Timers: Send + Sync {
    /// Run `task` once, `delay` from now.
    fn after(&self, delay: Duration, task: Box<dyn FnOnce() + Send>);
}

/// Each task sleeps on its own thread.
pub struct ThreadTimers;

impl Timers for ThreadTimers {
    fn after(&self, delay: Duration, task: Box<dyn FnOnce() + Send>) {
        std::thread::spawn(move || {
            std::thread::sleep(delay);
            task();
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Instant;

    #[test]
    fn thread_timers_run_the_task_once_the_delay_has_passed() {
        let (tx, rx) = mpsc::channel();
        let started = Instant::now();
        ThreadTimers.after(
            Duration::from_millis(20),
            Box::new(move || tx.send(started.elapsed()).unwrap()),
        );
        let elapsed = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(elapsed >= Duration::from_millis(20), "{elapsed:?}");
    }
}
