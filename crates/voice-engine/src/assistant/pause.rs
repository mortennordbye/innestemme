//! Pausing listening from the web page.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::SystemTime;

/// Listening switched off from the web page (guests, a long conversation in the room), for a while
/// or until it is switched on again. Only the wake word and requests are ignored: timers and
/// reminders still ring.
#[derive(Default)]
pub struct Pause {
    /// Unix seconds when listening resumes: 0 while listening, `u64::MAX` until switched on.
    until: AtomicU64,
}

impl Pause {
    /// Stops listening for `minutes`, or until `resume` without them.
    pub fn pause(&self, minutes: Option<u64>) {
        let until = minutes.map_or(u64::MAX, |m| unix_now().saturating_add(m.saturating_mul(60)));
        self.until.store(until, Relaxed);
    }

    pub fn resume(&self) {
        self.until.store(0, Relaxed);
    }

    /// When listening resumes, in unix seconds (`u64::MAX`: when switched on), if it is paused.
    pub fn until(&self) -> Option<u64> {
        let until = self.until.load(Relaxed);
        (until > unix_now()).then_some(until)
    }

    pub fn paused(&self) -> bool {
        self.until().is_some()
    }
}

fn unix_now() -> u64 {
    SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pauses_for_a_while_or_until_resumed() {
        let pause = Pause::default();
        assert!(!pause.paused());
        pause.pause(Some(60));
        let until = pause.until().unwrap();
        assert!((3590..=3610).contains(&(until - unix_now())));
        pause.pause(None);
        assert_eq!(pause.until(), Some(u64::MAX));
        pause.resume();
        assert!(!pause.paused());
        // A pause that ran out is over without anyone resuming it.
        pause.until.store(unix_now() - 1, Relaxed);
        assert!(!pause.paused());
    }
}
