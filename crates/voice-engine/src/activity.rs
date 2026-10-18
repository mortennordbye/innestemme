//! What the assistant did lately, for the web page: each request with what was heard and answered,
//! by which skill and how fast, today's numbers, and whether the satellite is connected. Kept in
//! memory only; a restart starts it over.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::{json, Value};

/// About a day of requests in a busy home.
const KEPT: usize = 500;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// Answered aloud.
    Answered,
    /// Done without a word ("pause the music").
    Done,
    /// Not understood after the wake word.
    NotUnderstood,
}

#[derive(Debug, Clone, Serialize)]
pub struct Turn {
    /// Seconds since the epoch.
    pub at: u64,
    pub heard: String,
    pub answer: Option<String>,
    /// The catalogue's name for the skill the rules picked.
    pub skill: Option<&'static str>,
    pub outcome: Outcome,
    /// From the end of speech to the first answer audio, or to the action for one done without words.
    pub after_ms: Option<u64>,
    /// From the request to the answer's text: what the skill took.
    pub decide_ms: u64,
}

pub struct Activity {
    started: Instant,
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    turns: VecDeque<Turn>,
    /// The connected satellite's name, and since when (seconds since the epoch).
    satellite: Option<(String, u64)>,
}

impl Default for Activity {
    fn default() -> Self {
        Self { started: Instant::now(), inner: Mutex::default() }
    }
}

pub fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

impl Activity {
    fn inner(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn record(&self, turn: Turn) {
        let mut inner = self.inner();
        if inner.turns.len() == KEPT {
            inner.turns.pop_front();
        }
        inner.turns.push_back(turn);
    }

    /// `name` when a satellite connects, `None` when it goes.
    pub fn satellite(&self, name: Option<&str>) {
        self.inner().satellite = name.map(|n| (n.to_owned(), now()));
    }

    pub fn uptime(&self) -> Duration {
        self.started.elapsed()
    }

    /// `/api/activity`: the latest requests (newest first), today's numbers, the satellite.
    /// `midnight`: today's start, in seconds since the epoch.
    pub fn json(&self, recent: usize, midnight: u64) -> Value {
        let inner = self.inner();
        let today: Vec<&Turn> = inner.turns.iter().filter(|t| t.at >= midnight).collect();
        let mut times: Vec<u64> = today.iter().filter_map(|t| t.after_ms).collect();
        times.sort_unstable();
        let pick = |q: f64| times.get(((times.len() as f64 - 1.0) * q).round() as usize).copied();
        let mut skills: Vec<(&str, usize)> = Vec::new();
        for skill in today.iter().filter_map(|t| t.skill) {
            match skills.iter_mut().find(|(s, _)| *s == skill) {
                Some((_, n)) => *n += 1,
                None => skills.push((skill, 1)),
            }
        }
        skills.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
        let count = |o: Outcome| today.iter().filter(|t| t.outcome == o).count();
        json!({
            "recent": inner.turns.iter().rev().take(recent).collect::<Vec<_>>(),
            "today": {
                "requests": today.len(),
                "answered": count(Outcome::Answered),
                "done": count(Outcome::Done),
                "not_understood": count(Outcome::NotUnderstood),
                "median_ms": pick(0.5),
                "p90_ms": pick(0.9),
                "fastest_ms": times.first(),
                "skills": skills.iter().take(6).map(|(name, n)| json!({ "name": name, "count": n })).collect::<Vec<_>>(),
            },
            "satellite": inner.satellite.as_ref().map(|(name, since)| json!({ "name": name, "since": since })),
            "uptime_s": self.started.elapsed().as_secs(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn(at: u64, after: Option<u64>, outcome: Outcome, skill: Option<&'static str>) -> Turn {
        Turn { at, heard: "x".into(), answer: None, skill, outcome, after_ms: after, decide_ms: 1 }
    }

    #[test]
    fn counts_today_only_and_takes_percentiles() {
        let a = Activity::default();
        a.record(turn(50, Some(9000), Outcome::Answered, Some("Weather")));
        for ms in [200, 300, 400, 500, 600] {
            a.record(turn(150, Some(ms), Outcome::Answered, Some("Lights")));
        }
        a.record(turn(160, None, Outcome::NotUnderstood, None));
        let v = a.json(3, 100);
        assert_eq!(v["today"]["requests"], 6);
        assert_eq!(v["today"]["not_understood"], 1);
        assert_eq!(v["today"]["median_ms"], 400);
        assert_eq!(v["today"]["fastest_ms"], 200);
        assert_eq!(v["today"]["skills"][0]["name"], "Lights");
        assert_eq!(v["recent"].as_array().unwrap().len(), 3);
        assert_eq!(v["recent"][0]["outcome"], "not_understood");
    }

    #[test]
    fn keeps_a_bounded_history() {
        let a = Activity::default();
        for i in 0..KEPT + 10 {
            a.record(turn(i as u64, None, Outcome::Done, None));
        }
        assert_eq!(a.inner().turns.len(), KEPT);
    }
}
