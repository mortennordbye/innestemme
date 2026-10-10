//! The optional web page (`--web`): a map of everything the assistant understands, from the skill
//! catalogue, and a box that shows how a typed phrase would be understood. Read-only: phrases are
//! parsed, and only requests that change nothing (weather, departures, what is on) are answered
//! aloud, so the page is safe on the same port as the answers.

use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use voice_assistant::catalogue::{self, SKILLS};
use voice_assistant::dialog::WakeWord;
use voice_assistant::intent;
use voice_proto::SAMPLE_RATE;

use crate::assistant::{AssistantHandle, Preview};
use crate::metrics::SpeechClips;

/// Weather or departures over a slow network, then speech on a slow CPU.
const HEAR_TIMEOUT: Duration = Duration::from_secs(30);

const PAGE: &str = include_str!("web/index.html");

pub struct Web {
    wake: WakeWord,
    /// What the page shows about this assistant: its name, wake words, voice.
    about: serde_json::Value,
    /// For "hear the answer"; without it the page only parses.
    assistant: Option<AssistantHandle>,
    speech: Arc<SpeechClips>,
}

/// Status, content type and body.
pub type Response = (&'static str, &'static str, Vec<u8>);

impl Web {
    pub fn new(
        wake: WakeWord,
        about: serde_json::Value,
        assistant: Option<AssistantHandle>,
        speech: Arc<SpeechClips>,
    ) -> Self {
        Self { wake, about, assistant, speech }
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
            "/" => Some(("200 OK", "text/html; charset=utf-8", PAGE.as_bytes().to_vec())),
            "/api/skills" => Some(json_response(&json!({ "about": self.about, "skills": SKILLS }))),
            "/api/parse" => {
                let text = query.split('&').find_map(|pair| pair.strip_prefix("q=")).map(decode).unwrap_or_default();
                Some(json_response(&self.parse(&text)))
            }
            _ => None,
        }
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

fn json_response(value: &serde_json::Value) -> Response {
    ("200 OK", "application/json", value.to_string().into_bytes())
}

/// Percent-decoding for a query value; `+` is a space.
fn decode(value: &str) -> String {
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
        Web::new(WakeWord::new("Jarvis"), json!({ "name": "Jarvis" }), None, Arc::default())
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
        assert!(web().route("/nothing").is_none());
    }
}
