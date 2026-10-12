//! What the assistant's screen on the TV (`/tv`) shows: one module at a time (the weather, a
//! scene, a timer), or the face when nothing is. The engine keeps the current module, so a page
//! that opens after the request, or reconnects, shows it at once. A module with a lifetime goes
//! back to the face when it runs out, on the page and here.

use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

pub const PAGE: &str = include_str!("web/tv.html");

#[derive(Default)]
pub struct Screen {
    /// The message last shown and when it runs out.
    current: Mutex<Option<(Value, Option<Instant>)>>,
    /// Pages showing the screen now.
    viewers: AtomicUsize,
}

impl Screen {
    /// The message for the page: `{"module": ..., "data": ..., "ttl": seconds}`.
    pub fn show(&self, module: &str, data: Value, ttl: Option<Duration>) -> Value {
        let message = json!({ "module": module, "data": data, "ttl": ttl.map(|t| t.as_secs()) });
        let ends = ttl.map(|t| Instant::now() + t);
        *self.current.lock().unwrap_or_else(|e| e.into_inner()) = Some((message.clone(), ends));
        message
    }

    pub fn hide(&self) {
        *self.current.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// The message showing now, if it has not run out.
    pub fn current(&self) -> Option<Value> {
        let current = self.current.lock().unwrap_or_else(|e| e.into_inner());
        let (message, ends) = current.as_ref()?;
        ends.is_none_or(|ends| ends > Instant::now()).then(|| message.clone())
    }

    pub fn showing(&self, module: &str) -> bool {
        self.current().is_some_and(|m| m["module"] == module)
    }

    /// Whether a page shows the screen now.
    pub fn watched(&self) -> bool {
        self.viewers.load(Relaxed) > 0
    }

    /// Counts a page as watching until the returned guard is dropped.
    pub fn watch(self: &Arc<Self>) -> Viewer {
        self.viewers.fetch_add(1, Relaxed);
        Viewer(self.clone())
    }
}

pub struct Viewer(Arc<Screen>);

impl Drop for Viewer {
    fn drop(&mut self) {
        self.0.viewers.fetch_sub(1, Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_the_current_module_until_it_runs_out() {
        let screen = Arc::new(Screen::default());
        let message = screen.show("weather", json!({ "temp": 9 }), None);
        assert_eq!(message, json!({ "module": "weather", "data": { "temp": 9 }, "ttl": null }));
        assert!(screen.showing("weather"));
        screen.show("scene", json!({}), Some(Duration::ZERO));
        assert_eq!(screen.current(), None);
        screen.show("lockdown", json!({}), None);
        screen.hide();
        assert_eq!(screen.current(), None);
    }

    #[test]
    fn counts_the_pages_watching() {
        let screen = Arc::new(Screen::default());
        let viewer = screen.watch();
        assert!(screen.watched());
        drop(viewer);
        assert!(!screen.watched());
    }
}
