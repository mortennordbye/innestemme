//! The house beyond the lights: a status check, routines for leaving, coming home and going to bed,
//! sunrise and sunset, finding a phone, the guest Wi-Fi, the camera's privacy mode and the alarm.
//! The rules are here, and so are the Home Assistant calls; the engine composes the routines,
//! since they also switch lights.

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::alerts::{self, Alert};
use crate::ha::{HomeAssistant, Light};
use crate::lang::Lang;
use crate::names::Names;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Switch {
    On,
    Off,
    Status,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SunEvent {
    Sunrise,
    Sunset,
    /// "When does it get dark?": dusk.
    Dark,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HouseCommand {
    /// "Is everything okay at home?"
    Status,
    Sun(SunEvent),
    /// Ring a phone through the Home Assistant app; `None` is "my phone".
    FindPhone(Option<String>),
    PhoneBattery(Option<String>),
    GuestWifi(Switch),
    /// The camera: `On` records, `Off` is privacy mode.
    Camera(Switch),
    /// The alarm system. `Off` is refused: anyone at the door could ask.
    Alarm(Switch),
    /// Temperature or humidity indoors: "how warm is it in the bedroom".
    Climate(Indoors, Reading),
}

/// Where an indoor climate question is about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Indoors {
    /// "Inside", "at home": every room with a sensor.
    Everywhere,
    /// "In here": the satellite's room, else every room.
    Here,
    /// A room by name, as [`ha::room_named`](crate::ha::room_named) gives it or as the language
    /// model said it.
    Room(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reading {
    Temperature,
    Humidity,
    Both,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Routine {
    /// "I'm leaving": everything off.
    Leaving,
    /// "I'm home": a welcome, and the room's lights when it is dark.
    Home,
    /// "I'm going to bed": everything off except the bedroom.
    Bedtime,
    /// "Initiate lockdown": every light red, and epic music. Just for fun; nothing is locked.
    Lockdown,
    /// "Stand down": the lights and volume back as they were before the lockdown.
    StandDown,
}

/// Words before a request that do not change it: "okay, I'm leaving", "so, is everything okay".
const LEADING: &[&str] = &["okay", "ok", "so", "well", "right", "alright", "hey", "please", "can", "you", "could"];

/// The words without leading filler, joined by spaces.
fn phrase(words: &[String]) -> String {
    let start = words.iter().position(|w| !LEADING.contains(&w.as_str())).unwrap_or(words.len());
    words[start..].join(" ")
}

fn has_any(words: &[String], list: &[&str]) -> bool {
    words.iter().any(|w| list.contains(&w.as_str()))
}

/// `text` contains `part` as whole words.
fn contains_words(text: &str, part: &str) -> bool {
    format!(" {text} ").contains(&format!(" {part} "))
}

/// A question rather than a command: "is the guest wifi on", "er kameraet på".
fn asks(words: &[String]) -> bool {
    let first = phrase(words);
    let first = first.split(' ').next().unwrap_or_default();
    ["is", "are", "was", "what", "what's", "how", "how's", "does", "do", "er", "hva", "hvordan", "står"]
        .contains(&first)
}

/// Switching words, off first: in Norwegian "på" is also the preposition ("skru av lyset på badet").
fn switch_of(words: &[String]) -> Option<bool> {
    let off = ["off", "disable", "deactivate", "stop", "close", "shut", "av", "deaktiver", "stopp", "steng"];
    let on = ["on", "enable", "activate", "start", "open", "resume", "back", "på", "aktiver", "åpne"];
    if has_any(words, &off) {
        Some(false)
    } else if has_any(words, &on) {
        Some(true)
    } else {
        None
    }
}

pub fn parse(words: &[String]) -> Option<HouseCommand> {
    if words.is_empty() {
        return None;
    }
    if let Some(event) = sun(words) {
        return Some(HouseCommand::Sun(event));
    }
    if let Some(command) = phone(words) {
        return Some(command);
    }
    if let Some(command) = guest_wifi(words) {
        return Some(HouseCommand::GuestWifi(command));
    }
    if let Some(command) = camera(words) {
        return Some(HouseCommand::Camera(command));
    }
    if let Some(command) = alarm(words) {
        return Some(HouseCommand::Alarm(command));
    }
    if let Some((place, reading)) = climate(words) {
        return Some(HouseCommand::Climate(place, reading));
    }
    is_status(words).then_some(HouseCommand::Status)
}

const TEMPERATURE_WORDS: &[&str] = &[
    "temperature",
    "temp",
    "warm",
    "hot",
    "cold",
    "chilly",
    "freezing",
    "degrees",
    "varm",
    "varmt",
    "kald",
    "kaldt",
    "grader",
    "iskaldt",
];
const HUMIDITY_WORDS: &[&str] =
    &["humid", "humidity", "dry", "damp", "muggy", "fukt", "fuktig", "fuktighet", "fuktigheten", "tørr", "tørt"];
/// Both readings: "what's the climate in the bedroom", "inneklimaet".
const CLIMATE_WORDS: &[&str] = &["climate", "inneklima", "inneklimaet", "klima", "klimaet"];
const INSIDE_WORDS: &[&str] =
    &["inside", "indoors", "inne", "innendørs", "hjemme", "innetemperatur", "innetemperaturen"];
const INSIDE_PHRASES: &[&str] =
    &["at home", "in the house", "in the flat", "in the apartment", "i huset", "i leiligheten"];
/// Outdoor words leave it to the weather: "is it cold outside", "how warm is it in the garden tomorrow".
const OUTDOOR_WORDS: &[&str] = &[
    "outside",
    "outdoors",
    "weather",
    "forecast",
    "tomorrow",
    "rain",
    "snow",
    "wind",
    "ute",
    "utenfor",
    "utendørs",
    "været",
    "vær",
    "morgen",
    "imorgen",
    "regn",
    "snø",
    "vind",
];
/// A command, not a question: there is no heating to set, so "make it warmer" goes to the language model.
const COMMAND_WORDS: &[&str] =
    &["make", "set", "turn", "raise", "lower", "increase", "decrease", "gjør", "sett", "skru", "øk", "senk"];

/// "How warm is it in the bedroom", "is it humid in the bathroom", "what's the temperature inside",
/// "it's freezing in here", "hvor varmt er det i stua".
fn climate(words: &[String]) -> Option<(Indoors, Reading)> {
    let temperature = has_any(words, TEMPERATURE_WORDS) || words.iter().any(|w| w.contains("peratur"));
    let humidity = has_any(words, HUMIDITY_WORDS) || words.iter().any(|w| w.starts_with("luftfukt"));
    let both = has_any(words, CLIMATE_WORDS);
    if !(temperature || humidity || both) {
        return None;
    }
    let first = phrase(words);
    let first = first.split(' ').next().unwrap_or_default();
    // "Make the bedroom lights warmer" is the lights.
    let light = has_any(words, &["light", "lights", "lamp", "lamps", "lys", "lyset", "lysene", "lampen", "lampene"]);
    if has_any(words, OUTDOOR_WORDS) || COMMAND_WORDS.contains(&first) || light {
        return None;
    }
    let text = phrase(words);
    let here = ["in here", "her inne", "this room", "dette rommet"].iter().any(|p| contains_words(&text, p));
    let place = if let Some(room) = crate::ha::room_named(words) {
        Indoors::Room(room.to_owned())
    } else if here {
        Indoors::Here
    } else if has_any(words, INSIDE_WORDS) || INSIDE_PHRASES.iter().any(|p| contains_words(&text, p)) {
        Indoors::Everywhere
    } else {
        return None;
    };
    let reading = match (temperature, humidity) {
        _ if both => Reading::Both,
        (true, true) => Reading::Both,
        (false, true) => Reading::Humidity,
        _ => Reading::Temperature,
    };
    Some((place, reading))
}

const STATUS_PHRASES: &[&str] = &[
    "is everything okay",
    "is everything ok",
    "is everything alright",
    "is everything all right",
    "is everything fine",
    "is everything good",
    "is everything off",
    "is all well",
    "everything okay at home",
    "everything good at home",
    "everything alright at home",
    "house status",
    "home status",
    "what's the house status",
    "what is the house status",
    "what's the home status",
    "what is the home status",
    "what's the status at home",
    "what is the status at home",
    "status report",
    "status update",
    "how's the house",
    "how is the house",
    "how's the home",
    "how's everything at home",
    "how is everything at home",
    "anything i should know",
    "is there anything i should know",
    "er alt i orden",
    "er alt bra hjemme",
    "er alt ok hjemme",
    "statusrapport",
    "hvordan står det til hjemme",
];

fn is_status(words: &[String]) -> bool {
    let text = phrase(words);
    words.len() <= 9 && STATUS_PHRASES.iter().any(|p| text.starts_with(p) && contains_words(&text, p))
}

fn sun(words: &[String]) -> Option<SunEvent> {
    let text = phrase(words);
    let has = |w: &str| words.iter().any(|x| x == w);
    let when = ["when", "when's", "what", "how", "når", "hvor"].iter().any(|w| has(w)) || has("time");
    let sun = ["sun", "solen", "sola"].iter().any(|w| has(w));
    if ["sunset", "sundown", "solnedgang", "solnedgangen"].iter().any(|w| has(w))
        || (sun && (has("set") || has("sets") || has("down") || has("ned")))
    {
        return Some(SunEvent::Sunset);
    }
    if ["sunrise", "sunup", "soloppgang", "soloppgangen"].iter().any(|w| has(w))
        || (sun && (has("rise") || has("rises") || has("up") || has("opp")))
    {
        return Some(SunEvent::Sunrise);
    }
    let dark =
        ["get dark", "gets dark", "it dark", "blir det mørkt", "blir mørkt"].iter().any(|p| contains_words(&text, p));
    if when && dark {
        return Some(SunEvent::Dark);
    }
    let light = ["get light", "gets light", "blir det lyst"].iter().any(|p| contains_words(&text, p));
    (when && light).then_some(SunEvent::Sunrise)
}

const PHONE_WORDS: &[&str] = &[
    "phone",
    "phones",
    "iphone",
    "cellphone",
    "mobile",
    "telefon",
    "telefonen",
    "telefonene",
    "mobil",
    "mobilen",
    "mobiltelefon",
    "mobiltelefonen",
];

fn phone(words: &[String]) -> Option<HouseCommand> {
    let at = words.iter().position(|w| PHONE_WORDS.contains(&w.as_str()))?;
    let battery = ["battery", "batteri", "batteriet", "charge", "charged", "charging", "ladet", "lader", "strøm"];
    let find = [
        "where", "where's", "find", "ring", "call", "locate", "lost", "can't", "cannot", "hvor", "finn", "finner",
        "mistet",
    ];
    // "Kristine's phone", "telefonen til Kristine"; "my phone" is nobody in particular.
    let name = at
        .checked_sub(1)
        .and_then(|i| words[i].strip_suffix("'s"))
        .or_else(|| {
            (words.get(at + 1).map(String::as_str) == Some("til"))
                .then(|| words.get(at + 2))
                .flatten()
                .map(String::as_str)
        })
        .filter(|n| !["my", "the", "your", "min", "mi"].contains(n))
        .map(str::to_owned);
    if has_any(words, &battery) {
        return Some(HouseCommand::PhoneBattery(name));
    }
    has_any(words, &find).then_some(HouseCommand::FindPhone(name))
}

const GUEST_WORDS: &[&str] = &["guest", "guests", "gjest", "gjeste"];
const NET_WORDS: &[&str] =
    &["wifi", "wireless", "network", "net", "wlan", "nett", "nettet", "nettverk", "nettverket", "trådløs"];
const GUEST_NETS: &[&str] = &["gjestenett", "gjestenettet", "gjestenettverk", "gjestenettverket", "guestwifi"];

fn guest_wifi(words: &[String]) -> Option<Switch> {
    // "Wi-Fi" is "wifi" once normalized; "wi fi" is two words.
    let net = has_any(words, NET_WORDS) || words.windows(2).any(|p| p[0] == "wi" && p[1] == "fi");
    if !(has_any(words, GUEST_NETS) || (has_any(words, GUEST_WORDS) && net)) {
        return None;
    }
    Some(switch_command(words))
}

/// Status for a question, else on or off; nothing said is a question.
fn switch_command(words: &[String]) -> Switch {
    if asks(words) {
        return Switch::Status;
    }
    match switch_of(words) {
        Some(true) => Switch::On,
        Some(false) => Switch::Off,
        None => Switch::Status,
    }
}

fn camera(words: &[String]) -> Option<Switch> {
    let camera = has_any(words, &["camera", "cameras", "kamera", "kameraet", "kameraene", "cctv"]);
    let privacy = has_any(words, &["privacy", "personvern", "personvernmodus"]);
    let recording = has_any(words, &["recording", "record", "opptak", "opptaket", "filming"]);
    if !(camera || privacy || recording) {
        return None;
    }
    let command = switch_command(words);
    // Privacy on is the camera off.
    if privacy {
        return Some(match command {
            Switch::On => Switch::Off,
            Switch::Off => Switch::On,
            Switch::Status => Switch::Status,
        });
    }
    if !camera && command == Switch::Status && !asks(words) {
        return None;
    }
    Some(command)
}

fn alarm(words: &[String]) -> Option<Switch> {
    // Strict words only: "turn off the alarm" is a ringing timer, "set an alarm" a wake-up time.
    let arm = has_any(words, &["arm", "armed", "disarm", "disarmed"]);
    let system =
        has_any(words, &["security", "burglar", "alarmen", "innbruddsalarm", "innbruddsalarmen", "boligalarmen"])
            || ["alarm system", "house alarm", "home alarm"].iter().any(|p| contains_words(&words.join(" "), p));
    if !(arm || system) {
        return None;
    }
    if asks(words) {
        return Some(Switch::Status);
    }
    if has_any(words, &["disarm", "disarmed"]) {
        return Some(Switch::Off);
    }
    if has_any(words, &["arm", "set"]) {
        return Some(Switch::On);
    }
    Some(switch_command(words))
}

const LEAVING: &[&str] = &[
    "i'm leaving",
    "i am leaving",
    "we're leaving",
    "we are leaving",
    "i'm heading out",
    "i am heading out",
    "we're heading out",
    "heading out",
    "i'm going out",
    "i am going out",
    "we're going out",
    "i'm off to work",
    "i'm off now",
    "leaving now",
    "jeg går nå",
    "jeg drar",
    "jeg drar nå",
    "vi drar",
    "vi går nå",
    "jeg stikker",
];
const HOME: &[&str] = &[
    "i'm home",
    "i am home",
    "we're home",
    "we are home",
    "i'm back",
    "i am back",
    "we're back",
    "honey i'm home",
    "daddy's home",
    "daddy is home",
    "dad's home",
    "dad is home",
    "pappa er hjemme",
    "jeg er hjemme",
    "vi er hjemme",
    "jeg er tilbake",
    "vi er tilbake",
];
const BEDTIME: &[&str] = &[
    "i'm going to bed",
    "i am going to bed",
    "we're going to bed",
    "going to bed",
    "i'm off to bed",
    "i'm going to sleep",
    "going to sleep",
    "time for bed",
    "bedtime",
    "bed time",
    "good night",
    "goodnight",
    "night night",
    "god natt",
    "natta",
    "jeg legger meg",
    "vi legger oss",
    "leggetid",
];

const LOCKDOWN: &[&str] = &[
    "lockdown",
    "lock down",
    "lockdown mode",
    "initiate lockdown",
    "initiate lock down",
    "initiate the lockdown",
    "activate lockdown",
    "start lockdown",
    "start the lockdown",
    "lock down the house",
    "lock the house down",
    "red alert",
    "code red",
];
const STAND_DOWN: &[&str] = &[
    "stand down",
    "end lockdown",
    "end the lockdown",
    "stop lockdown",
    "stop the lockdown",
    "lift lockdown",
    "lift the lockdown",
    "cancel lockdown",
    "cancel the lockdown",
    "lockdown off",
    "all clear",
];

/// Fixed English lockdown answers, rendered ahead of time.
pub const LOCKDOWN_ANSWERS: &[&str] = &[
    "Lockdown initiated. The house is sealed.",
    "Standing down. Everything is back as it was.",
    "There is no lockdown to end.",
];

/// The fixed sentences of leaving, coming home and bedtime, for rendering ahead of time.
pub const ROUTINE_ANSWERS: &[&str] = &[
    "Okay, the lights are off.",
    "Okay, the lights are off, except in the bedroom.",
    "Okay, everything is already off.",
    "The music is paused.",
    "The TV is off.",
    "The bedroom lights are dimmed.",
    "The alarm is off. Say arm the alarm to set it.",
    "The alarm is still armed.",
    "See you later.",
    "Good night.",
    "Welcome home.",
];

pub fn routine(words: &[String]) -> Option<Routine> {
    if words.len() > 9 {
        return None;
    }
    let text = phrase(words);
    let starts = |list: &[&str]| list.iter().any(|p| text == *p || text.starts_with(&format!("{p} ")));
    if starts(STAND_DOWN) {
        return Some(Routine::StandDown);
    }
    if starts(LOCKDOWN) {
        return Some(Routine::Lockdown);
    }
    // The transcriber mangles the words around it ("Initially lockdown.", "In the shape, lock
    // down."), but the word itself in a short request is enough. A question about it is not one.
    let lockdown = ["lockdown", "lock down", "code red"].iter().any(|w| text.contains(w));
    let asks =
        words.first().is_some_and(|w| ["is", "are", "what", "what's", "why", "when", "how"].contains(&w.as_str()));
    if lockdown && words.len() <= 6 && !asks {
        return Some(Routine::Lockdown);
    }
    // Before leaving: "I'm off to bed" is bedtime.
    if starts(BEDTIME) {
        return Some(Routine::Bedtime);
    }
    if starts(HOME) {
        return Some(Routine::Home);
    }
    starts(LEAVING).then_some(Routine::Leaving)
}

// --- Home Assistant ----------------------------------------------------------------------------

/// Phones with the Home Assistant app: notify service, tracker state, battery and owner.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Phone {
    /// The device id: `notify.mobile_app_<id>`, `device_tracker.<id>`.
    pub id: String,
    pub name: String,
    #[serde(rename = "where")]
    pub location: String,
    #[serde(default)]
    pub battery: Value,
    #[serde(default)]
    pub charging: String,
    #[serde(default)]
    pub owner: String,
}

impl Phone {
    fn battery_percent(&self) -> Option<u32> {
        match &self.battery {
            Value::Number(n) => n.as_f64(),
            Value::String(s) => s.parse().ok(),
            _ => None,
        }
        .map(|b| b.round() as u32)
    }

    fn owner_first_name(&self) -> Option<&str> {
        self.owner.split_whitespace().next()
    }
}

/// Everything a status check reports, apart from alerts and lights.
#[derive(Debug, Default, Deserialize)]
pub struct Snapshot {
    /// The first alarm panel's state; empty without one.
    #[serde(default)]
    pub alarm: String,
    /// TVs that are on.
    #[serde(default)]
    pub tvs: Vec<String>,
    /// Music Assistant players that are playing.
    #[serde(default)]
    pub playing: Vec<Named>,
    /// Privacy switches that are on.
    #[serde(default)]
    pub privacy: Vec<String>,
    #[serde(default)]
    pub low_batteries: Vec<Battery>,
    /// The last motion a camera saw, an ISO timestamp.
    #[serde(default)]
    pub motion: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Named {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Battery {
    pub name: String,
    pub level: f64,
}

const SNAPSHOT_TEMPLATE: &str = r#"{% set tvs = states.media_player | rejectattr('state', 'in', ['off', 'standby', 'unavailable', 'unknown']) | rejectattr('attributes.mass_player_type', 'defined') | list %}
{"alarm": {{ (states.alarm_control_panel | map(attribute='state') | first | default('')) | tojson }},
"tvs": [{% for s in tvs if s.attributes.device_class == 'tv' or 'tv' in (s.name | lower) %}{{ s.name | tojson }}{% if not loop.last %},{% endif %}{% endfor %}],
"playing": [{% for s in states.media_player | selectattr('state', 'eq', 'playing') | selectattr('attributes.mass_player_type', 'defined') %}{"id": {{ s.entity_id | tojson }}, "name": {{ s.name | tojson }}}{% if not loop.last %},{% endif %}{% endfor %}],
"privacy": [{% for s in states.switch | selectattr('state', 'eq', 'on') if 'privacy' in s.entity_id %}{{ s.name | tojson }}{% if not loop.last %},{% endif %}{% endfor %}],
"low_batteries": [{% for s in states.sensor | selectattr('attributes.device_class', 'defined') | selectattr('attributes.device_class', 'eq', 'battery') if s.state | float(100) < 20 and not s.entity_id.endswith('_battery_level') %}{"name": {{ s.name | tojson }}, "level": {{ s.state | float }}}{% if not loop.last %},{% endif %}{% endfor %}],
"motion": {{ (states.event | selectattr('entity_id', 'search', 'motion') | map(attribute='state') | reject('in', ['unknown', 'unavailable']) | sort | last | default('')) | tojson }}}"#;

/// One entity of a domain whose id contains a word, with its state.
#[derive(Debug, Clone, Deserialize)]
pub struct Entity {
    pub id: String,
    pub name: String,
    pub state: String,
    #[serde(default)]
    pub clients: String,
}

fn entities_template(domain: &str, word: &str) -> String {
    format!(
        r#"[{{% for s in states.{domain} if '{word}' in s.entity_id and s.state != 'unavailable' %}}{{"id": {{{{ s.entity_id | tojson }}}}, "name": {{{{ s.name | tojson }}}}, "state": {{{{ s.state | tojson }}}}, "clients": {{{{ states('sensor.' ~ s.object_id) | tojson }}}}}}{{% if not loop.last %}},{{% endif %}}{{% endfor %}}]"#
    )
}

pub fn entities(ha: &HomeAssistant, domain: &str, word: &str) -> Result<Vec<Entity>> {
    serde_json::from_str(&ha.template(&entities_template(domain, word))?)
        .with_context(|| format!("parsing {domain} entities"))
}

pub fn snapshot(ha: &HomeAssistant) -> Result<Snapshot> {
    serde_json::from_str(&ha.template(SNAPSHOT_TEMPLATE)?).context("parsing the house snapshot")
}

/// Phones with the Home Assistant app.
pub fn phones(ha: &HomeAssistant) -> Result<Vec<Phone>> {
    let services = ha.get("/api/services")?;
    let ids: Vec<String> = services
        .as_array()
        .into_iter()
        .flatten()
        .filter(|d| d["domain"] == "notify")
        .flat_map(|d| d["services"].as_object().into_iter().flat_map(|s| s.keys().cloned()))
        .filter_map(|s| s.strip_prefix("mobile_app_").map(str::to_owned))
        .collect();
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let template = format!(
        r#"{{% set ids = {} %}}[{{% for id in ids %}}{{% set t = 'device_tracker.' ~ id %}}{{% set b = states('sensor.' ~ id ~ '_battery_level') %}}{{"id": {{{{ id | tojson }}}}, "name": {{{{ (state_attr(t, 'friendly_name') or id) | tojson }}}}, "where": {{{{ states(t) | tojson }}}}, "battery": {{{{ (b if b not in ['unknown', 'unavailable'] else state_attr(t, 'battery_level')) | tojson }}}}, "charging": {{{{ states('sensor.' ~ id ~ '_battery_state') | tojson }}}}, "owner": {{{{ (states.person | selectattr('attributes.device_trackers', 'defined') | selectattr('attributes.device_trackers', 'contains', t) | map(attribute='name') | first | default('')) | tojson }}}}}}{{% if not loop.last %}},{{% endif %}}{{% endfor %}}]"#,
        serde_json::to_string(&ids)?
    );
    serde_json::from_str(&ha.template(&template)?).context("parsing the phones")
}

/// The phones a request means: by owner or device name; without one, the only phone, else those at home.
pub fn pick_phones<'a>(phones: &'a [Phone], name: Option<&str>) -> Vec<&'a Phone> {
    if let Some(name) = name {
        let labels: Vec<String> =
            phones.iter().flat_map(|p| [p.owner_first_name().unwrap_or_default().to_owned(), p.name.clone()]).collect();
        let names = Names::new(&labels);
        let Some(found) = names.best(name) else { return Vec::new() };
        return phones.iter().filter(|p| p.owner_first_name() == Some(found.name) || p.name == found.name).collect();
    }
    if phones.len() <= 1 {
        return phones.iter().collect();
    }
    let home: Vec<&Phone> = phones.iter().filter(|p| p.location == "home").collect();
    if home.is_empty() {
        phones.iter().collect()
    } else {
        home
    }
}

/// Rings a phone loudly, through silent mode where the app may (iOS critical alerts, Android's
/// alarm stream).
pub fn ring(ha: &HomeAssistant, phone: &Phone, name: &str) -> Result<()> {
    ha.send(
        "notify",
        &format!("mobile_app_{}", phone.id),
        json!({
            "title": format!("{name} is looking for this phone"),
            "message": "Here it is.",
            "data": {
                "push": { "sound": { "name": "default", "critical": 1, "volume": 1.0 } },
                "ttl": 0,
                "priority": "high",
                "channel": "alarm_stream",
            },
        }),
    )
}

/// "Your phone", "Kristine's phone".
fn whose(phone: &Phone, named: bool, no: bool) -> String {
    match (named, phone.owner_first_name(), no) {
        (true, Some(owner), false) => format!("{owner}'s phone"),
        (true, Some(owner), true) => format!("telefonen til {owner}"),
        (true, None, _) => phone.name.clone(),
        (false, _, false) => "your phone".into(),
        (false, _, true) => "telefonen din".into(),
    }
}

/// The answer to "where's my phone" once the phones have rung.
pub fn rang_answer(rung: &[&Phone], name: Option<&str>, lang: Lang) -> String {
    let no = lang == Lang::Norwegian;
    match (rung, name) {
        ([], Some(name)) if no => {
            format!("Jeg kan ikke ringe telefonen til {name}; den har ikke Home Assistant-appen.")
        }
        ([], Some(name)) => format!("I can't ring {name}'s phone; it doesn't have the Home Assistant app."),
        ([], None) if no => "Jeg ser ingen telefoner med Home Assistant-appen.".into(),
        ([], None) => "I can only ring phones with the Home Assistant app, and I don't see any.".into(),
        ([one], _) => {
            let who = whose(one, name.is_some(), no);
            let mut answer = if no { format!("Ok, jeg ringer {who}.") } else { format!("Okay, I'm ringing {who}.") };
            match (one.location.as_str(), no) {
                ("home" | "unknown" | "unavailable", _) => {}
                ("not_home", false) => answer.push_str(" It isn't at home, though."),
                ("not_home", true) => answer.push_str(" Den er ikke hjemme, forresten."),
                (zone, false) => answer.push_str(&format!(" Home Assistant has it at {zone}.")),
                (zone, true) => answer.push_str(&format!(" Home Assistant har den på {zone}.")),
            }
            answer
        }
        (_, _) if no => "Ok, jeg ringer telefonene.".into(),
        (_, _) => "Okay, I'm ringing the phones.".into(),
    }
}

pub fn battery_answer(phones: &[&Phone], name: Option<&str>, lang: Lang) -> String {
    let no = lang == Lang::Norwegian;
    if phones.is_empty() {
        return match (name, no) {
            (Some(name), false) => format!("I don't know {name}'s phone."),
            (Some(name), true) => format!("Jeg kjenner ikke telefonen til {name}."),
            (None, false) => "I don't see any phones with the Home Assistant app.".into(),
            (None, true) => "Jeg ser ingen telefoner med Home Assistant-appen.".into(),
        };
    }
    let named = name.is_some() || phones.len() > 1;
    let sentences: Vec<String> = phones
        .iter()
        .map(|phone| {
            let who = capitalize(&whose(phone, named, no));
            let level = match (phone.battery_percent(), no) {
                (Some(level), false) => format!("{who} is at {level} percent."),
                (Some(level), true) => format!("{who} har {level} prosent."),
                (None, false) => format!("{who} doesn't report its battery."),
                (None, true) => format!("{who} sier ikke noe om batteriet."),
            };
            // Kept apart from the number, so it is rendered once.
            let charging = matches!(phone.charging.as_str(), "Charging" | "charging");
            match (charging, no) {
                (true, false) => format!("{level} It's charging."),
                (true, true) => format!("{level} Den lader."),
                (false, _) => level,
            }
        })
        .collect();
    sentences.join(" ")
}

fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map_or_else(String::new, |c| c.to_uppercase().chain(chars).collect())
}

/// "Sunset is at 18:19." from the `sun.sun` state.
pub fn sun_answer(sun: &Value, event: SunEvent, lang: Lang) -> Result<String> {
    let key = match event {
        SunEvent::Sunrise => "next_rising",
        SunEvent::Sunset => "next_setting",
        SunEvent::Dark => "next_dusk",
    };
    let at: jiff::Timestamp = sun["attributes"][key].as_str().context("no sun times in Home Assistant")?.parse()?;
    let at = at.to_zoned(jiff::tz::TimeZone::system());
    let today = jiff::Zoned::now().date();
    let time = at.strftime("%H:%M").to_string();
    let tomorrow = at.date() != today;
    let no = lang == Lang::Norwegian;
    Ok(match (event, no, tomorrow) {
        (SunEvent::Sunset, false, false) => format!("Sunset is at {time}."),
        (SunEvent::Sunset, false, true) => format!("The sun has set. Tomorrow it sets at {time}."),
        (SunEvent::Sunrise, false, false) => format!("Sunrise is at {time}."),
        (SunEvent::Sunrise, false, true) => format!("Sunrise is at {time} tomorrow."),
        (SunEvent::Dark, false, false) => format!("It gets dark at about {time}."),
        (SunEvent::Dark, false, true) => format!("It's already dark. Tomorrow it gets dark at about {time}."),
        (SunEvent::Sunset, true, false) => format!("Solen går ned klokka {time}."),
        (SunEvent::Sunset, true, true) => format!("Solen har gått ned. I morgen går den ned klokka {time}."),
        (SunEvent::Sunrise, true, false) => format!("Solen står opp klokka {time}."),
        (SunEvent::Sunrise, true, true) => format!("Solen står opp klokka {time} i morgen."),
        (SunEvent::Dark, true, false) => format!("Det blir mørkt rundt {time}."),
        (SunEvent::Dark, true, true) => format!("Det er allerede mørkt. I morgen blir det mørkt rundt {time}."),
    })
}

/// Temperature and humidity of one room; several sensors in a room are averaged.
#[derive(Debug, Clone, PartialEq)]
pub struct RoomClimate {
    pub area: String,
    pub temperature: Option<f64>,
    pub humidity: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct ClimateSensor {
    area: String,
    kind: String,
    value: f64,
}

/// Sensors without an area say nothing about a room (a phone, a router's CPU), so they are left out.
const CLIMATE_TEMPLATE: &str = r#"[{% for s in states.sensor if s.attributes.device_class in ['temperature', 'humidity'] and area_name(s.entity_id) and s.state | is_number %}{"area": {{ area_name(s.entity_id) | tojson }}, "kind": {{ s.attributes.device_class | tojson }}, "value": {{ s.state | float }}}{% if not loop.last %},{% endif %}{% endfor %}]"#;

/// Areas that are not indoors, by their names in either language.
const OUTDOOR_AREAS: &[&str] = &[
    "outside",
    "outdoors",
    "garden",
    "yard",
    "balcony",
    "terrace",
    "patio",
    "garage",
    "ute",
    "hage",
    "hagen",
    "balkong",
    "balkongen",
    "terrasse",
    "terrassen",
    "garasje",
    "garasjen",
];

/// Every indoor room with a temperature or humidity sensor, in Home Assistant's order.
pub fn climate_readings(ha: &HomeAssistant) -> Result<Vec<RoomClimate>> {
    let sensors: Vec<ClimateSensor> =
        serde_json::from_str(&ha.template(CLIMATE_TEMPLATE)?).context("parsing the climate sensors")?;
    Ok(rooms_of(&sensors))
}

fn rooms_of(sensors: &[ClimateSensor]) -> Vec<RoomClimate> {
    let mut areas: Vec<&str> = Vec::new();
    for sensor in sensors {
        let outdoor = sensor.area.split_whitespace().any(|w| OUTDOOR_AREAS.contains(&w.to_lowercase().as_str()));
        if !outdoor && !areas.contains(&sensor.area.as_str()) {
            areas.push(&sensor.area);
        }
    }
    let mean = |area: &str, kind: &str| {
        let values: Vec<f64> = sensors.iter().filter(|s| s.area == area && s.kind == kind).map(|s| s.value).collect();
        (!values.is_empty()).then(|| values.iter().sum::<f64>() / values.len() as f64)
    };
    areas
        .into_iter()
        .map(|area| RoomClimate {
            area: area.to_owned(),
            temperature: mean(area, "temperature"),
            humidity: mean(area, "humidity"),
        })
        .collect()
}

/// Below this the air feels dry (skin, throat, static); above the other it feels damp.
const DRY_BELOW: f64 = 30.0;
const DAMP_ABOVE: f64 = 60.0;

/// The answer to an indoor climate question. `here` is the satellite's room, if known; "in here"
/// without a sensor there gives every room.
pub fn climate_answer(
    rooms: &[RoomClimate],
    place: &Indoors,
    reading: Reading,
    here: Option<&str>,
    lang: Lang,
) -> String {
    let no = lang == Lang::Norwegian;
    let wants = |r: &RoomClimate| match reading {
        Reading::Temperature => r.temperature.is_some(),
        Reading::Humidity => r.humidity.is_some(),
        Reading::Both => r.temperature.is_some() || r.humidity.is_some(),
    };
    let measured: Vec<&RoomClimate> = rooms.iter().filter(|r| wants(r)).collect();
    let sensor = match (reading, no) {
        (Reading::Humidity, false) => "humidity sensor",
        (Reading::Humidity, true) => "fuktighetsmåler",
        (_, false) => "temperature sensor",
        (_, true) => "temperaturmåler",
    };
    if measured.is_empty() {
        return if no {
            format!("Jeg finner ingen {sensor} i Home Assistant.")
        } else {
            format!("I don't see a {sensor} in any room in Home Assistant.")
        };
    }
    let room = match place {
        Indoors::Room(room) => match measured.iter().find(|r| crate::ha::is_room(&r.area, room)) {
            Some(found) => Some(*found),
            None => {
                let have: Vec<String> = measured.iter().map(|r| r.area.to_lowercase()).collect();
                let room = room.to_lowercase();
                return if no {
                    format!("Det er ingen {sensor} i {room}. Jeg har {}.", join(&have, lang))
                } else {
                    format!("There's no {sensor} in the {room}. I have the {}.", join(&have, lang))
                };
            }
        },
        Indoors::Here => here.and_then(|h| measured.iter().find(|r| crate::ha::is_room(&r.area, h)).copied()),
        Indoors::Everywhere => None,
    };
    let picked: Vec<&RoomClimate> = match room {
        Some(room) => vec![room],
        None => measured,
    };
    let mut parts = Vec::new();
    if let [one] = picked.as_slice() {
        parts.push(one_room(one, reading, lang));
    } else {
        let list = |value: fn(&RoomClimate) -> Option<f64>, unit: &str| {
            let items: Vec<String> = picked
                .iter()
                .filter_map(|r| value(r).map(|v| (r, v)))
                .enumerate()
                .map(|(i, (r, v))| {
                    let area = r.area.to_lowercase();
                    match (i, no) {
                        (0, false) => format!("{} {unit} in the {area}", v.round()),
                        (0, true) => format!("{} {unit} i {area}", v.round()),
                        (_, false) => format!("{} in the {area}", v.round()),
                        (_, true) => format!("{} i {area}", v.round()),
                    }
                })
                .collect();
            join(&items, lang)
        };
        if reading != Reading::Humidity {
            let degrees = if no { "grader" } else { "degrees" };
            let text = list(|r| r.temperature, degrees);
            parts.push(if no { format!("Det er {text}.") } else { format!("It's {text}.") });
        }
        if reading != Reading::Temperature {
            let percent = if no { "prosent" } else { "percent" };
            let text = list(|r| r.humidity, percent);
            if !text.is_empty() {
                parts.push(if no { format!("Luftfuktigheten er {text}.") } else { format!("Humidity is {text}.") });
            }
        }
    }
    if reading != Reading::Temperature {
        parts.extend(picked.iter().filter_map(|r| humidity_remark(r, picked.len() > 1, lang)));
    }
    parts.join(" ")
}

fn one_room(room: &RoomClimate, reading: Reading, lang: Lang) -> String {
    let no = lang == Lang::Norwegian;
    let area = room.area.to_lowercase();
    let t = room.temperature.map(f64::round);
    let h = room.humidity.map(f64::round);
    match (reading, t, h, no) {
        (Reading::Humidity, _, Some(h), false) => format!("The humidity in the {area} is {h} percent."),
        (Reading::Humidity, _, Some(h), true) => format!("Luftfuktigheten i {area} er {h} prosent."),
        (Reading::Both, Some(t), Some(h), false) => {
            format!("It's {t} degrees in the {area}, with {h} percent humidity.")
        }
        (Reading::Both, Some(t), Some(h), true) => {
            format!("Det er {t} grader i {area}, med {h} prosent luftfuktighet.")
        }
        (_, Some(t), _, false) => format!("It's {t} degrees in the {area}."),
        (_, Some(t), _, true) => format!("Det er {t} grader i {area}."),
        (_, None, Some(h), false) => format!("The humidity in the {area} is {h} percent."),
        (_, None, Some(h), true) => format!("Luftfuktigheten i {area} er {h} prosent."),
        (_, None, None, _) => String::new(),
    }
}

/// "The air is dry." Fixed wording on its own when one room is asked about, so it is pre-rendered.
fn humidity_remark(room: &RoomClimate, name_it: bool, lang: Lang) -> Option<String> {
    let no = lang == Lang::Norwegian;
    let h = room.humidity?;
    let dry = if h < DRY_BELOW {
        true
    } else if h > DAMP_ABOVE {
        false
    } else {
        return None;
    };
    let area = room.area.to_lowercase();
    Some(match (dry, name_it, no) {
        (true, false, false) => "The air is dry.".into(),
        (false, false, false) => "The air is damp.".into(),
        (true, false, true) => "Lufta er tørr.".into(),
        (false, false, true) => "Lufta er fuktig.".into(),
        (true, true, false) => format!("The air in the {area} is dry."),
        (false, true, false) => format!("The air in the {area} is damp."),
        (true, true, true) => format!("Lufta i {area} er tørr."),
        (false, true, true) => format!("Lufta i {area} er fuktig."),
    })
}

/// "the living room and kitchen", "stua og kjøkkenet".
pub fn join(items: &[String], lang: Lang) -> String {
    let and = if lang == Lang::Norwegian { "og" } else { "and" };
    match items {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} {and} {last}", rest.join(", ")),
    }
}

/// One name per room with lights on: the area, or the light's own name without one. A group with
/// no area ("House Lights") says nothing about where.
pub fn rooms_lit(lights: &[Light]) -> Vec<String> {
    let mut places: Vec<String> = Vec::new();
    for light in lights.iter().filter(|l| !(l.group && l.area.is_empty())) {
        let place = if light.area.is_empty() { light.name.to_lowercase() } else { light.area.to_lowercase() };
        if !places.contains(&place) {
            places.push(place);
        }
    }
    places
}

/// The alarm's state in a sentence; `None` without a panel.
pub fn alarm_sentence(state: &str, lang: Lang) -> Option<String> {
    let no = lang == Lang::Norwegian;
    let text = match (state, no) {
        ("", _) | ("unavailable" | "unknown", _) => return None,
        ("disarmed", false) => "The alarm is off.",
        ("disarmed", true) => "Alarmen er av.",
        ("arming", false) => "The alarm is arming.",
        ("arming", true) => "Alarmen slås på.",
        ("pending", false) => "The alarm is about to go off.",
        ("pending", true) => "Alarmen er i ferd med å gå.",
        ("triggered", false) => "The alarm has gone off!",
        ("triggered", true) => "Alarmen har gått!",
        (_, false) => "The alarm is armed.",
        (_, true) => "Alarmen er på.",
    };
    Some(text.into())
}

/// "Is everything okay at home?": warnings first, then the alarm, lights, TV and music, the
/// camera, low batteries and the last motion.
pub fn status_answer(alerts: &[Alert], lights: &[Light], snapshot: &Snapshot, lang: Lang) -> String {
    let no = lang == Lang::Norwegian;
    let mut parts: Vec<String> = alerts.iter().map(|a| alerts::sentence(a, lang)).collect();
    if parts.is_empty() {
        parts.push(if no { "Alt er i orden." } else { "All is well." }.into());
    }
    parts.extend(alarm_sentence(&snapshot.alarm, lang));
    let rooms = rooms_lit(lights);
    parts.push(match (rooms.is_empty(), no) {
        (true, false) => "All the lights are off.".into(),
        (true, true) => "Alt lyset er av.".into(),
        (false, false) => format!("Lights are on in the {}.", join(&rooms, lang)),
        (false, true) => format!("Lyset er på i {}.", join(&rooms, lang)),
    });
    if !snapshot.tvs.is_empty() {
        parts.push(if no { "TV-en er på." } else { "The TV is on." }.into());
    }
    let mut speakers: Vec<String> = Vec::new();
    for player in &snapshot.playing {
        let name = player.name.to_lowercase();
        if !speakers.contains(&name) {
            speakers.push(name);
        }
    }
    if !speakers.is_empty() {
        parts.push(if no {
            format!("Musikken spiller på {}.", join(&speakers, lang))
        } else {
            format!("Music is playing in the {}.", join(&speakers, lang))
        });
    }
    if !snapshot.privacy.is_empty() {
        parts.push(if no { "Kameraet er i personvernmodus." } else { "The camera is in privacy mode." }.into());
    }
    for battery in &snapshot.low_batteries {
        let name = battery.name.to_lowercase();
        let name = name.trim_end_matches(" battery");
        let level = battery.level.round();
        parts.push(if no {
            format!("Batteriet i {name} er på {level} prosent.")
        } else {
            format!("The {name} battery is at {level} percent.")
        });
    }
    if let Ok(at) = snapshot.motion.parse::<jiff::Timestamp>() {
        let at = at.to_zoned(jiff::tz::TimeZone::system());
        if at.date() == jiff::Zoned::now().date() {
            let time = at.strftime("%H:%M");
            parts.push(if no {
                format!("Kameraet så sist bevegelse klokka {time}.")
            } else {
                format!("The camera last saw motion at {time}.")
            });
        }
    }
    parts.join(" ")
}

/// What "everything off" switched off.
#[derive(Debug, Default)]
pub struct Switched {
    pub lights: Vec<Light>,
    pub music: bool,
    pub tvs: bool,
}

/// Switches off the lights (all but `keep`), pauses the music and turns off the TVs.
pub fn everything_off(ha: &mut HomeAssistant, keep: impl Fn(&Light) -> bool) -> Result<Switched> {
    // Network gear reports its status LED as a light; nobody means that. A group with no area
    // ("House Lights") would also switch the lights kept on.
    let lights: Vec<Light> = ha
        .lights_on()?
        .into_iter()
        .filter(|l| !l.name.ends_with(" LED") && !(l.group && l.area.is_empty()) && !keep(l))
        .collect();
    if !lights.is_empty() {
        let ids: Vec<&str> = lights.iter().map(|l| l.id.as_str()).collect();
        ha.send("light", "turn_off", json!({ "entity_id": ids }))?;
    }
    let snapshot = snapshot(ha)?;
    if !snapshot.playing.is_empty() {
        let ids: Vec<&str> = snapshot.playing.iter().map(|p| p.id.as_str()).collect();
        ha.send("media_player", "media_pause", json!({ "entity_id": ids }))?;
    }
    let tvs = crate::tv::on_tvs(ha)?;
    if !tvs.is_empty() {
        ha.send("media_player", "turn_off", json!({ "entity_id": tvs }))?;
    }
    Ok(Switched { lights, music: !snapshot.playing.is_empty(), tvs: !tvs.is_empty() })
}

/// The scene `scene.create` keeps the lights in while the house is locked down.
const LOCKDOWN_SCENE: &str = "lockdown_restore";

/// Lights in a room (a neighbour's light that shows up in Home Assistant has none), not room
/// groups, and not status LEDs (network gear, the Voice PE's ring). `color`: it can go red.
const LOCKDOWN_LIGHTS_TEMPLATE: &str = r#"[{% for s in states.light if area_name(s.entity_id) and s.attributes.entity_id is not defined and 'led' not in (s.name | lower).split(' ') %}{"id": {{ s.entity_id | tojson }}, "color": {{ ((s.attributes.supported_color_modes or []) | select('in', ['hs', 'xy', 'rgb', 'rgbw', 'rgbww']) | list | count > 0) | tojson }}}{% if not loop.last %},{% endif %}{% endfor %}]"#;

#[derive(Deserialize)]
struct LockdownLight {
    id: String,
    color: bool,
}

/// Every light that can go red goes full red, the rest go off; a snapshot of them all is kept
/// first, for standing down. A second lockdown keeps the first snapshot. The red lights.
pub fn lockdown(ha: &HomeAssistant) -> Result<Vec<String>> {
    let lights: Vec<LockdownLight> =
        serde_json::from_str(&ha.template(LOCKDOWN_LIGHTS_TEMPLATE)?).context("parsing the lockdown lights")?;
    let ids = |color: bool| lights.iter().filter(|l| l.color == color).map(|l| l.id.as_str()).collect::<Vec<_>>();
    let (red, off) = (ids(true), ids(false));
    if !locked_down(ha) {
        let all: Vec<&str> = lights.iter().map(|l| l.id.as_str()).collect();
        ha.send("scene", "create", json!({ "scene_id": LOCKDOWN_SCENE, "snapshot_entities": all }))?;
    }
    if !red.is_empty() {
        ha.send("light", "turn_on", json!({ "entity_id": red, "rgb_color": [255, 0, 0], "brightness_pct": 100 }))?;
    }
    if !off.is_empty() {
        ha.send("light", "turn_off", json!({ "entity_id": off }))?;
    }
    Ok(red.into_iter().map(str::to_owned).collect())
}

/// Hue rooms and zones in Home Assistant, as `[{"id": ..., "members": [...]}]`.
const HUE_GROUPS_TEMPLATE: &str = r#"[{% for s in states.light if s.attributes.is_hue_group %}{"id": {{ s.entity_id | tojson }}, "members": {{ (s.attributes.entity_id or []) | list | tojson }}}{% if not loop.last %},{% endif %}{% endfor %}]"#;

#[derive(Deserialize)]
struct HueGroup {
    id: String,
    members: Vec<String>,
}

/// What the lockdown's alert goes to: each Hue room whose lights are all red, as one command the
/// bridge sends to them together, and the red lights in no such room one by one.
pub fn alarm_targets(ha: &HomeAssistant, red: &[String]) -> Result<Vec<String>> {
    let groups: Vec<HueGroup> =
        serde_json::from_str(&ha.template(HUE_GROUPS_TEMPLATE)?).context("parsing the Hue rooms")?;
    Ok(cover(&groups, red))
}

fn cover(groups: &[HueGroup], red: &[String]) -> Vec<String> {
    let mut left: Vec<&String> = red.iter().collect();
    let mut targets = Vec::new();
    // Rooms before zones: a zone over several rooms would leave a room's own lights to it.
    let mut groups: Vec<&HueGroup> = groups.iter().filter(|g| !g.members.is_empty()).collect();
    groups.sort_by_key(|g| g.members.len());
    for group in groups {
        if group.members.iter().all(|m| left.contains(&m)) {
            left.retain(|l| !group.members.contains(l));
            targets.push(group.id.clone());
        }
    }
    targets.extend(left.into_iter().cloned());
    targets
}

/// The Hue bridge's own alert on the lockdown's lights: about 15 seconds of fast pulsing, in step
/// across a room. Driving the blink from here instead (twice a second) queued up at the bridge.
pub fn alarm_pulse(ha: &HomeAssistant, targets: &[String]) -> Result<()> {
    ha.service("light", "turn_on", json!({ "entity_id": targets, "flash": "long" }))
}

/// The lights back as they were before the lockdown; false when there was none.
pub fn stand_down(ha: &HomeAssistant) -> Result<bool> {
    if !locked_down(ha) {
        return Ok(false);
    }
    let scene = format!("scene.{LOCKDOWN_SCENE}");
    ha.send("scene", "turn_on", json!({ "entity_id": scene }))?;
    ha.send("scene", "delete", json!({ "entity_id": scene }))?;
    Ok(true)
}

/// The scene the lights are kept in while party mode streams to them.
const PARTY_SCENE: &str = "party_restore";

/// Keeps the lights as they are, for after party mode; a second start keeps the first snapshot.
pub fn party_snapshot(ha: &HomeAssistant) -> Result<()> {
    if ha.state(&format!("scene.{PARTY_SCENE}")).is_ok() {
        return Ok(());
    }
    let lights: Vec<LockdownLight> =
        serde_json::from_str(&ha.template(LOCKDOWN_LIGHTS_TEMPLATE)?).context("parsing the lights")?;
    let all: Vec<&str> = lights.iter().map(|l| l.id.as_str()).collect();
    ha.service("scene", "create", json!({ "scene_id": PARTY_SCENE, "snapshot_entities": all }))?;
    Ok(())
}

/// The lights as they were before party mode; false when there was no snapshot.
pub fn party_restore(ha: &HomeAssistant) -> Result<bool> {
    let scene = format!("scene.{PARTY_SCENE}");
    if ha.state(&scene).is_err() {
        return Ok(false);
    }
    ha.service("scene", "turn_on", json!({ "entity_id": scene }))?;
    ha.service("scene", "delete", json!({ "entity_id": scene }))?;
    Ok(true)
}

/// A lockdown's snapshot exists. It outlives an engine restart, not a Home Assistant one.
fn locked_down(ha: &HomeAssistant) -> bool {
    ha.state(&format!("scene.{LOCKDOWN_SCENE}")).is_ok()
}

/// "Okay, the lights are off. The music is paused." for a routine; `except` names the room left lit.
pub fn switched_answer(switched: &Switched, except: Option<&str>, lang: Lang) -> String {
    let no = lang == Lang::Norwegian;
    let mut parts = Vec::new();
    if !switched.lights.is_empty() {
        parts.push(match (except, no) {
            (Some(room), false) => format!("Okay, the lights are off, except in the {room}."),
            (Some(room), true) => format!("Ok, lyset er av, bortsett fra på {room}."),
            (None, false) => "Okay, the lights are off.".into(),
            (None, true) => "Ok, lyset er av.".into(),
        });
    }
    if switched.music {
        parts.push(if no { "Musikken er satt på pause." } else { "The music is paused." }.into());
    }
    if switched.tvs {
        parts.push(if no { "TV-en er av." } else { "The TV is off." }.into());
    }
    if parts.is_empty() {
        parts.push(if no { "Ok, alt er allerede av." } else { "Okay, everything is already off." }.into());
    }
    parts.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routine_answers_are_rendered_ahead() {
        let light = Light { id: "light.a".into(), name: "A".into(), area: "Kitchen".into(), group: false };
        for lights in [vec![], vec![light]] {
            for (music, tvs) in [(false, false), (true, false), (false, true), (true, true)] {
                let switched = Switched { lights: lights.clone(), music, tvs };
                for except in [None, Some("bedroom")] {
                    let answer = switched_answer(&switched, except, Lang::English);
                    for sentence in answer.split_inclusive('.').map(str::trim) {
                        assert!(ROUTINE_ANSWERS.contains(&sentence), "{sentence}");
                    }
                }
            }
        }
    }

    #[test]
    fn alarm_covers_whole_red_rooms() {
        let group = |id: &str, members: &[&str]| HueGroup {
            id: id.into(),
            members: members.iter().map(|m| (*m).to_owned()).collect(),
        };
        let groups = [
            group("light.living_room", &["light.round", "light.fresh"]),
            group("light.kitchen", &["light.plug"]),
            group("light.bedroom", &["light.bed", "light.window"]),
        ];
        let red: Vec<String> = ["light.round", "light.fresh", "light.bed", "light.lamp"].map(String::from).to_vec();
        // The bedroom's window light is not red, so its bed light goes alone.
        assert_eq!(cover(&groups, &red), ["light.living_room", "light.bed", "light.lamp"]);
    }
    use crate::dialog::normalize;

    fn words(text: &str) -> Vec<String> {
        text.split_whitespace().map(normalize).filter(|w| !w.is_empty()).collect()
    }

    fn house(text: &str) -> Option<HouseCommand> {
        parse(&words(text))
    }

    #[test]
    fn status_questions() {
        for text in [
            "Is everything okay at home?",
            "is everything alright",
            "status report",
            "What's the house status?",
            "how's the house?",
            "Is there anything I should know?",
            "er alt i orden?",
        ] {
            assert_eq!(house(text), Some(HouseCommand::Status), "{text}");
        }
        assert_eq!(house("is everything okay with you and your family over there today"), None);
    }

    #[test]
    fn climate_questions() {
        use Indoors::*;
        use Reading::*;
        let climate = |place, reading| Some(HouseCommand::Climate(place, reading));
        let room = |r: &str| Room(r.to_owned());
        assert_eq!(house("how warm is it in the bedroom"), climate(room("bedroom"), Temperature));
        assert_eq!(house("what's the temperature in the living room?"), climate(room("living room"), Temperature));
        assert_eq!(house("is it humid in the bathroom"), climate(room("bathroom"), Humidity));
        assert_eq!(house("what's the temperature inside"), climate(Everywhere, Temperature));
        assert_eq!(house("is it cold at home"), climate(Everywhere, Temperature));
        assert_eq!(house("it's freezing in here"), climate(Here, Temperature));
        assert_eq!(house("is it dry in here"), climate(Here, Humidity));
        assert_eq!(house("what's the temperature and humidity in the kitchen"), climate(room("kitchen"), Both));
        assert_eq!(house("hvor varmt er det i stua"), climate(room("living room"), Temperature));
        assert_eq!(house("hva er innetemperaturen"), climate(Everywhere, Temperature));
        assert_eq!(house("hva er luftfuktigheten på badet"), climate(room("bathroom"), Humidity));
        // The weather, the lights and a heating command are not it.
        assert_eq!(house("how warm is it"), None);
        assert_eq!(house("what's the temperature in Trondheim"), None);
        assert_eq!(house("is it cold outside"), None);
        assert_eq!(house("make it warmer in here"), None);
        assert_eq!(house("make the bedroom lights warmer"), None);
        assert_eq!(house("hvordan blir været i morgen"), None);
    }

    fn rooms() -> Vec<RoomClimate> {
        let room =
            |area: &str, t: f64, h: f64| RoomClimate { area: area.into(), temperature: Some(t), humidity: Some(h) };
        vec![room("Living Room", 23.96, 28.4), room("Bedroom", 19.05, 43.2), room("Bathroom", 22.0, 65.0)]
    }

    #[test]
    fn climate_answers() {
        let answer = |place: Indoors, reading, here| climate_answer(&rooms(), &place, reading, here, Lang::English);
        let bedroom = Indoors::Room("bedroom".into());
        assert_eq!(answer(bedroom.clone(), Reading::Temperature, None), "It's 19 degrees in the bedroom.");
        assert_eq!(answer(bedroom, Reading::Both, None), "It's 19 degrees in the bedroom, with 43 percent humidity.");
        assert_eq!(
            answer(Indoors::Here, Reading::Humidity, Some("Living Room")),
            "The humidity in the living room is 28 percent. The air is dry."
        );
        assert_eq!(
            answer(Indoors::Everywhere, Reading::Temperature, None),
            "It's 24 degrees in the living room, 19 in the bedroom and 22 in the bathroom."
        );
        assert_eq!(
            answer(Indoors::Here, Reading::Humidity, None),
            "Humidity is 28 percent in the living room, 43 in the bedroom and 65 in the bathroom. \
             The air in the living room is dry. The air in the bathroom is damp."
        );
        assert_eq!(
            answer(Indoors::Room("kitchen".into()), Reading::Temperature, None),
            "There's no temperature sensor in the kitchen. I have the living room, bedroom and bathroom."
        );
        assert_eq!(
            climate_answer(&[], &Indoors::Everywhere, Reading::Temperature, None, Lang::English),
            "I don't see a temperature sensor in any room in Home Assistant."
        );
        assert_eq!(
            climate_answer(&rooms(), &Indoors::Room("stua".into()), Reading::Temperature, None, Lang::Norwegian),
            "Det er 24 grader i living room."
        );
    }

    #[test]
    fn climate_sensors_group_by_room() {
        let sensor = |area: &str, kind: &str, value| ClimateSensor { area: area.into(), kind: kind.into(), value };
        let rooms = rooms_of(&[
            sensor("Bedroom", "temperature", 19.0),
            sensor("Bedroom", "temperature", 20.0),
            sensor("Balcony", "temperature", 8.0),
            sensor("Bedroom", "humidity", 40.0),
            sensor("Bathroom", "humidity", 50.0),
        ]);
        assert_eq!(
            rooms,
            [
                RoomClimate { area: "Bedroom".into(), temperature: Some(19.5), humidity: Some(40.0) },
                RoomClimate { area: "Bathroom".into(), temperature: None, humidity: Some(50.0) },
            ]
        );
    }

    #[test]
    fn sun_questions() {
        use SunEvent::*;
        assert_eq!(house("When is sunset?"), Some(HouseCommand::Sun(Sunset)));
        assert_eq!(house("what time does the sun go down"), Some(HouseCommand::Sun(Sunset)));
        assert_eq!(house("when's sunrise tomorrow"), Some(HouseCommand::Sun(Sunrise)));
        assert_eq!(house("when does the sun come up"), Some(HouseCommand::Sun(Sunrise)));
        assert_eq!(house("when does it get dark"), Some(HouseCommand::Sun(Dark)));
        assert_eq!(house("når går solen ned"), Some(HouseCommand::Sun(Sunset)));
        assert_eq!(house("når blir det mørkt"), Some(HouseCommand::Sun(Dark)));
        assert_eq!(house("it's too dark in the kitchen"), None);
    }

    #[test]
    fn phone_requests() {
        use HouseCommand::*;
        assert_eq!(house("Where's my phone?"), Some(FindPhone(None)));
        assert_eq!(house("I can't find my phone"), Some(FindPhone(None)));
        assert_eq!(house("ring Kristine's phone"), Some(FindPhone(Some("kristine".into()))));
        assert_eq!(house("hvor er telefonen min"), Some(FindPhone(None)));
        assert_eq!(house("finn telefonen til Kristine"), Some(FindPhone(Some("kristine".into()))));
        assert_eq!(house("how much battery does my phone have"), Some(PhoneBattery(None)));
        assert_eq!(house("is my phone charged"), Some(PhoneBattery(None)));
        assert_eq!(house("my phone is new"), None);
    }

    #[test]
    fn guest_wifi_camera_and_alarm() {
        use HouseCommand::*;
        use Switch::{Off, On};
        assert_eq!(house("turn on the guest wifi"), Some(GuestWifi(On)));
        assert_eq!(house("Turn off the guest Wi-Fi."), Some(GuestWifi(Off)));
        assert_eq!(house("is the guest network on"), Some(GuestWifi(Switch::Status)));
        assert_eq!(house("skru på gjestenettet"), Some(GuestWifi(On)));
        assert_eq!(house("turn off the camera"), Some(Camera(Off)));
        assert_eq!(house("turn on privacy mode"), Some(Camera(Off)));
        assert_eq!(house("stop recording"), Some(Camera(Off)));
        assert_eq!(house("turn the camera back on"), Some(Camera(On)));
        assert_eq!(house("is the camera recording"), Some(Camera(Switch::Status)));
        assert_eq!(house("arm the alarm"), Some(Alarm(On)));
        assert_eq!(house("is the alarm armed"), Some(Alarm(Switch::Status)));
        assert_eq!(house("disarm the alarm"), Some(Alarm(Off)));
        assert_eq!(house("turn off the alarm"), None);
        assert_eq!(house("set an alarm for seven"), None);
    }

    #[test]
    fn routines() {
        let routine = |t: &str| routine(&words(t));
        assert_eq!(routine("I'm leaving"), Some(Routine::Leaving));
        assert_eq!(routine("okay, I'm heading out now"), Some(Routine::Leaving));
        assert_eq!(routine("I'm home"), Some(Routine::Home));
        assert_eq!(routine("honey, I'm home!"), Some(Routine::Home));
        assert_eq!(routine("I'm off to bed"), Some(Routine::Bedtime));
        assert_eq!(routine("bedtime"), Some(Routine::Bedtime));
        assert_eq!(routine("jeg legger meg"), Some(Routine::Bedtime));
        assert_eq!(routine("I'm leaving my old job at the end of the month for something new"), None);
        assert_eq!(routine("is Ingrid home"), None);
        assert_eq!(routine("initiate lockdown"), Some(Routine::Lockdown));
        assert_eq!(routine("red alert"), Some(Routine::Lockdown));
        assert_eq!(routine("stand down"), Some(Routine::StandDown));
        assert_eq!(routine("end the lockdown"), Some(Routine::StandDown));
    }

    fn phone(id: &str, owner: &str, location: &str) -> Phone {
        Phone {
            id: id.into(),
            name: format!("{id} device"),
            location: location.into(),
            battery: json!("80"),
            charging: "Not Charging".into(),
            owner: owner.into(),
        }
    }

    #[test]
    fn picks_and_answers_for_phones() {
        let phones =
            vec![phone("morten_iphone", "Morten Nordbye", "home"), phone("kristine_iphone", "Kristine", "not_home")];
        assert_eq!(pick_phones(&phones, Some("kristine"))[0].id, "kristine_iphone");
        assert_eq!(pick_phones(&phones, None).len(), 1);
        assert!(pick_phones(&phones, Some("ingrid")).is_empty());
        let one = pick_phones(&phones[..1], None);
        assert_eq!(rang_answer(&one, None, Lang::English), "Okay, I'm ringing your phone.");
        let away = pick_phones(&phones, Some("kristine"));
        assert_eq!(
            rang_answer(&away, Some("kristine"), Lang::English),
            "Okay, I'm ringing Kristine's phone. It isn't at home, though."
        );
        assert_eq!(battery_answer(&one, None, Lang::English), "Your phone is at 80 percent.");
    }

    #[test]
    fn status_reads_in_sentences() {
        let lights = vec![Light { id: "light.a".into(), name: "A".into(), area: "Kitchen".into(), group: false }];
        let snapshot = Snapshot {
            alarm: "disarmed".into(),
            tvs: vec!["TV".into()],
            playing: vec![
                Named { id: "media_player.living_room".into(), name: "Living Room".into() },
                Named { id: "media_player.living_room_2".into(), name: "Living Room".into() },
            ],
            ..Snapshot::default()
        };
        assert_eq!(
            status_answer(&[], &lights, &snapshot, Lang::English),
            "All is well. The alarm is off. Lights are on in the kitchen. The TV is on. Music is playing in the living room."
        );
        let house = Light { id: "light.house".into(), name: "House Lights".into(), area: String::new(), group: true };
        assert_eq!(rooms_lit(&[house, lights[0].clone()]), ["kitchen"]);
    }
}
