//! The optional web page (`--web`), the hub (`hub.rs`): a dashboard with the assistant's face, a map
//! of everything the assistant understands from the skill catalogue, and a box that shows how a typed
//! phrase would be understood. Phrases are only parsed, and only requests that change nothing
//! (weather, departures, what is on) are answered aloud. Besides a room's lights from the dashboard,
//! the one thing it changes is whether the assistant listens: a pause for guests or a long conversation.

use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use voice_assistant::catalogue::{self, SKILLS};
use voice_assistant::dialog::WakeWord;
use voice_assistant::intent;
use voice_proto::SAMPLE_RATE;

use crate::assistant::{AssistantHandle, Preview};
use crate::control::Control;
use crate::hub::Hub;
use crate::metrics::SpeechClips;
use crate::unhandled::UnhandledLog;

/// Weather or departures over a slow network, then speech on a slow CPU.
const HEAR_TIMEOUT: Duration = Duration::from_secs(30);

/// Entries the page lists from the unhandled log.
const UNHANDLED_SHOWN: usize = 200;
/// Requests the page lists as recent.
const RECENT_SHOWN: usize = 30;
/// The page's apartment-wide buttons, as the requests they make.
const ACTIONS: &[(&str, &str)] = &[
    ("party", "party mode"),
    ("party_fast", "fast party mode"),
    ("party_off", "stop the party"),
    ("lockdown", "initiate lockdown"),
    ("stand_down", "stand down"),
];

pub struct Web {
    wake: WakeWord,
    /// What the page shows about this assistant: its name, wake words, voice.
    about: serde_json::Value,
    /// For "hear the answer"; without it the page only parses.
    assistant: Option<AssistantHandle>,
    speech: Arc<SpeechClips>,
    unhandled: Option<Arc<UnhandledLog>>,
    /// The dashboard at `/hub`.
    hub: Option<Arc<Hub>>,
    /// Home Assistant's entities and services for the dashboard's views.
    control: Option<Arc<Control>>,
}

/// Status, content type and body.
pub type Response = (&'static str, &'static str, Vec<u8>);

impl Web {
    pub fn new(
        wake: WakeWord,
        about: serde_json::Value,
        assistant: Option<AssistantHandle>,
        speech: Arc<SpeechClips>,
        unhandled: Option<Arc<UnhandledLog>>,
    ) -> Self {
        Self { wake, about, assistant, speech, unhandled, hub: None, control: None }
    }

    pub fn with_control(mut self, control: Arc<Control>) -> Self {
        self.control = Some(control);
        self
    }

    /// `/api/ha/...`: the views' entities, their trends and the weather forecast, images, and
    /// `POST /api/ha/call` for a service.
    pub async fn control_api(&self, post: bool, target: &str) -> Option<Response> {
        let control = self.control.clone()?;
        let (path, query) = target.split_once('?').unwrap_or((target, ""));
        let (path, query) = (path.to_owned(), query.to_owned());
        let result = tokio::task::spawn_blocking(move || match (path.as_str(), post) {
            ("/api/ha/states", false) => control.states().map(|v| json_response(&v)),
            ("/api/ha/call", true) => control.call(&query).map(|()| json_response(&json!({ "ok": true }))),
            ("/api/ha/history", false) => control.history(&query).map(|v| json_response(&v)),
            ("/api/ha/forecast", false) => control.forecast(&query).map(|v| json_response(&v)),
            ("/api/ha/image", false) => control.image(&query),
            _ => Ok(("404 Not Found", "text/plain", b"not found\n".to_vec())),
        })
        .await;
        Some(match result {
            Ok(Ok(response)) => response,
            Ok(Err(error)) => {
                tracing::warn!(error = format!("{error:#}"), "dashboard request failed");
                (
                    "502 Bad Gateway",
                    "application/json",
                    json!({ "reason": format!("{error:#}") }).to_string().into_bytes(),
                )
            }
            Err(error) => ("500 Internal Server Error", "text/plain", error.to_string().into_bytes()),
        })
    }

    pub fn with_hub(mut self, hub: Arc<Hub>) -> Self {
        self.hub = Some(hub);
        self
    }

    /// `/api/events`: the assistant's state as server-sent events, for the hub's face, and with
    /// `?screen` what the TV screen shows. `false` when there is nothing to stream.
    pub async fn events(&self, stream: &mut tokio::net::TcpStream, screen: bool) -> bool {
        let Some(assistant) = &self.assistant else { return false };
        if let Err(error) = crate::hub::events(stream, assistant, screen).await {
            tracing::debug!(%error, "hub event stream closed");
        }
        true
    }

    /// `/api/hub` and `POST /api/hub/room?...`.
    pub async fn hub_api(&self, post: bool, target: &str) -> Option<Response> {
        let hub = self.hub.as_ref()?;
        let (path, query) = target.split_once('?').unwrap_or((target, ""));
        match (path, post) {
            ("/api/hub", _) => Some(hub.snapshot().await),
            ("/api/hub/room", true) => Some(hub.room(query).await),
            _ => None,
        }
    }

    /// `/api/listening`: whether the assistant listens. A POST with `pause=<minutes>` (0: until
    /// switched on) or `resume` changes it.
    pub fn listening(&self, post: bool, target: &str) -> Option<Response> {
        let query = target.strip_prefix("/api/listening")?;
        let query = match query.strip_prefix('?') {
            Some(query) => query,
            None if query.is_empty() => "",
            None => return None,
        };
        let Some(assistant) = &self.assistant else {
            return Some(json_response(&json!({ "available": false })));
        };
        if post {
            let pause = query.split('&').find_map(|pair| pair.strip_prefix("pause="));
            match pause.map(str::parse::<u64>) {
                Some(Ok(0)) => assistant.pause.pause(None),
                Some(Ok(minutes)) => assistant.pause.pause(Some(minutes)),
                Some(Err(_)) => return Some(bad_request("`pause` is a number of minutes")),
                None if query.split('&').any(|pair| pair == "resume") => assistant.pause.resume(),
                None => return Some(bad_request("`pause=<minutes>` or `resume`")),
            }
            tracing::info!(until = ?assistant.pause.until(), "listening changed from the web page");
        }
        let until = assistant.pause.until();
        Some(json_response(&json!({
            "available": true,
            "listening": until.is_none(),
            // Seconds since the epoch, or null while paused until switched on.
            "until": until.filter(|&u| u != u64::MAX),
        })))
    }

    /// `POST /api/act?do=party`: the page's apartment-wide buttons, each a fixed request.
    pub async fn act(&self, post: bool, target: &str) -> Option<Response> {
        let query = target.strip_prefix("/api/act?")?;
        let Some(assistant) = &self.assistant else {
            return Some(bad_request("This engine has no assistant."));
        };
        if !post {
            return Some(bad_request("POST"));
        }
        let what = query.split('&').find_map(|pair| pair.strip_prefix("do=")).unwrap_or_default();
        let Some((_, request)) = ACTIONS.iter().find(|(name, _)| *name == what) else {
            return Some(bad_request("`do` is one of party, party_fast, party_off, lockdown, stand_down"));
        };
        let answer = tokio::time::timeout(HEAR_TIMEOUT, assistant.act((*request).to_owned())).await;
        Some(match answer {
            Ok(Ok(answer)) => json_response(&json!({ "answer": answer })),
            _ => bad_request("The assistant did not answer in time."),
        })
    }

    /// `/api/hear?q=...`: the spoken answer as a URL under `/speech/`, when the request only reads.
    pub async fn hear(&self, target: &str) -> Option<Response> {
        let query = target.strip_prefix("/api/hear?")?;
        let text = query.split('&').find_map(|pair| pair.strip_prefix("q=")).map(decode).unwrap_or_default();
        let Some(assistant) = &self.assistant else {
            return Some(json_response(&json!({ "reason": "This engine has no assistant to answer." })));
        };
        let reply = tokio::time::timeout(HEAR_TIMEOUT, assistant.preview(text)).await;
        let value = match reply {
            Ok(Ok(Preview::Spoken { text, audio })) => {
                json!({ "answer": text, "audio": self.speech.put(&audio, SAMPLE_RATE).trim_start_matches('/') })
            }
            Ok(Ok(Preview::Silent)) => json!({ "reason": "The assistant ends the conversation without a word." }),
            Ok(Ok(Preview::NotRun(reason))) => json!({ "reason": reason }),
            Ok(Err(_)) | Err(_) => json!({ "reason": "The assistant did not answer in time." }),
        };
        Some(json_response(&value))
    }

    pub fn route(&self, target: &str) -> Option<Response> {
        let (path, query) = target.split_once('?').unwrap_or((target, ""));
        match path {
            "/" | "/hub" => Some(("200 OK", "text/html; charset=utf-8", crate::hub::PAGE.as_bytes().to_vec())),
            "/tv" => Some(("200 OK", "text/html; charset=utf-8", crate::screen::PAGE.as_bytes().to_vec())),
            _ if path.starts_with("/hub/") => self.hub.as_ref()?.face(path),
            "/api/skills" => Some(json_response(&json!({ "about": self.about, "skills": SKILLS }))),
            "/api/activity" => Some(json_response(&self.activity())),
            "/api/unhandled" => {
                let entries = self.unhandled.as_ref().map(|log| log.recent(UNHANDLED_SHOWN));
                Some(json_response(&json!({ "enabled": entries.is_some(), "entries": entries.unwrap_or_default() })))
            }
            "/api/parse" => {
                let text = query.split('&').find_map(|pair| pair.strip_prefix("q=")).map(decode).unwrap_or_default();
                Some(json_response(&self.parse(&text)))
            }
            _ => None,
        }
    }

    /// What was asked lately and how fast it was answered, and how the engine is.
    fn activity(&self) -> serde_json::Value {
        let midnight = jiff::Zoned::now().start_of_day().map_or(0, |d| d.timestamp().as_second().max(0) as u64);
        let mut value = match &self.assistant {
            Some(assistant) => assistant.activity.json(RECENT_SHOWN, midnight),
            None => json!({ "recent": [], "today": {} }),
        };
        let cache = self.about["speech_cache"].as_str().map(|dir| {
            let (sentences, bytes) = folder_size(std::path::Path::new(dir));
            json!({ "sentences": sentences, "mb": bytes / 1_000_000 })
        });
        value["system"] = json!({ "memory_mb": resident_mb(), "speech_cache": cache });
        value["party"] = match self.assistant.as_ref().and_then(|a| a.party.mode()) {
            Some(crate::party::Mode::Normal) => json!("normal"),
            Some(crate::party::Mode::Fast) => json!("fast"),
            None => json!(null),
        };
        value["about"] = self.about.clone();
        value
    }

    fn parse(&self, text: &str) -> serde_json::Value {
        let text = text.trim();
        // "Jarvis, turn off the lights": the name is how it is addressed, not part of the request.
        let request = self.wake.strip(text).unwrap_or_else(|| text.to_owned());
        let intent = intent::parse(&request);
        let skill = catalogue::skill_of(&intent);
        json!({
            "text": text,
            "request": request,
            "intent": format!("{intent:?}"),
            "skill": skill,
        })
    }
}

/// Files and bytes under `dir` and its folders (the speech cache keeps one per voice).
fn folder_size(dir: &std::path::Path) -> (u64, u64) {
    let (mut files, mut bytes) = (0, 0);
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        if meta.is_dir() {
            let (f, b) = folder_size(&entry.path());
            (files, bytes) = (files + f, bytes + b);
        } else {
            (files, bytes) = (files + 1, bytes + meta.len());
        }
    }
    (files, bytes)
}

/// The engine's resident memory, where the system says (Linux).
fn resident_mb() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let kb: u64 =
        status.lines().find_map(|l| l.strip_prefix("VmRSS:"))?.trim().trim_end_matches("kB").trim().parse().ok()?;
    Some(kb / 1024)
}

fn json_response(value: &serde_json::Value) -> Response {
    ("200 OK", "application/json", value.to_string().into_bytes())
}

fn bad_request(reason: &str) -> Response {
    ("400 Bad Request", "application/json", json!({ "reason": reason }).to_string().into_bytes())
}

/// Percent-decoding for a query value; `+` is a space.
pub(crate) fn decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let (mut out, mut i) = (Vec::with_capacity(bytes.len()), 0);
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => {
                match std::str::from_utf8(&bytes[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok()) {
                    Some(byte) => {
                        out.push(byte);
                        i += 2;
                    }
                    None => out.push(b'%'),
                }
            }
            byte => out.push(byte),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn web() -> Web {
        Web::new(WakeWord::new("Jarvis"), json!({ "name": "Jarvis" }), None, Arc::default(), None)
    }

    #[test]
    fn decodes_queries() {
        assert_eq!(decode("turn+off%20the%20lights"), "turn off the lights");
        assert_eq!(decode("sl%C3%A5+p%C3%A5"), "slå på");
        assert_eq!(decode("100%"), "100%");
    }

    #[test]
    fn parses_a_phrase_without_acting() {
        let (status, kind, body) = web().route("/api/parse?q=Jarvis%2C+turn+off+the+kitchen+lights").unwrap();
        assert_eq!((status, kind), ("200 OK", "application/json"));
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["skill"]["id"], "lights");
        assert!(value["intent"].as_str().unwrap().contains("on: false"));
    }

    #[test]
    fn serves_the_page_and_the_map() {
        assert!(web().route("/").unwrap().2.starts_with(b"<!doctype html>"));
        let body = web().route("/api/skills").unwrap().2;
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["skills"].as_array().unwrap().len(), SKILLS.len());
        assert!(web().route("/tv").unwrap().2.starts_with(b"<!doctype html>"));
        assert!(web().route("/nothing").is_none());
    }

    #[test]
    fn draws_every_skill_icon() {
        let page = crate::hub::PAGE;
        for skill in SKILLS {
            assert!(page.contains(&format!("id=\"i-{}\"", skill.icon)), "no symbol for {}", skill.icon);
        }
        let emoji = page.chars().find(|&c| matches!(c, '\u{2600}'..='\u{27bf}' | '\u{1f000}'..));
        assert_eq!(emoji, None, "icons are drawn, not emoji");
    }

    #[tokio::test]
    async fn actions_and_activity_are_apart() {
        // "/api/act?" is a prefix of nothing else the page asks for.
        assert!(web().act(true, "/api/activity").await.is_none());
        let (status, ..) = web().act(true, "/api/act?do=party").await.unwrap();
        assert_eq!(status, "400 Bad Request");
    }

    #[test]
    fn activity_without_an_assistant() {
        let body = web().route("/api/activity").unwrap().2;
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["recent"].as_array().unwrap().len(), 0);
        assert_eq!(value["about"]["name"], "Jarvis");
    }

    #[test]
    fn listening_without_an_assistant() {
        let body = web().listening(true, "/api/listening?pause=60").unwrap().2;
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["available"], false);
        assert!(web().listening(false, "/api/listeningx").is_none());
    }

    #[test]
    fn lists_unhandled_requests_only_when_logged() {
        let body = web().route("/api/unhandled").unwrap().2;
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["enabled"], false);
    }
}
