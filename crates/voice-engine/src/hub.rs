//! The hub page (`/hub`): a dashboard for a wall tablet or the TV, with the assistant's face. The
//! face follows what the assistant does (listening, speaking) over `/api/events`; the cards come
//! from Home Assistant through `/api/hub`, and a room card switches that room's lights.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::sync::broadcast::error::RecvError;
use voice_assistant::alerts;
use voice_assistant::ha::{HomeAssistant, Light};
use voice_assistant::house;
use voice_assistant::lang::Lang;
use voice_assistant::shopping::ShoppingList;
use voice_assistant::timer::NoticeKind;
use voice_proto::SAMPLE_RATE;

use crate::assistant::{AssistantEvent, AssistantHandle};
use crate::web::Response;

pub const PAGE: &str = include_str!("web/hub.html");
/// The face for each state. Built in; the `hub-faces` folder replaces any of them.
const FACES: &[(&str, &[u8])] = &[
    ("idle", include_bytes!("web/face-idle.jpg")),
    ("listening", include_bytes!("web/face-listening.jpg")),
    ("speaking", include_bytes!("web/face-speaking.jpg")),
];
/// Several screens may show the hub; Home Assistant is asked at most this often.
const FRESH: Duration = Duration::from_secs(5);
/// A comment line now and then keeps idle proxies from closing the event stream.
const KEEPALIVE: Duration = Duration::from_secs(20);

/// What the hub shows besides lights, in one template.
const HUB_TEMPLATE: &str = r#"{% set w = states.weather | first %}
{"weather": {% if w %}{"temp": {{ w.attributes.temperature | tojson }}, "unit": {{ (w.attributes.temperature_unit or '°') | tojson }}, "condition": {{ w.state | tojson }}}{% else %}null{% endif %},
"sunset": {{ state_attr('sun.sun', 'next_setting') | tojson }},
"sunrise": {{ state_attr('sun.sun', 'next_rising') | tojson }},
"media": [{% for s in states.media_player | selectattr('state', 'eq', 'playing') | selectattr('attributes.mass_player_type', 'defined') %}{"name": {{ s.name | tojson }}, "title": {{ (s.attributes.media_title or '') | tojson }}, "artist": {{ (s.attributes.media_artist or '') | tojson }}}{% if not loop.last %},{% endif %}{% endfor %}],
"people": [{% for s in states.person %}{"name": {{ s.name.split(' ')[0] | tojson }}, "home": {{ (s.state == 'home') | tojson }}}{% if not loop.last %},{% endif %}{% endfor %}]}"#;

pub struct Hub {
    ha: Option<Mutex<HomeAssistant>>,
    shopping: Mutex<ShoppingList>,
    /// Full-size face images (`idle.jpg`, `listening.jpg`, `speaking.jpg`), the `hub-faces` setting.
    faces: Option<PathBuf>,
    last: Mutex<Option<(Instant, Value)>>,
}

impl Hub {
    pub fn new(home_assistant: Option<(String, String)>, faces: Option<PathBuf>) -> Arc<Self> {
        Arc::new(Self {
            ha: home_assistant.map(|(url, token)| Mutex::new(HomeAssistant::new(&url, &token))),
            shopping: Mutex::new(ShoppingList::default()),
            faces,
            last: Mutex::new(None),
        })
    }

    /// `/hub/face-<state>.jpg`.
    pub fn face(&self, path: &str) -> Option<Response> {
        let state = path.strip_prefix("/hub/face-")?.strip_suffix(".jpg")?;
        let (_, built_in) = FACES.iter().find(|(name, _)| *name == state)?;
        let own = self.faces.as_ref().and_then(|dir| std::fs::read(dir.join(format!("{state}.jpg"))).ok());
        Some(("200 OK", "image/jpeg", own.unwrap_or_else(|| built_in.to_vec())))
    }

    /// `/api/hub`: the cards' data.
    pub async fn snapshot(self: &Arc<Self>) -> Response {
        let hub = self.clone();
        let value = tokio::task::spawn_blocking(move || hub.cached())
            .await
            .unwrap_or_else(|error| json!({ "error": error.to_string() }));
        ("200 OK", "application/json", value.to_string().into_bytes())
    }

    /// `POST /api/hub/room?area=Kitchen&on=1`: switches a room's lights.
    pub async fn room(self: &Arc<Self>, query: &str) -> Response {
        let param = |key: &str| {
            query.split('&').find_map(|pair| pair.strip_prefix(key)?.strip_prefix('=')).map(crate::web::decode)
        };
        let (Some(area), Some(on)) = (param("area"), param("on")) else {
            return ("400 Bad Request", "application/json", br#"{"reason":"`area` and `on`"}"#.to_vec());
        };
        let hub = self.clone();
        let result = tokio::task::spawn_blocking(move || hub.switch_room(&area, on == "1")).await;
        match result {
            Ok(Ok(())) => ("200 OK", "application/json", br#"{"ok":true}"#.to_vec()),
            Ok(Err(error)) => {
                tracing::warn!(error = format!("{error:#}"), "hub could not switch a room");
                ("502 Bad Gateway", "application/json", json!({ "reason": error.to_string() }).to_string().into_bytes())
            }
            Err(error) => ("500 Internal Server Error", "text/plain", error.to_string().into_bytes()),
        }
    }

    fn switch_room(&self, area: &str, on: bool) -> Result<()> {
        let ha = self.ha.as_ref().context("Home Assistant isn't connected")?;
        let mut ha = ha.lock().unwrap_or_else(|e| e.into_inner());
        let target = ha.find(area)?.with_context(|| format!("no lights in {area}"))?;
        ha.set(&target, on)?;
        tracing::info!(area, on, "room switched from the hub");
        *self.last.lock().unwrap_or_else(|e| e.into_inner()) = None;
        Ok(())
    }

    fn cached(&self) -> Value {
        let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((_, value)) = last.as_ref().filter(|(at, _)| at.elapsed() < FRESH) {
            return value.clone();
        }
        let value = self.fetch().unwrap_or_else(|error| {
            tracing::warn!(error = format!("{error:#}"), "hub data from home assistant failed");
            json!({ "error": "Home Assistant didn't answer." })
        });
        *last = Some((Instant::now(), value.clone()));
        value
    }

    fn fetch(&self) -> Result<Value> {
        let Some(ha) = &self.ha else { return Ok(json!({ "error": "Home Assistant isn't connected." })) };
        let mut ha = ha.lock().unwrap_or_else(|e| e.into_inner());
        let mut value: Value = serde_json::from_str(&ha.template(HUB_TEMPLATE)?).context("parsing the hub data")?;
        let lights = ha.lights()?.to_vec();
        let on = ha.lights_on()?;
        value["rooms"] = rooms(&lights, &on).into();
        let alerts = alerts::active(&ha)?;
        let snapshot = house::snapshot(&ha)?;
        value["alerts"] = alerts.iter().map(|a| alerts::sentence(a, Lang::English)).collect::<Vec<_>>().into();
        value["alarm"] = house::alarm_sentence(&snapshot.alarm, Lang::English).into();
        value["tv_on"] = (!snapshot.tvs.is_empty()).into();
        value["privacy"] = (!snapshot.privacy.is_empty()).into();
        let guest = house::entities(&ha, "switch", "guest")?.into_iter().next();
        value["guest_wifi"] =
            guest.map(|g| json!({ "on": g.state == "on", "clients": g.clients.parse::<u32>().ok() })).into();
        value["shopping"] =
            self.shopping.lock().unwrap_or_else(|e| e.into_inner()).open_items(&ha).unwrap_or_default().into();
        Ok(value)
    }
}

/// One entry per area with lights, alphabetically: how many are on. Status LEDs and the voice
/// device's ring are not room lights; a group counts only through its members.
fn rooms(lights: &[Light], on: &[Light]) -> Vec<Value> {
    let real = |l: &&Light| !l.area.is_empty() && !l.group && !l.name.contains(" LED");
    let mut areas: Vec<&str> = lights.iter().filter(real).map(|l| l.area.as_str()).collect();
    areas.sort_unstable();
    areas.dedup();
    areas
        .into_iter()
        .map(|area| {
            let total = lights.iter().filter(real).filter(|l| l.area == area).count();
            let lit = on.iter().filter(real).filter(|l| l.area == area).count();
            json!({ "area": area, "lit": lit, "total": total })
        })
        .collect()
}

/// `/api/events`: what the assistant is doing, as server-sent events, for the face. The TV screen
/// (`screen`) also gets what it shows now, and counts as watching while connected.
pub async fn events(stream: &mut TcpStream, assistant: &AssistantHandle, screen: bool) -> std::io::Result<()> {
    let mut events = assistant.subscribe();
    let _viewer = screen.then(|| assistant.screen.watch());
    stream
        .write_all(b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncache-control: no-cache\r\nconnection: close\r\n\r\n")
        .await?;
    stream.write_all(b"data: {\"state\":\"idle\"}\n\n").await?;
    if screen {
        let now = json!({ "show": assistant.screen.current() });
        stream.write_all(format!("data: {now}\n\n").as_bytes()).await?;
    }
    // Audio sent for the current answer: the face keeps talking that long.
    let mut samples = 0usize;
    loop {
        let event = match tokio::time::timeout(KEEPALIVE, events.recv()).await {
            Err(_) => {
                stream.write_all(b": keepalive\n\n").await?;
                continue;
            }
            Ok(Err(RecvError::Lagged(_))) => continue,
            Ok(Err(RecvError::Closed)) => return Ok(()),
            Ok(Ok(event)) => event,
        };
        let seconds = |samples: usize| samples as f32 / SAMPLE_RATE as f32;
        let messages = match event {
            AssistantEvent::Listening => vec![json!({ "state": "listening" })],
            AssistantEvent::Heard { text } => vec![json!({ "state": "thinking", "heard": text })],
            AssistantEvent::Answer { text } => {
                samples = 0;
                vec![json!({ "state": "speaking", "text": text })]
            }
            AssistantEvent::Speech(pcm) => {
                samples += pcm.len();
                Vec::new()
            }
            AssistantEvent::Spoken => vec![json!({ "state": "spoken", "seconds": seconds(samples) })],
            AssistantEvent::Done => vec![json!({ "state": "idle" })],
            AssistantEvent::Announcement { text, audio, .. } => vec![
                json!({ "state": "speaking", "text": text }),
                json!({ "state": "spoken", "seconds": seconds(audio.len()) }),
            ],
            AssistantEvent::Timer(notice) => vec![json!({ "timer": {
                "id": notice.id,
                "name": notice.name,
                "left": notice.seconds_left,
                "total": notice.total_seconds,
                "active": notice.active,
                "gone": matches!(notice.kind, NoticeKind::Cancelled | NoticeKind::Finished),
                "finished": notice.kind == NoticeKind::Finished,
            } })],
            AssistantEvent::Show(message) => vec![json!({ "show": message })],
        };
        for message in messages {
            stream.write_all(format!("data: {message}\n\n").as_bytes()).await?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn light(name: &str, area: &str, group: bool) -> Light {
        Light {
            id: format!("light.{}", name.to_lowercase().replace(' ', "_")),
            name: name.into(),
            area: area.into(),
            group,
            members: Vec::new(),
        }
    }

    #[test]
    fn rooms_count_real_lights() {
        let lights = vec![
            light("Kitchen", "Kitchen", true),
            light("Plug", "Kitchen", false),
            light("Sink", "Bathroom", false),
            light("Shower", "Bathroom", false),
            light("U6+ LED", "Living Room", false),
            light("House Lights", "", true),
        ];
        let on = vec![lights[0].clone(), lights[1].clone(), lights[2].clone(), lights[5].clone()];
        let rooms = rooms(&lights, &on);
        assert_eq!(
            Value::from(rooms),
            json!([{ "area": "Bathroom", "lit": 1, "total": 2 }, { "area": "Kitchen", "lit": 1, "total": 1 }])
        );
    }

    #[test]
    fn faces_are_built_in() {
        let hub = Hub::new(None, None);
        assert_eq!(hub.face("/hub/face-speaking.jpg").unwrap().1, "image/jpeg");
        assert!(hub.face("/hub/face-../secret.jpg").is_none());
    }
}
