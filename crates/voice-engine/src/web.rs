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

use voice_assistant::lang::Lang;

use crate::assistant::{Announcement, AssistantHandle, Preview};
use crate::control::Control;
use crate::hub::Hub;
use crate::learned::Learned;
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
    ("party_relaxed", "relaxed party mode"),
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
    /// Corrections learned from repeats, listed on the log view.
    learned: Option<Arc<Learned>>,
    /// The bearer token Home Assistant sends with `/api/announce`; without one there is no such page.
    announce_token: Option<String>,
    /// When the last announcements were taken, for [`ANNOUNCE_RATE`].
    announced: Recent,
    /// When the last wrong or missing tokens came, for [`REFUSED_RATE`].
    refused: Recent,
}

type Recent = std::sync::Mutex<std::collections::VecDeque<std::time::Instant>>;

/// Announcements taken per [`ANNOUNCE_WINDOW`]; more are refused, so an automation that loops
/// cannot talk over everything.
const ANNOUNCE_RATE: usize = 5;
/// Wrong tokens per [`ANNOUNCE_WINDOW`] before every request is refused, so the token cannot be
/// guessed at speed.
const REFUSED_RATE: usize = 10;
const ANNOUNCE_WINDOW: Duration = Duration::from_secs(60);
/// The longest text spoken, about 20 seconds of speech: more is a mistake in the automation. In
/// bytes too, so the encoded text and the headers fit the request head (`metrics::serve_http`).
const ANNOUNCE_MAX: usize = 300;
const ANNOUNCE_MAX_BYTES: usize = 1200;
/// The longest request typed into the page's "hear" and "plan" boxes.
const TYPED_MAX: usize = 300;

/// Takes one more within the window, unless `max` are taken already.
fn take(recent: &Recent, max: usize) -> bool {
    let mut recent = recent.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let now = std::time::Instant::now();
    while recent.front().is_some_and(|at| now.duration_since(*at) > ANNOUNCE_WINDOW) {
        recent.pop_front();
    }
    let room = recent.len() < max;
    if room {
        recent.push_back(now);
    }
    room
}

/// Whether as many as `max` are in the window already.
fn full(recent: &Recent, max: usize) -> bool {
    let recent = recent.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let now = std::time::Instant::now();
    recent.iter().filter(|at| now.duration_since(**at) <= ANNOUNCE_WINDOW).count() >= max
}

/// Text from outside on one log line.
pub(crate) fn one_line(text: &str) -> String {
    text.chars().map(|c| if c.is_control() { ' ' } else { c }).collect()
}

fn too_many(reason: String) -> Response {
    ("429 Too Many Requests", "application/json", json!({ "reason": reason }).to_string().into_bytes())
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
        Self {
            wake,
            about,
            assistant,
            speech,
            unhandled,
            hub: None,
            control: None,
            learned: None,
            announce_token: None,
            announced: Default::default(),
            refused: Default::default(),
        }
    }

    /// Turns on `/api/announce` for requests that carry this token.
    pub fn with_announce_token(mut self, token: Option<String>) -> Self {
        self.announce_token = token.filter(|t| !t.trim().is_empty()).map(|t| t.trim().to_owned());
        self
    }

    /// `POST /api/announce?text=...` with `Authorization: Bearer <announce-token>`: Home Assistant
    /// automations speak through the assistant (a doorbell, a washing machine done). Optional
    /// `room=kitchen` or `player=media_player.kitchen` for another speaker, `lang=no` for
    /// Norwegian. `head` is the request's header block.
    pub fn announce(&self, post: bool, target: &str, head: &str) -> Option<Response> {
        let query = target.strip_prefix("/api/announce")?;
        let query = if query.is_empty() { "" } else { query.strip_prefix('?')? };
        let token = self.announce_token.as_deref()?;
        // The token first: nothing about the request is told to a caller without it.
        if full(&self.refused, REFUSED_RATE) {
            return Some(too_many("Too many wrong tokens; try again in a minute.".into()));
        }
        if !bearer(head).is_some_and(|given| same(given.as_bytes(), token.as_bytes())) {
            take(&self.refused, REFUSED_RATE);
            let body = json!({ "reason": "Send Authorization: Bearer <announce-token>." }).to_string();
            return Some(("401 Unauthorized", "application/json", body.into_bytes()));
        }
        if !post {
            return Some(bad_request("POST"));
        }
        let Some(assistant) = &self.assistant else {
            return Some(bad_request("This engine has no assistant."));
        };
        let announcement = match announcement(query) {
            Ok(announcement) => announcement,
            Err(reason) => return Some(bad_request(reason)),
        };
        if !take(&self.announced, ANNOUNCE_RATE) {
            return Some(too_many(format!("At most {ANNOUNCE_RATE} announcements a minute.")));
        }
        tracing::info!(
            text = one_line(&announcement.text),
            room = announcement.room.as_deref().map(one_line),
            player = announcement.player,
            "announcement taken"
        );
        assistant.announce(announcement);
        Some(("202 Accepted", "application/json", json!({ "ok": true }).to_string().into_bytes()))
    }

    pub fn with_learned(mut self, learned: Arc<Learned>) -> Self {
        self.learned = Some(learned);
        self
    }

    /// `/api/learned`: the corrections learned from repeats; `POST /api/learned?forget=<heard>`
    /// drops one.
    pub fn learned(&self, post: bool, target: &str) -> Option<Response> {
        let query = target.strip_prefix("/api/learned")?;
        let query = match query.strip_prefix('?') {
            Some(query) => query,
            None if query.is_empty() => "",
            None => return None,
        };
        let Some(learned) = &self.learned else {
            return Some(json_response(&json!({ "enabled": false, "pairs": [] })));
        };
        if post {
            let Some(heard) = query.split('&').find_map(|pair| pair.strip_prefix("forget=")).map(decode) else {
                return Some(bad_request("`forget=<heard>`"));
            };
            learned.forget(&heard);
        }
        let pairs: Vec<serde_json::Value> = learned
            .list()
            .into_iter()
            .map(|p| {
                json!({
                    "heard": p.heard,
                    "said": p.said,
                    "meant": p.meant,
                    "count": p.count,
                    "last": p.last,
                    "used": p.count >= crate::learned::LEARN_AFTER,
                })
            })
            .collect();
        Some(json_response(&json!({ "enabled": true, "after": crate::learned::LEARN_AFTER, "pairs": pairs })))
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
            return Some(bad_request(
                "`do` is one of party, party_fast, party_relaxed, party_off, lockdown, stand_down",
            ));
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
        if text.chars().count() > TYPED_MAX {
            return Some(json_response(&json!({ "reason": "That is longer than any spoken request." })));
        }
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

    /// `/api/plan?q=...`: what a request would do to each light, from the code that runs it.
    /// Nothing is switched.
    pub async fn plan(&self, target: &str) -> Option<Response> {
        let query = target.strip_prefix("/api/plan?")?;
        let text = query.split('&').find_map(|pair| pair.strip_prefix("q=")).map(decode).unwrap_or_default();
        if text.chars().count() > TYPED_MAX {
            return Some(json_response(&json!({ "reason": "That is longer than any spoken request." })));
        }
        let Some(assistant) = &self.assistant else {
            return Some(json_response(&json!({ "reason": "This engine has no assistant." })));
        };
        let reply = tokio::time::timeout(HEAR_TIMEOUT, assistant.plan(text)).await;
        Some(json_response(
            &reply
                .ok()
                .and_then(Result::ok)
                .unwrap_or_else(|| json!({ "reason": "The assistant did not answer in time." })),
        ))
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
            Some(crate::party::Mode::Relaxed) => json!("relaxed"),
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

/// The announcement a query asks for, or why it cannot be spoken.
fn announcement(query: &str) -> Result<Announcement, &'static str> {
    let value = |key: &str| {
        query
            .split('&')
            .find_map(|pair| pair.strip_prefix(key)?.strip_prefix('='))
            .map(decode)
            .map(|v| v.trim().to_owned())
    };
    let text = value("text").unwrap_or_default();
    if text.is_empty() {
        return Err("`text` is what to say.");
    }
    if text.chars().count() > ANNOUNCE_MAX || text.len() > ANNOUNCE_MAX_BYTES {
        return Err("`text` is over 300 characters.");
    }
    let player = value("player").filter(|p| !p.is_empty());
    let valid = |p: &String| {
        p.strip_prefix("media_player.").is_some_and(|id| {
            !id.is_empty() && id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        })
    };
    if player.as_ref().is_some_and(|p| !valid(p)) {
        return Err("`player` is a media player entity id, like media_player.kitchen.");
    }
    let lang = match value("lang").as_deref() {
        Some("no" | "nb" | "nn" | "norwegian") => Lang::Norwegian,
        None | Some("" | "en" | "english") => Lang::English,
        Some(_) => return Err("`lang` is en or no."),
    };
    Ok(Announcement { text, lang, room: value("room").filter(|r| !r.is_empty()), player })
}

/// The bearer token in a request's header block.
fn bearer(head: &str) -> Option<&str> {
    head.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        let value = value.trim();
        let scheme = value.get(..7)?;
        (name.trim().eq_ignore_ascii_case("authorization") && scheme.eq_ignore_ascii_case("bearer "))
            .then(|| value[7..].trim())
    })
}

/// Compares in time that does not depend on where the token differs.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |diff, (x, y)| diff | (x ^ y)) == 0
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
    fn announces_only_with_the_token_and_a_short_text() {
        let head =
            |token: &str| format!("POST /api/announce HTTP/1.1\r\nHost: x\r\nauthorization: Bearer {token}\r\n\r\n");
        let status = |response: Option<Response>| response.map(|r| r.0);
        // Off without a token: no such page.
        assert_eq!(status(web().announce(true, "/api/announce?text=hi", &head("secret"))), None);
        let (handle, announced) = AssistantHandle::for_tests();
        let web = Web::new(WakeWord::new("Jarvis"), json!({}), Some(handle), Arc::default(), None)
            .with_announce_token(Some("secret".into()));
        let call = |target: &str, head: &str| status(web.announce(true, target, head));
        assert_eq!(call("/api/announce?text=hi", &head("wrong")), Some("401 Unauthorized"));
        assert_eq!(call("/api/announce?text=hi", "POST / HTTP/1.1\r\n\r\n"), Some("401 Unauthorized"));
        assert_eq!(status(web.announce(false, "/api/announce?text=hi", &head("secret"))), Some("400 Bad Request"));
        assert_eq!(call("/api/announce?text=", &head("secret")), Some("400 Bad Request"));
        let long = format!("/api/announce?text={}", "a".repeat(ANNOUNCE_MAX + 1));
        assert_eq!(call(&long, &head("secret")), Some("400 Bad Request"));
        assert_eq!(call("/api/announce?text=hi&player=light.kitchen", &head("secret")), Some("400 Bad Request"));
        assert_eq!(call("/api/announcements?text=hi", &head("secret")), None);
        assert!(announced().is_empty());

        assert_eq!(
            call("/api/announce?text=Someone+is+at+the+door.&room=kitchen", &head("secret")),
            Some("202 Accepted")
        );
        assert_eq!(
            call("/api/announce?text=Vaskemaskinen+er+ferdig.&lang=no&player=media_player.bad", &head("secret")),
            Some("202 Accepted")
        );
        assert_eq!(
            announced(),
            [
                Announcement {
                    text: "Someone is at the door.".into(),
                    lang: Lang::English,
                    room: Some("kitchen".into()),
                    player: None
                },
                Announcement {
                    text: "Vaskemaskinen er ferdig.".into(),
                    lang: Lang::Norwegian,
                    room: None,
                    player: Some("media_player.bad".into())
                },
            ]
        );
        // A looping automation is cut off.
        for _ in 2..ANNOUNCE_RATE {
            assert_eq!(call("/api/announce?text=hi", &head("secret")), Some("202 Accepted"));
        }
        assert_eq!(call("/api/announce?text=hi", &head("secret")), Some("429 Too Many Requests"));
    }

    #[test]
    fn announce_checks_the_token_first_and_slows_guessing() {
        let (handle, _announced) = AssistantHandle::for_tests();
        let web = Web::new(WakeWord::new("Jarvis"), json!({}), Some(handle), Arc::default(), None)
            .with_announce_token(Some("secret".into()));
        let head = |token: &str| format!("POST /api/announce HTTP/1.1\r\nAuthorization: bearer {token}\r\n\r\n");
        let status = |post, target: &str, head: &str| web.announce(post, target, head).map(|r| r.0);
        // Without the token, nothing about the request is told: a GET or a bad text is still 401.
        assert_eq!(status(false, "/api/announce?text=hi", &head("nope")), Some("401 Unauthorized"));
        assert_eq!(status(true, "/api/announce?text=", "POST / HTTP/1.1\r\n\r\n"), Some("401 Unauthorized"));
        for _ in 2..REFUSED_RATE {
            assert_eq!(status(true, "/api/announce?text=hi", &head("guess")), Some("401 Unauthorized"));
        }
        // Too many wrong tokens: even the right one waits out the minute.
        assert_eq!(status(true, "/api/announce?text=hi", &head("secret")), Some("429 Too Many Requests"));
        assert_eq!(one_line("Someone\nis at\r\nthe door"), "Someone is at  the door");
    }

    #[tokio::test]
    async fn typed_requests_are_capped() {
        let long = format!("/api/hear?q={}", "a+".repeat(TYPED_MAX + 1));
        let body: serde_json::Value = serde_json::from_slice(&web().hear(&long).await.unwrap().2).unwrap();
        assert_eq!(body["reason"], "That is longer than any spoken request.");
        let long = long.replace("/api/hear", "/api/plan");
        let body: serde_json::Value = serde_json::from_slice(&web().plan(&long).await.unwrap().2).unwrap();
        assert_eq!(body["reason"], "That is longer than any spoken request.");
    }

    #[test]
    fn lists_and_forgets_learned_corrections() {
        let learned = Arc::new(Learned::new(None));
        for _ in 0..2 {
            learned.failed("Code rent.");
            learned.understood("Code red.");
        }
        let web = web().with_learned(learned.clone());
        let body: serde_json::Value = serde_json::from_slice(&web.learned(false, "/api/learned").unwrap().2).unwrap();
        assert_eq!(body["pairs"][0]["meant"], "Code red.");
        assert_eq!(body["pairs"][0]["used"], true);
        let body: serde_json::Value =
            serde_json::from_slice(&web.learned(true, "/api/learned?forget=code+rent").unwrap().2).unwrap();
        assert_eq!(body["pairs"].as_array().map(Vec::len), Some(0));
        assert!(web.learned(false, "/api/learnedx").is_none());
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
