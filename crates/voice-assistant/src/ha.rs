//! Home Assistant over the REST API. Lights: one template call lists every light with its area,
//! then `light.turn_on` / `light.turn_off` switch the ones a request names. Scenes and scripts are
//! listed the same way and found by name ("set the living room to relax", "run movie time").

use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use crate::dialog::normalize;
use crate::intent::LightLevel;

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

/// Every scene or script, same shape as the lights.
const SCENES_TEMPLATE: &str = r#"[{% for s in states.scene %}{"id": {{ s.entity_id | tojson }}, "name": {{ s.name | tojson }}, "area": {{ (area_name(s.entity_id) or "") | tojson }}}{% if not loop.last %},{% endif %}{% endfor %}]"#;
const SCRIPTS_TEMPLATE: &str = r#"[{% for s in states.script %}{"id": {{ s.entity_id | tojson }}, "name": {{ s.name | tojson }}, "area": {{ (area_name(s.entity_id) or "") | tojson }}}{% if not loop.last %},{% endif %}{% endfor %}]"#;
const LIGHTS_ON_TEMPLATE: &str =
    r#"{{ states.light | selectattr("state", "eq", "on") | map(attribute="entity_id") | list | tojson }}"#;

/// Every person with their state: "home", "not_home" or a zone's name.
const PEOPLE_TEMPLATE: &str = r#"[{% for s in states.person %}{"name": {{ s.name | tojson }}, "state": {{ s.state | tojson }}}{% if not loop.last %},{% endif %}{% endfor %}]"#;

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Person {
    pub name: String,
    pub state: String,
}

/// Words in a scene request that are not the scene's or the room's name.
const SCENE_FILLER: &[&str] = &[
    "set",
    "the",
    "to",
    "scene",
    "scenes",
    "mode",
    "activate",
    "turn",
    "on",
    "in",
    "please",
    "lights",
    "light",
    "make",
    "it",
    "my",
    "use",
    "switch",
    "start",
    "put",
    "a",
    "sett",
    "til",
    "på",
    "i",
    "aktiver",
    "scenen",
    "scene",
    "modus",
    "lyset",
    "lysene",
    "gjør",
    "bruk",
    "stemning",
    "stemningen",
    "start",
    "can",
    "you",
    "could",
    "kan",
    "du",
    "for",
    "here",
    "her",
    "inne",
];
/// Words in a script request that are not the script's name.
const SCRIPT_FILLER: &[&str] = &[
    "run", "the", "script", "execute", "trigger", "please", "can", "you", "could", "kjør", "skript", "skriptet", "kan",
    "du",
];

/// A scene or script.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Named {
    pub id: String,
    pub name: String,
    pub area: String,
}

/// What a scene request resolved to.
#[derive(Debug, PartialEq)]
pub enum SceneMatch<'a> {
    Found(&'a Named),
    /// The scene exists in several rooms and the request named none.
    WhichRoom,
    NotFound,
}

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
    scenes: Vec<Named>,
    scripts: Vec<Named>,
    named_fetched: Option<Instant>,
}

impl HomeAssistant {
    pub fn new(base: &str, token: &str) -> Self {
        Self {
            base: base.trim_end_matches('/').to_owned(),
            token: token.to_owned(),
            agent: ureq::Agent::config_builder().timeout_global(Some(TIMEOUT)).build().into(),
            lights: Vec::new(),
            fetched: None,
            scenes: Vec::new(),
            scripts: Vec::new(),
            named_fetched: None,
        }
    }

    fn post(&self, path: &str, body: serde_json::Value) -> Result<ureq::http::Response<ureq::Body>> {
        self.post_within(path, body, TIMEOUT)
    }

    fn post_within(
        &self,
        path: &str,
        body: serde_json::Value,
        timeout: Duration,
    ) -> Result<ureq::http::Response<ureq::Body>> {
        self.agent
            .post(&format!("{}{path}", self.base))
            .config()
            .timeout_global(Some(timeout))
            .build()
            .header("Authorization", &format!("Bearer {}", self.token))
            .send_json(body)
            .map_err(|e| match e {
                ureq::Error::StatusCode(401) => {
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

    fn refresh_named(&mut self) -> Result<()> {
        if self.named_fetched.is_none_or(|at| at.elapsed() > REFRESH) {
            self.scenes = serde_json::from_str(&self.template(SCENES_TEMPLATE)?).context("parsing the scene list")?;
            self.scripts =
                serde_json::from_str(&self.template(SCRIPTS_TEMPLATE)?).context("parsing the script list")?;
            self.named_fetched = Some(Instant::now());
        }
        Ok(())
    }

    /// All scenes with their areas, cached.
    pub fn scenes(&mut self) -> Result<&[Named]> {
        self.refresh_named()?;
        Ok(&self.scenes)
    }

    /// All scripts, cached.
    pub fn scripts(&mut self) -> Result<&[Named]> {
        self.refresh_named()?;
        Ok(&self.scripts)
    }

    /// Brightness or colour; the lights are switched on if they were off.
    pub fn set_level(&self, target: &Target, level: &LightLevel) -> Result<()> {
        use serde_json::json;
        let ids = &target.ids;
        let (service, data) = match level {
            LightLevel::Percent(0) => ("turn_off", json!({ "entity_id": ids })),
            LightLevel::Percent(n) => ("turn_on", json!({ "entity_id": ids, "brightness_pct": n })),
            LightLevel::Brighter => ("turn_on", json!({ "entity_id": ids, "brightness_step_pct": 25 })),
            LightLevel::Dimmer => ("turn_on", json!({ "entity_id": ids, "brightness_step_pct": -25 })),
            LightLevel::Color(name) if name == "white" => {
                ("turn_on", json!({ "entity_id": ids, "color_temp_kelvin": 4000 }))
            }
            LightLevel::Color(name) => ("turn_on", json!({ "entity_id": ids, "color_name": name })),
            LightLevel::Warm => ("turn_on", json!({ "entity_id": ids, "color_temp_kelvin": 2700 })),
            LightLevel::Cool => ("turn_on", json!({ "entity_id": ids, "color_temp_kelvin": 5500 })),
        };
        self.post(&format!("/api/services/light/{service}"), data)?;
        Ok(())
    }

    /// Everyone Home Assistant tracks, and where they are.
    pub fn people(&self) -> Result<Vec<Person>> {
        serde_json::from_str(&self.template(PEOPLE_TEMPLATE)?).context("parsing the people from Home Assistant")
    }

    /// Turns on a scene or runs a script (both are `<domain>.turn_on`).
    pub fn activate(&self, entity: &Named) -> Result<()> {
        let domain = entity.id.split('.').next().unwrap_or("scene");
        self.service(domain, "turn_on", serde_json::json!({ "entity_id": entity.id }))
    }

    /// The lights that are on now (not cached).
    pub fn lights_on(&mut self) -> Result<Vec<Light>> {
        let ids: Vec<String> =
            serde_json::from_str(&self.template(LIGHTS_ON_TEMPLATE)?).context("parsing the lights that are on")?;
        Ok(self.lights()?.iter().filter(|l| ids.contains(&l.id)).cloned().collect())
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
            .body_mut()
            .read_json()?;
        Ok(body["service_response"].take())
    }

    /// Renders a template.
    pub fn template(&self, template: &str) -> Result<String> {
        Ok(self.post("/api/template", serde_json::json!({ "template": template }))?.body_mut().read_to_string()?)
    }

    /// Any GET under the API, e.g. `/api/states/light.kitchen`.
    pub fn get(&self, path: &str) -> Result<serde_json::Value> {
        self.agent
            .get(&format!("{}{path}", self.base))
            .header("Authorization", &format!("Bearer {}", self.token))
            .call()
            .map_err(|e| anyhow::anyhow!("Home Assistant request to {path} failed: {e}"))?
            .body_mut()
            .read_json()
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

/// The scene a request names. The room is the one named in the request, else `here` (the
/// satellite's room); a scene name found only in other rooms still counts when it is in exactly
/// one of them.
pub fn resolve_scene<'a>(scenes: &'a [Named], request: &str, here: Option<&str>) -> SceneMatch<'a> {
    let mut words: Vec<String> = request
        .split_whitespace()
        .map(normalize)
        .filter(|w| !w.is_empty() && !SCENE_FILLER.contains(&w.as_str()))
        .collect();
    let named_room = take_room(&mut words);
    if words.is_empty() {
        return SceneMatch::NotFound;
    }
    let wanted = crate::names::sound_key(&words.join(" "));
    let room = named_room.or_else(|| here.and_then(|h| room_group(&normalize_phrase(h))));
    let here_area = here.map(normalize_phrase);
    let in_room = |scene: &Named| {
        let area = normalize_phrase(&scene.area);
        match room {
            Some(group) => room_group(&area) == Some(group),
            None => here_area.as_deref().is_some_and(|h| h == area),
        }
    };
    let score = |scene: &Named| {
        let own = crate::names::sound_key(&scene_own_name(scene));
        // "reading" for "Read", "relaxing" for "Relax".
        let stem = own.len() >= 3 && wanted.starts_with(&own) && wanted.len() <= own.len() + 3;
        if stem {
            0.9f32.max(crate::names::similarity(&wanted, &own))
        } else {
            crate::names::similarity(&wanted, &own)
        }
    };
    // Hue names are "<room> <scene>"; one wrong letter in "relax" still leaves it closest.
    const MIN: f32 = 0.7;
    let best_of = |candidates: Vec<&'a Named>| -> Vec<(&'a Named, f32)> {
        let mut scored: Vec<(&Named, f32)> =
            candidates.into_iter().map(|s| (s, score(s))).filter(|(_, sc)| *sc >= MIN).collect();
        scored.sort_by(|a, b| b.1.total_cmp(&a.1));
        scored
    };
    if room.is_some() || here_area.is_some() {
        if let Some((scene, _)) = best_of(scenes.iter().filter(|s| in_room(s)).collect()).first() {
            return SceneMatch::Found(scene);
        }
        if named_room.is_some() {
            return SceneMatch::NotFound;
        }
    }
    let scored = best_of(scenes.iter().collect());
    let Some(&(first, top)) = scored.first() else { return SceneMatch::NotFound };
    let tied: Vec<&Named> = scored.iter().filter(|(_, sc)| (top - sc).abs() < 1e-6).map(|(s, _)| *s).collect();
    let areas: std::collections::HashSet<&str> = tied.iter().map(|s| s.area.as_str()).collect();
    if areas.len() > 1 {
        SceneMatch::WhichRoom
    } else {
        SceneMatch::Found(first)
    }
}

/// A scene's name without its room: "Living room Relax" is "relax".
fn scene_own_name(scene: &Named) -> String {
    let name = normalize_phrase(&scene.name);
    let area = normalize_phrase(&scene.area);
    match name.strip_prefix(&format!("{area} ")) {
        Some(rest) if !area.is_empty() => rest.to_owned(),
        _ => name,
    }
}

/// Removes a room name from the words; its group.
fn take_room(words: &mut Vec<String>) -> Option<usize> {
    for len in [2, 1] {
        for start in 0..words.len().saturating_sub(len - 1) {
            if let Some(group) = room_group(&words[start..start + len].join(" ")) {
                words.drain(start..start + len);
                return Some(group);
            }
        }
    }
    None
}

/// The script a request names ("run movie time", "kjør movie time"), allowing for mishearings.
pub fn resolve_script<'a>(scripts: &'a [Named], request: &str) -> Option<&'a Named> {
    let words: Vec<String> = request
        .split_whitespace()
        .map(normalize)
        .filter(|w| !w.is_empty() && !SCRIPT_FILLER.contains(&w.as_str()))
        .collect();
    if words.is_empty() {
        return None;
    }
    let phrase = words.join(" ");
    if let Some(exact) = scripts.iter().find(|s| normalize_phrase(&s.name) == phrase) {
        return Some(exact);
    }
    let names = crate::names::Names::new(scripts.iter().map(|s| s.name.as_str()));
    let found = names.best(&phrase)?;
    scripts.iter().find(|s| s.name == found.name)
}

/// A scene or script whose whole name is the request: "movie time", "living room relax". Makes
/// every Home Assistant script a voice command without a verb.
pub fn exact<'a>(entities: &'a [Named], request: &str) -> Option<&'a Named> {
    let phrase = normalize_phrase(request);
    let polite: &[&str] = &["please", "takk", "ok", "okay"];
    let phrase: Vec<&str> = phrase.split(' ').filter(|w| !polite.contains(w)).collect();
    let phrase = phrase.join(" ");
    entities.iter().find(|e| !phrase.is_empty() && normalize_phrase(&e.name) == phrase)
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

    fn scenes() -> Vec<Named> {
        let scene = |id: &str, name: &str, area: &str| Named { id: id.into(), name: name.into(), area: area.into() };
        vec![
            scene("scene.living_room_relax", "Living room Relax", "Living Room"),
            scene("scene.living_room_read", "Living room Read", "Living Room"),
            scene("scene.bedroom_relax", "Bedroom Relax", "Bedroom"),
            scene("scene.bedroom_nightlight", "Bedroom Nightlight", "Bedroom"),
            scene("scene.bedroom_dreamy_dusk", "Bedroom Dreamy dusk", "Bedroom"),
        ]
    }

    #[test]
    fn scenes_by_room_and_name() {
        let found = |request: &str, here: Option<&str>| match resolve_scene(&scenes(), request, here) {
            SceneMatch::Found(scene) => Some(scene.id.clone()),
            _ => None,
        };
        let id = |s: &str| Some(s.to_owned());
        assert_eq!(found("set the living room to relax", None), id("scene.living_room_relax"));
        assert_eq!(found("activate the reading scene in the living room", None), id("scene.living_room_read"));
        assert_eq!(found("bedroom dreamy dusk", None), id("scene.bedroom_dreamy_dusk"));
        assert_eq!(found("sett soverommet til relax", None), id("scene.bedroom_relax"));
        // The satellite's room decides between rooms; a scene only one room has needs none.
        assert_eq!(found("relax mode", Some("Bedroom")), id("scene.bedroom_relax"));
        assert_eq!(found("nightlight scene", Some("Living Room")), id("scene.bedroom_nightlight"));
        assert_eq!(resolve_scene(&scenes(), "relax mode", None), SceneMatch::WhichRoom);
        assert_eq!(resolve_scene(&scenes(), "set the kitchen to relax", None), SceneMatch::NotFound);
        assert_eq!(resolve_scene(&scenes(), "party mode", Some("Bedroom")), SceneMatch::NotFound);
    }

    #[test]
    fn scripts_by_name() {
        let scripts = vec![
            Named { id: "script.movie_time".into(), name: "Movie time".into(), area: String::new() },
            Named { id: "script.toggle_house_lights".into(), name: "Toggle House Lights".into(), area: String::new() },
        ];
        assert_eq!(resolve_script(&scripts, "run movie time").unwrap().id, "script.movie_time");
        assert_eq!(resolve_script(&scripts, "kjør skriptet movie time").unwrap().id, "script.movie_time");
        assert_eq!(resolve_script(&scripts, "run the moovie time script").unwrap().id, "script.movie_time");
        assert_eq!(resolve_script(&scripts, "run the dishwasher"), None);
        assert_eq!(exact(&scripts, "Movie time, please.").unwrap().id, "script.movie_time");
        assert_eq!(exact(&scripts, "what time is the movie"), None);
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
