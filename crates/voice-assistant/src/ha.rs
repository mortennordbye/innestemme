//! Home Assistant lights over the REST API: one template call lists every light with its area,
//! then `light.turn_on` / `light.turn_off` switch the ones a request names.

use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use crate::dialog::normalize;

const TIMEOUT: Duration = Duration::from_secs(4);
/// Generic service calls: Music Assistant's `play_media` answers only once playback has started
/// (about 4 s for a Spotify playlist on a Sonos).
const SERVICE_TIMEOUT: Duration = Duration::from_secs(20);
/// Lights and areas change rarely; refetch after this long.
const REFRESH: Duration = Duration::from_secs(600);

/// Every light as `{"id", "name", "area"}`, rendered by Home Assistant itself.
const LIGHTS_TEMPLATE: &str = r#"[{% for s in states.light %}{"id": {{ s.entity_id | tojson }}, "name": {{ s.name | tojson }}, "area": {{ (area_name(s.entity_id) or "") | tojson }}}{% if not loop.last %},{% endif %}{% endfor %}]"#;

/// Room names people say, in both languages, grouped by room. A request and an area match when
/// they share a group, so "living room" finds an area called "Stue" and the other way round.
const ROOMS: &[&[&str]] = &[
    &["living room", "livingroom", "lounge", "stue", "stua", "stuen"],
    &["kitchen", "kjøkken", "kjøkkenet"],
    &["bedroom", "soverom", "soverommet"],
    &["office", "study", "kontor", "kontoret"],
    &["hallway", "hall", "entrance", "gang", "gangen", "entre", "entreen"],
    &["bathroom", "bad", "badet", "baderom"],
    &["dining room", "spisestue", "spisestua"],
    &["kids room", "children's room", "barnerom", "barnerommet"],
];

/// Words in a light request that are not part of the room or light name.
const FILLER: &[&str] = &[
    "the", "light", "lights", "lamp", "lamps", "in", "on", "off", "all", "my", "please", "turn", "switch", "and", "of",
    "lys", "lyset", "lysene", "lampe", "lampen", "lampene", "i", "på", "av", "alle", "skru", "slå", "mitt", "min",
    "takk", "them", "it", "those", "these", "they", "that", "back", "again", "too", "now", "dem", "den", "det", "de",
    "igjen", "tilbake", "nå", "here", "her",
];

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Light {
    pub id: String,
    pub name: String,
    pub area: String,
}

/// What a request resolved to: a label to speak and the entities to switch.
#[derive(Debug, Clone, PartialEq)]
pub struct Target {
    pub label: String,
    pub ids: Vec<String>,
    /// `label` is an area (every light in it), not a light's own name.
    pub area: bool,
}

pub struct HomeAssistant {
    base: String,
    token: String,
    agent: ureq::Agent,
    lights: Vec<Light>,
    fetched: Option<Instant>,
}

impl HomeAssistant {
    pub fn new(base: &str, token: &str) -> Self {
        Self {
            base: base.trim_end_matches('/').to_owned(),
            token: token.to_owned(),
            agent: ureq::AgentBuilder::new().timeout(TIMEOUT).build(),
            lights: Vec::new(),
            fetched: None,
        }
    }

    fn post(&self, path: &str, body: serde_json::Value) -> Result<ureq::Response> {
        self.post_within(path, body, TIMEOUT)
    }

    fn post_within(&self, path: &str, body: serde_json::Value, timeout: Duration) -> Result<ureq::Response> {
        self.agent
            .post(&format!("{}{path}", self.base))
            .timeout(timeout)
            .set("Authorization", &format!("Bearer {}", self.token))
            .send_json(body)
            .map_err(|e| match e {
                ureq::Error::Status(401, _) => {
                    anyhow::anyhow!("Home Assistant rejected the token (401)")
                }
                other => anyhow::anyhow!("Home Assistant request to {path} failed: {other}"),
            })
    }

    /// All lights with their areas, cached.
    pub fn lights(&mut self) -> Result<&[Light]> {
        if self.fetched.is_none_or(|at| at.elapsed() > REFRESH) {
            let text = self.template(LIGHTS_TEMPLATE)?;
            self.lights = serde_json::from_str(&text).context("parsing the light list from Home Assistant")?;
            self.fetched = Some(Instant::now());
        }
        Ok(&self.lights)
    }

    /// The lights a request names. `Ok(None)` when nothing matches.
    pub fn find(&mut self, request: &str) -> Result<Option<Target>> {
        Ok(resolve_heard(self.lights()?, request))
    }

    pub fn set(&self, target: &Target, on: bool) -> Result<()> {
        let service = if on { "turn_on" } else { "turn_off" };
        self.post(&format!("/api/services/light/{service}"), serde_json::json!({ "entity_id": target.ids }))?;
        Ok(())
    }

    /// Calls any service, e.g. `media_player.media_pause`.
    pub fn service(&self, domain: &str, service: &str, data: serde_json::Value) -> Result<()> {
        self.post_within(&format!("/api/services/{domain}/{service}"), data, SERVICE_TIMEOUT)?;
        Ok(())
    }

    /// Calls a service that answers, e.g. `music_assistant.search`; the answer.
    pub fn service_response(&self, domain: &str, service: &str, data: serde_json::Value) -> Result<serde_json::Value> {
        let mut body: serde_json::Value = self
            .post_within(&format!("/api/services/{domain}/{service}?return_response"), data, SERVICE_TIMEOUT)?
            .into_json()?;
        Ok(body["service_response"].take())
    }

    /// Renders a template.
    pub fn template(&self, template: &str) -> Result<String> {
        Ok(self.post("/api/template", serde_json::json!({ "template": template }))?.into_string()?)
    }

    /// Any GET under the API, e.g. `/api/states/light.kitchen`.
    pub fn get(&self, path: &str) -> Result<serde_json::Value> {
        self.agent
            .get(&format!("{}{path}", self.base))
            .set("Authorization", &format!("Bearer {}", self.token))
            .call()
            .map_err(|e| anyhow::anyhow!("Home Assistant request to {path} failed: {e}"))?
            .into_json()
            .map_err(Into::into)
    }

    /// An entity's state object (`state`, `attributes`).
    pub fn state(&self, entity_id: &str) -> Result<serde_json::Value> {
        self.get(&format!("/api/states/{entity_id}"))
    }

    /// The area of the first device with one of these names (friendly name or ESPHome name).
    pub fn device_area(&self, names: &[&str]) -> Result<Option<String>> {
        let template = format!(
            r#"{{% set ns = namespace(a="") %}}{{% for n in {} %}}{{% set d = device_id(n) %}}{{% if d and not ns.a %}}{{% set ns.a = area_name(d) or "" %}}{{% endif %}}{{% endfor %}}{{{{ ns.a }}}}"#,
            serde_json::to_string(names)?
        );
        let area = self.template(&template)?;
        let area = area.trim();
        Ok((!area.is_empty()).then(|| area.to_owned()))
    }

    /// Checks the URL and token without changing anything.
    pub fn check(&mut self) -> Result<usize> {
        let count = self.lights()?.len();
        if count == 0 {
            bail!("Home Assistant answered but reports no lights");
        }
        Ok(count)
    }
}

/// Like [`resolve`], but also finds a name the transcriber misspelled ("the round lamb", "the sync
/// light") by matching against the light and room names.
pub fn resolve_heard(lights: &[Light], request: &str) -> Option<Target> {
    if let Some(target) = resolve(lights, request) {
        return Some(target);
    }
    let names = crate::names::Names::new(lights.iter().flat_map(|l| [l.name.as_str(), l.area.as_str()]));
    let found = names.best(request)?;
    tracing::info!(heard = request, name = found.name, score = found.score, "light name matched");
    resolve(lights, found.name)
}

/// Whether a request names a room or light at all ("turn them off" does not).
pub fn names_something(request: &str) -> bool {
    request.split_whitespace().map(normalize).any(|w| !w.is_empty() && !FILLER.contains(&w.as_str()))
}

/// The room group a phrase belongs to, if any.
fn room_group(phrase: &str) -> Option<usize> {
    ROOMS.iter().position(|names| names.contains(&phrase))
}

/// Picks the lights a request names: a light named after the room (a Hue or HA room group, which
/// switches exactly the room's lamps), else every light in the area, else lights whose name
/// contains every word of the request.
pub fn resolve(lights: &[Light], request: &str) -> Option<Target> {
    let words: Vec<String> =
        request.split_whitespace().map(normalize).filter(|w| !w.is_empty() && !FILLER.contains(&w.as_str())).collect();
    if words.is_empty() {
        return None;
    }
    let phrase = words.join(" ");
    let group = room_group(&phrase).or_else(|| words.iter().find_map(|w| room_group(w)));

    let area_matches = |area: &str| {
        let area = normalize_phrase(area);
        !area.is_empty() && (area == phrase || (group.is_some() && room_group(&area) == group))
    };
    let room_light = lights.iter().find(|l| {
        let name = normalize_phrase(&l.name);
        name == phrase || (group.is_some() && room_group(&name) == group)
    });
    if let Some(light) = room_light {
        let label = if light.area.is_empty() { light.name.clone() } else { light.area.clone() };
        return Some(Target { label, ids: vec![light.id.clone()], area: true });
    }

    let in_area: Vec<&Light> = lights.iter().filter(|l| area_matches(&l.area)).collect();
    if let Some(first) = in_area.first() {
        return Some(Target {
            label: first.area.clone(),
            ids: in_area.iter().map(|l| l.id.clone()).collect(),
            area: true,
        });
    }

    let named: Vec<&Light> = lights
        .iter()
        .filter(|l| {
            let name = normalize_phrase(&l.name);
            words.iter().all(|w| name.split(' ').any(|n| n == w))
                || group.is_some_and(|g| ROOMS[g].iter().any(|room| name.contains(room)))
        })
        .collect();
    match named.as_slice() {
        [] => None,
        [one] => Some(Target { label: one.name.clone(), ids: vec![one.id.clone()], area: false }),
        many => Some(Target { label: phrase, ids: many.iter().map(|l| l.id.clone()).collect(), area: true }),
    }
}

fn normalize_phrase(text: &str) -> String {
    text.split_whitespace().map(normalize).filter(|w| !w.is_empty()).collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn light(id: &str, name: &str, area: &str) -> Light {
        Light { id: id.into(), name: name.into(), area: area.into() }
    }

    fn home() -> Vec<Light> {
        vec![
            light("light.ceiling", "Ceiling", "Living Room"),
            light("light.sofa_lamp", "Sofa lamp", "Living Room"),
            light("light.kitchen_spots", "Kitchen spots", "Kitchen"),
            light("light.desk", "Desk lamp", ""),
        ]
    }

    #[test]
    fn a_room_switches_every_light_in_its_area() {
        let t = resolve(&home(), "the light in my living room").unwrap();
        assert_eq!(t.label, "Living Room");
        assert_eq!(t.ids, vec!["light.ceiling", "light.sofa_lamp"]);
        assert_eq!(resolve(&home(), "the living room lights").unwrap().ids.len(), 2);
    }

    #[test]
    fn a_light_named_after_the_room_is_the_room() {
        // Like a Hue setup: the room group plus lamps and a network switch's status LED in the area.
        let lights = vec![
            light("light.fresh_lamp", "Fresh lamp", "Living Room"),
            light("light.living_room", "Living room", "Living Room"),
            light("light.usw_led", "USW-Lite-8-PoE LED", "Living Room"),
        ];
        let t = resolve(&lights, "turn off the light in my living room").unwrap();
        assert_eq!(t.ids, vec!["light.living_room"]);
        assert_eq!(t.label, "Living Room");
        assert!(t.area);
    }

    #[test]
    fn norwegian_room_names_match_english_areas_and_back() {
        assert_eq!(resolve(&home(), "lyset i stua").unwrap().label, "Living Room");
        let norsk = vec![light("light.taklampe", "Taklampe", "Stue")];
        assert_eq!(resolve(&norsk, "the living room light").unwrap().ids, vec!["light.taklampe"]);
    }

    #[test]
    fn a_light_name_switches_that_light() {
        let t = resolve(&home(), "the desk lamp").unwrap();
        assert_eq!(t.ids, vec!["light.desk"]);
        assert_eq!(t.label, "Desk lamp");
    }

    #[test]
    fn misheard_light_names_are_matched() {
        let lights = vec![
            light("light.round_lamp", "Round lamp", "Living Room"),
            light("light.sink", "Sink", "Kitchen"),
            light("light.fresh_lamp", "Fresh lamp", "Living Room"),
        ];
        assert_eq!(resolve_heard(&lights, "turn on the round lamb").unwrap().ids, ["light.round_lamp"]);
        assert_eq!(resolve_heard(&lights, "turn on the sync light").unwrap().ids, ["light.sink"]);
        assert_eq!(resolve_heard(&lights, "turn off the garage lights"), None);
    }

    #[test]
    fn pronouns_name_nothing() {
        assert!(!names_something("Turn off them."));
        assert!(!names_something("turn it back on again"));
        assert!(names_something("turn off the garage light"));
    }

    #[test]
    fn unknown_or_empty_requests_match_nothing() {
        assert_eq!(resolve(&home(), "the garage lights"), None);
        assert_eq!(resolve(&home(), "the lights"), None);
    }
}
