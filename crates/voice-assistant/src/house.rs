//! The house beyond the lights: a status check, routines for leaving, coming home and going to bed,
//! sunrise and sunset, finding a phone, the guest Wi-Fi, the camera's privacy mode and the alarm.
//! The rules, the Home Assistant calls and the answers are here; the engine keeps which lights a
//! routine switched, for "undo", and the music coming home starts.

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use tracing::info;

use crate::alerts::{self, Alert};
use crate::ha::{self, HomeAssistant, Light, Target};
use crate::lang::Lang;
use crate::names::Names;
use crate::say::{capitalize, join};
use crate::sensors::{self, SensorQuery};
use crate::skill::{Done, LightChange, Lights};

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
    /// "Is it dark outside?" (`true`), "is it light yet?" (`false`): now, and the next change.
    IsDark(bool),
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
    /// Battery levels and leak sensors: "which batteries are low", "is there a leak".
    Sensors(crate::sensors::SensorQuery),
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
    /// "What do you think about open source?": party mode and a song. Just for fun.
    OpenSource,
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
    // After the phone: "how much battery does my phone have" is the phone's.
    if let Some(query) = crate::sensors::parse(words) {
        return Some(HouseCommand::Sensors(query));
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

pub(crate) const TEMPERATURE_WORDS: &[&str] = &[
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
pub(crate) const HUMIDITY_WORDS: &[&str] =
    &["humid", "humidity", "dry", "damp", "muggy", "fukt", "fuktig", "fuktighet", "fuktigheten", "tørr", "tørt"];
/// Humidity by name, which is asked about indoors even without a place.
const HUMIDITY_NOUNS: &[&str] = &["humidity", "fuktighet", "fuktigheten"];
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
    } else if !temperature && (has_any(words, HUMIDITY_NOUNS) || words.iter().any(|w| w.starts_with("luftfukt"))) {
        // "What's the humidity": the weather never says it, so it is this room's.
        Indoors::Here
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
    let light = ["get light", "gets light", "be light", "get bright", "blir det lyst", "blir lyst"]
        .iter()
        .any(|p| contains_words(&text, p));
    if when && light {
        return Some(SunEvent::Sunrise);
    }
    // "Is it dark outside", "is it light yet", "er det mørkt ute"; not "is it too dark in here".
    let rest =
        ["outside", "out", "yet", "already", "now", "still", "ute", "nå", "enda", "ennå", "allerede", "fortsatt"];
    let now = |start: &str| {
        text.strip_prefix(start).is_some_and(|tail| {
            tail.is_empty() || tail.starts_with(' ') && tail.split_whitespace().all(|w| rest.contains(&w))
        })
    };
    if now("is it dark") || now("er det mørkt") {
        return Some(SunEvent::IsDark(true));
    }
    (now("is it light") || now("er det lyst")).then_some(SunEvent::IsDark(false))
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
    // "Is the alarm on?", "is the alarm set?"; "is my alarm set?" is the wake-up alarm (timer.rs).
    let status = has_any(words, &["alarm"])
        && asks(words)
        && has_any(words, &["on", "set", "active", "off"])
        && !has_any(words, &["my", "wake", "time", "when", "tomorrow"]);
    if !(arm || system || status) {
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

const OPEN_SOURCE: &[&str] = &["open source", "opensource", "åpen kildekode"];
/// Words that ask for an opinion: "what do you think of", "do you like", "hva synes du om".
const OPINION: &[&str] =
    &["think", "feel", "like", "love", "opinion", "thoughts", "take", "view", "synes", "liker", "elsker", "mener"];

/// The answer to "what do you think about open source?", rendered ahead of time.
pub const OPEN_SOURCE_ANSWER: &str = "I fucking love open source.";

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

/// Words that open a sentence without changing it.
const OPENERS: &[&str] = &["oh", "well", "so", "okay", "ok", "yeah", "right", "alright", "hey"];

pub fn routine(words: &[String]) -> Option<Routine> {
    // An opening the speaker adds: "oh, I'm home", "well, good night".
    let start = words.iter().position(|w| !OPENERS.contains(&w.as_str())).unwrap_or(words.len());
    let words = &words[start..];
    if words.len() > 9 {
        return None;
    }
    let text = phrase(words);
    let starts = |list: &[&str]| list.iter().any(|p| text == *p || text.starts_with(&format!("{p} ")));
    if starts(STAND_DOWN) {
        return Some(Routine::StandDown);
    }
    if OPEN_SOURCE.iter().any(|p| contains_words(&text, p)) && has_any(words, OPINION) {
        return Some(Routine::OpenSource);
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
    // Said alone, the listener's "look down" (16 Nov) is lockdown; in a sentence it is ordinary
    // words. A bare "leaving" is `misheard_leaving`: it needs the name.
    if text == "look down" {
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

/// The listener's "I'm leaving" heard without the "I'm": "Leaving." (19 Nov), "You're leaving."
/// (21 Nov). They are also what a TV or a person in the room says, and the routine turns off the
/// house, so the engine takes them only when the request was said to the assistant.
pub fn misheard_leaving(words: &[String]) -> bool {
    let start = words.iter().position(|w| !OPENERS.contains(&w.as_str())).unwrap_or(words.len());
    matches!(
        words[start..].join(" ").as_str(),
        "leaving" | "you're leaving" | "you are leaving" | "you're leaving now" | "your leaving"
    )
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
    /// One battery per device, by the battery skill's rules (`sensors::from_states`).
    #[serde(default)]
    pub batteries: Vec<sensors::Battery>,
    /// The last motion a camera saw, an ISO timestamp.
    #[serde(default)]
    pub motion: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Named {
    pub id: String,
    pub name: String,
}

const SNAPSHOT_TEMPLATE: &str = concat!(
    sensors::battery_states!(),
    r#"{% set tvs = states.media_player | rejectattr('state', 'in', ['off', 'standby', 'unavailable', 'unknown']) | rejectattr('attributes.mass_player_type', 'defined') | list %}
{"alarm": {{ (states.alarm_control_panel | map(attribute='state') | first | default('')) | tojson }},
"tvs": [{% for s in tvs if s.attributes.device_class == 'tv' or 'tv' in (s.name | lower) %}{{ s.name | tojson }}{% if not loop.last %},{% endif %}{% endfor %}],
"playing": [{% for s in states.media_player | selectattr('state', 'eq', 'playing') | selectattr('attributes.mass_player_type', 'defined') %}{"id": {{ s.entity_id | tojson }}, "name": {{ s.name | tojson }}}{% if not loop.last %},{% endif %}{% endfor %}],
"privacy": [{% for s in states.switch | selectattr('state', 'eq', 'on') if 'privacy' in s.entity_id %}{{ s.name | tojson }}{% if not loop.last %},{% endif %}{% endfor %}],
"batteries": {{ battery_states.out | tojson }},
"motion": {{ (states.event | selectattr('entity_id', 'search', 'motion') | map(attribute='state') | reject('in', ['unknown', 'unavailable']) | sort | last | default('')) | tojson }}}"#
);

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
    ha.template_json(&entities_template(domain, word), &format!("parsing {domain} entities"))
}

pub fn snapshot(ha: &HomeAssistant) -> Result<Snapshot> {
    parse_snapshot(&ha.template(SNAPSHOT_TEMPLATE)?)
}

fn parse_snapshot(json: &str) -> Result<Snapshot> {
    let mut snapshot: Snapshot = serde_json::from_str(json).context("parsing the house snapshot")?;
    snapshot.batteries = sensors::from_states(std::mem::take(&mut snapshot.batteries));
    Ok(snapshot)
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
    ha.template_json(&template, "parsing the phones")
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

/// The sun's elevation in degrees below which it is dark: the end of civil twilight, as dusk.
const DARK_BELOW: f64 = -6.0;

const NORWEGIAN_MONTHS: [&str; 12] = [
    "januar",
    "februar",
    "mars",
    "april",
    "mai",
    "juni",
    "juli",
    "august",
    "september",
    "oktober",
    "november",
    "desember",
];

/// When it next gets light (`dark` now) or dark, at the -6 degree line: dawn or dusk. Far north
/// the next one can be weeks away, or Home Assistant has none: then it says so, with the date.
fn next_change(sun: &Value, dark: bool, lang: Lang) -> Result<String> {
    let no = lang == Lang::Norwegian;
    let (key, fallback) = if dark { ("next_dawn", "next_rising") } else { ("next_dusk", "next_setting") };
    let Some(at) = sun["attributes"][key].as_str().or_else(|| sun["attributes"][fallback].as_str()) else {
        return Ok(match (dark, no) {
            (true, false) => "It doesn't get light for now.",
            (false, false) => "It doesn't get dark for now.",
            (true, true) => "Det blir ikke lyst med det første.",
            (false, true) => "Det blir ikke mørkt med det første.",
        }
        .into());
    };
    let at = at.parse::<jiff::Timestamp>()?.to_zoned(jiff::tz::TimeZone::system());
    let time = at.strftime("%H:%M").to_string();
    if at.timestamp().duration_since(jiff::Timestamp::now()) > jiff::SignedDuration::from_hours(24) {
        let month = at.month() as usize - 1;
        return Ok(match (dark, no) {
            (true, false) => format!("It doesn't get light for now. It next gets light on {}.", at.strftime("%B %-d")),
            (false, false) => format!("It doesn't get dark for now. It next gets dark on {}.", at.strftime("%B %-d")),
            (true, true) => {
                format!("Det blir ikke lyst med det første. Neste gang er {}. {}.", at.day(), NORWEGIAN_MONTHS[month])
            }
            (false, true) => {
                format!("Det blir ikke mørkt med det første. Neste gang er {}. {}.", at.day(), NORWEGIAN_MONTHS[month])
            }
        });
    }
    Ok(match (dark, no) {
        (true, false) => format!("It gets light at about {time}."),
        (false, false) => format!("It gets dark at about {time}."),
        (true, true) => format!("Det blir lyst rundt {time}."),
        (false, true) => format!("Det blir mørkt rundt {time}."),
    })
}

/// "Sunset is at 18:19." from the `sun.sun` state.
pub fn sun_answer(sun: &Value, event: SunEvent, lang: Lang) -> Result<String> {
    let no = lang == Lang::Norwegian;
    let elevation = sun["attributes"]["elevation"].as_f64();
    let key = match event {
        SunEvent::Sunrise => "next_rising",
        SunEvent::Sunset => "next_setting",
        SunEvent::IsDark(asked_dark) => {
            let dark = elevation.context("no sun elevation in Home Assistant")? < DARK_BELOW;
            // Fixed wording on its own, the time in the next sentence.
            let now = match (dark, dark == asked_dark, no) {
                (true, true, false) => "Yes, it's dark.",
                (true, false, false) => "No, it's still dark.",
                (false, true, false) => "Yes, it's light.",
                (false, false, false) => "No, it's still light.",
                (true, true, true) => "Ja, det er mørkt.",
                (true, false, true) => "Nei, det er fortsatt mørkt.",
                (false, true, true) => "Ja, det er lyst.",
                (false, false, true) => "Nei, det er fortsatt lyst.",
            };
            return Ok(format!("{now} {}", next_change(sun, dark, lang)?));
        }
        // "When does it get dark" while it is light: the coming dusk, even after midnight in
        // summer, never "already dark".
        SunEvent::Dark if elevation.is_some_and(|e| e >= DARK_BELOW) => return next_change(sun, false, lang),
        SunEvent::Dark => "next_dusk",
    };
    let at: jiff::Timestamp = sun["attributes"][key].as_str().context("no sun times in Home Assistant")?.parse()?;
    let at = at.to_zoned(jiff::tz::TimeZone::system());
    let today = jiff::Zoned::now().date();
    let time = at.strftime("%H:%M").to_string();
    let tomorrow = at.date() != today;
    Ok(match (event, no, tomorrow) {
        (SunEvent::Sunset, false, false) => format!("Sunset is at {time}."),
        (SunEvent::Sunset, false, true) => format!("The sun has set. Tomorrow it sets at {time}."),
        (SunEvent::Sunrise, false, false) => format!("Sunrise is at {time}."),
        (SunEvent::Sunrise, false, true) => format!("Sunrise is at {time} tomorrow."),
        (SunEvent::Dark | SunEvent::IsDark(_), false, false) => format!("It gets dark at about {time}."),
        (SunEvent::Dark | SunEvent::IsDark(_), false, true) => {
            format!("It's already dark. Tomorrow it gets dark at about {time}.")
        }
        (SunEvent::Sunset, true, false) => format!("Solen går ned klokka {time}."),
        (SunEvent::Sunset, true, true) => format!("Solen har gått ned. I morgen går den ned klokka {time}."),
        (SunEvent::Sunrise, true, false) => format!("Solen står opp klokka {time}."),
        (SunEvent::Sunrise, true, true) => format!("Solen står opp klokka {time} i morgen."),
        (SunEvent::Dark | SunEvent::IsDark(_), true, false) => format!("Det blir mørkt rundt {time}."),
        (SunEvent::Dark | SunEvent::IsDark(_), true, true) => {
            format!("Det er allerede mørkt. I morgen blir det mørkt rundt {time}.")
        }
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
    let sensors: Vec<ClimateSensor> = ha.template_json(CLIMATE_TEMPLATE, "parsing the climate sensors")?;
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
        if reading != Reading::Humidity && !no {
            // A sentence per room: each one is rendered ahead, a list of rooms never is.
            parts.extend(picked.iter().filter_map(|r| {
                let t = r.temperature?.round();
                Some(format!("It's {t} degrees in the {}.", r.area.to_lowercase()))
            }));
        } else if reading != Reading::Humidity {
            let degrees = "grader";
            let text = list(|r| r.temperature, degrees);
            parts.push(format!("Det er {text}."));
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
/// camera, the batteries the battery skill would name and the last motion.
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
    parts.extend(sensors::low_sentences(&snapshot.batteries, lang));
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
    let lights = to_switch_off(ha.lights_on()?, keep);
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

/// The lights on now that [`everything_off`] switches off.
pub fn to_switch_off(on: Vec<Light>, keep: impl Fn(&Light) -> bool) -> Vec<Light> {
    // A group with no area ("House Lights") would also switch the lights kept on.
    on.into_iter().filter(|l| !l.status_led() && !(l.group && l.area.is_empty()) && !keep(l)).collect()
}

/// The lights a snapshot scene puts back; `None` when there is no such snapshot.
pub fn snapshot_lights(ha: &HomeAssistant, scene: &str) -> Result<Option<Vec<String>>> {
    let Ok(state) = ha.state(&format!("scene.{scene}")) else { return Ok(None) };
    let ids = state["attributes"]["entity_id"].as_array().into_iter().flatten();
    Ok(Some(ids.filter_map(|id| id.as_str().map(str::to_owned)).collect()))
}

/// The scene the lights are kept in while party mode streams to them.
pub const PARTY_SCENE: &str = "party_restore";

/// Keeps the lights as they are, for after party mode. `keep`: a snapshot already there is still
/// the one to go back to (a second start, a lockdown since); otherwise one left over from a
/// restore whose delete failed is replaced.
pub fn party_snapshot(ha: &HomeAssistant, keep: bool) -> Result<()> {
    if keep && ha.state(&format!("scene.{PARTY_SCENE}")).is_ok() {
        return Ok(());
    }
    let (colour, plain) = crate::lockdown::lockdown_lights(ha)?;
    let all: Vec<&String> = colour.iter().chain(&plain).collect();
    ha.service("scene", "create", json!({ "scene_id": PARTY_SCENE, "snapshot_entities": all }))?;
    Ok(())
}

/// The lights as they were before party mode; false when there was no snapshot.
pub fn party_restore(ha: &HomeAssistant) -> Result<bool> {
    let scene = format!("scene.{PARTY_SCENE}");
    if ha.state(&scene).is_err() {
        return Ok(false);
    }
    // Queued behind the calls before it when the client writes behind (standing down).
    ha.send("scene", "turn_on", json!({ "entity_id": scene }))?;
    ha.send("scene", "delete", json!({ "entity_id": scene }))?;
    Ok(true)
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

/// The answer to a house request. `here`: the room the microphone is in; `name`: the assistant's,
/// for the message on a phone it rings.
pub fn answer(
    ha: &mut HomeAssistant,
    command: &HouseCommand,
    here: Option<&str>,
    name: &str,
    lang: Lang,
) -> Result<String> {
    let no = lang == Lang::Norwegian;
    let say = |english: &str, norwegian: &str| if no { norwegian.to_owned() } else { english.to_owned() };
    Ok(match command {
        HouseCommand::Status => {
            let alerts = alerts::active(ha)?;
            let lights: Vec<Light> = ha.lights_on()?.into_iter().filter(|l| !l.status_led()).collect();
            status_answer(&alerts, &lights, &snapshot(ha)?, lang)
        }
        HouseCommand::Sun(event) => sun_answer(&ha.state("sun.sun")?, *event, lang)?,
        HouseCommand::FindPhone(who) => {
            let phones = phones(ha)?;
            let picked = pick_phones(&phones, who.as_deref());
            for phone in &picked {
                ring(ha, phone, name)?;
                info!(phone = phone.id, "ringing phone");
            }
            rang_answer(&picked, who.as_deref(), lang)
        }
        HouseCommand::PhoneBattery(who) => {
            let phones = phones(ha)?;
            battery_answer(&pick_phones(&phones, who.as_deref()), who.as_deref(), lang)
        }
        HouseCommand::GuestWifi(switch) => {
            let Some(wifi) = entities(ha, "switch", "guest")?.into_iter().next() else {
                return Ok(say(
                    "I can't find a guest Wi-Fi in Home Assistant.",
                    "Jeg finner ikke gjestenettet i Home Assistant.",
                ));
            };
            match switch {
                Switch::On | Switch::Off => {
                    let on = *switch == Switch::On;
                    ha.service("switch", if on { "turn_on" } else { "turn_off" }, json!({ "entity_id": wifi.id }))?;
                    match on {
                        true => say("Okay, the guest Wi-Fi is on.", "Ok, gjestenettet er på."),
                        false => say("Okay, the guest Wi-Fi is off.", "Ok, gjestenettet er av."),
                    }
                }
                Switch::Status if wifi.state != "on" => say("The guest Wi-Fi is off.", "Gjestenettet er av."),
                Switch::Status => {
                    let clients = match (wifi.clients.parse::<u32>(), no) {
                        (Ok(0), false) => " Nobody is connected.".to_owned(),
                        (Ok(0), true) => " Ingen er koblet til.".to_owned(),
                        (Ok(1), false) => " One device is connected.".to_owned(),
                        (Ok(1), true) => " Én enhet er koblet til.".to_owned(),
                        (Ok(n), false) => format!(" {n} devices are connected."),
                        (Ok(n), true) => format!(" {n} enheter er koblet til."),
                        (Err(_), _) => String::new(),
                    };
                    say("The guest Wi-Fi is on.", "Gjestenettet er på.") + &clients
                }
            }
        }
        HouseCommand::Camera(switch) => {
            let privacy = entities(ha, "switch", "privacy")?;
            if privacy.is_empty() {
                return Ok(say(
                    "I can't find a camera with a privacy mode.",
                    "Jeg finner ikke noe kamera med personvernmodus.",
                ));
            }
            let ids: Vec<&str> = privacy.iter().map(|p| p.id.as_str()).collect();
            match switch {
                Switch::On => {
                    ha.service("switch", "turn_off", json!({ "entity_id": ids }))?;
                    say("Okay, the camera is back on.", "Ok, kameraet er på igjen.")
                }
                Switch::Off => {
                    ha.service("switch", "turn_on", json!({ "entity_id": ids }))?;
                    say(
                        "Okay, privacy mode is on. The camera isn't recording.",
                        "Ok, personvernmodus er på. Kameraet tar ikke opp.",
                    )
                }
                Switch::Status if privacy.iter().any(|p| p.state == "on") => {
                    say("The camera is in privacy mode.", "Kameraet er i personvernmodus.")
                }
                Switch::Status if entities(ha, "camera", "")?.iter().any(|c| c.state == "recording") => {
                    say("The camera is recording.", "Kameraet tar opp.")
                }
                Switch::Status => say("The camera is on, but not recording.", "Kameraet er på, men tar ikke opp."),
            }
        }
        HouseCommand::Alarm(switch) => {
            let Some(panel) = entities(ha, "alarm_control_panel", "")?.into_iter().next() else {
                return Ok(say("There's no alarm in Home Assistant.", "Det er ingen alarm i Home Assistant."));
            };
            match switch {
                Switch::Off => say(
                    "I won't disarm the alarm by voice; anyone at the door could ask. Use the app instead.",
                    "Jeg slår ikke av alarmen med stemmen; hvem som helst ved døra kunne spurt. Bruk appen.",
                ),
                Switch::Status => alarm_sentence(&panel.state, lang)
                    .unwrap_or_else(|| say("I can't tell whether the alarm is on.", "Jeg vet ikke om alarmen er på.")),
                Switch::On if panel.state.starts_with("armed") => {
                    say("The alarm is already armed.", "Alarmen er allerede på.")
                }
                Switch::On => {
                    ha.service("alarm_control_panel", "alarm_arm_away", json!({ "entity_id": panel.id }))?;
                    info!(panel = panel.id, "alarm armed");
                    say("Okay, the alarm is armed.", "Ok, alarmen er på.")
                }
            }
        }
        HouseCommand::Climate(place, reading) => climate_answer(&climate_readings(ha)?, place, *reading, here, lang),
        HouseCommand::Sensors(SensorQuery::Battery(name)) => {
            sensors::battery_answer(&sensors::batteries(ha)?, name.as_deref(), lang)
        }
        HouseCommand::Sensors(SensorQuery::Leaks) => sensors::leak_answer(&sensors::water_sensors(ha)?, lang),
    })
}

/// The question asked before a change that a misheard request must not make on its own: the
/// camera's privacy mode ("stop the timer" was heard as "Stop the camera.") and arming the alarm.
pub fn confirm_question(command: &HouseCommand, lang: Lang) -> Option<&'static str> {
    let no = lang == Lang::Norwegian;
    Some(match command {
        HouseCommand::Camera(Switch::Off) if no => "Skal jeg slå på personvernmodus?",
        HouseCommand::Camera(Switch::Off) => "Turn on privacy mode?",
        HouseCommand::Camera(Switch::On) if no => "Skal kameraet ta opp igjen?",
        HouseCommand::Camera(Switch::On) => "Turn privacy mode off?",
        HouseCommand::Alarm(Switch::On) if no => "Skal jeg slå på alarmen?",
        HouseCommand::Alarm(Switch::On) => "Arm the alarm?",
        _ => return None,
    })
}

/// Bedtime light colour: warm white.
const BEDTIME_KELVIN: u32 = 2700;

/// What bedtime leaves on.
pub fn in_bedroom(light: &Light) -> bool {
    ha::is_room(&light.area, "bedroom") || ha::is_room(&light.name, "bedroom")
}

/// The bedroom lights that are on which bedtime dims. A Hue room is also its own lights: the
/// lights, or the room when that is all there is.
pub fn bedtime_dimmed(bedroom: &[Light]) -> Vec<&str> {
    let single: Vec<&Light> = bedroom.iter().filter(|l| !l.group).collect();
    if single.is_empty() { bedroom.iter().collect() } else { single }.iter().map(|l| l.id.as_str()).collect()
}

/// The lights coming home switches on, when none of them is on yet: the `home_room` setting, else
/// the room the microphone is in after dark.
pub fn home_target(ha: &mut HomeAssistant, home_room: Option<&str>, here: Option<&str>) -> Result<Option<Target>> {
    let room = match home_room {
        Some(room) => Some(room),
        None => here.filter(|_| ha.state("sun.sun").is_ok_and(|s| s["state"] == "below_horizon")),
    };
    Ok(room.map(|room| ha.find(room)).transpose()?.flatten())
}

/// The settings a routine reads.
#[derive(Debug, Clone, Copy, Default)]
pub struct RoutineSettings<'a> {
    /// The room coming home lights; else the room the microphone is in, after dark.
    pub home_room: Option<&'a str>,
    /// The bedroom lights' brightness at bedtime, in percent.
    pub bedtime_light: u8,
}

/// Leaving, coming home and bedtime. Lights a routine switched count as the last lights, so
/// "undo" or "turn them back on" reverses it. The music coming home starts is the engine's.
pub fn run_routine(
    ha: &mut HomeAssistant,
    routine: Routine,
    here: Option<&str>,
    settings: RoutineSettings,
    lang: Lang,
) -> Result<Done> {
    let no = lang == Lang::Norwegian;
    let mut parts = Vec::new();
    let mut switched_lights = None;
    match routine {
        Routine::Leaving | Routine::Bedtime => {
            let bedtime = routine == Routine::Bedtime;
            let keep = |l: &Light| bedtime && in_bedroom(l);
            let switched = everything_off(ha, keep)?;
            let except = if bedtime && !switched.lights.is_empty() {
                Some(if no { "soverommet" } else { "bedroom" })
            } else {
                None
            };
            parts.push(switched_answer(&switched, except, lang));
            if bedtime {
                let bedroom: Vec<Light> = ha.lights_on()?.into_iter().filter(|l| keep(l)).collect();
                let ids = bedtime_dimmed(&bedroom);
                if !ids.is_empty() {
                    let data = json!({
                        "entity_id": ids,
                        "brightness_pct": settings.bedtime_light,
                        "color_temp_kelvin": BEDTIME_KELVIN,
                    });
                    ha.send("light", "turn_on", data)?;
                    parts.push(
                        if no { "Lyset på soverommet er dempet." } else { "The bedroom lights are dimmed." }.into(),
                    );
                }
            }
            if !switched.lights.is_empty() {
                let ids = switched.lights.iter().map(|l| l.id.clone()).collect();
                let label = if no { "huset" } else { "house" }.to_owned();
                switched_lights = Some((Target { label, ids, area: true }, false));
            }
            parts.extend(alerts::active(ha)?.iter().map(|a| alerts::sentence(a, lang)));
            if !bedtime && snapshot(ha)?.alarm == "disarmed" {
                parts.push(
                    if no {
                        "Alarmen er av. Si slå på alarmen hvis du vil."
                    } else {
                        "The alarm is off. Say arm the alarm to set it."
                    }
                    .into(),
                );
            }
            parts.push(
                match (bedtime, no) {
                    (false, false) => "See you later.",
                    (false, true) => "Ha det bra.",
                    (true, false) => "Good night.",
                    (true, true) => "God natt.",
                }
                .into(),
            );
        }
        Routine::Lockdown | Routine::StandDown | Routine::OpenSource => unreachable!("the worker runs these itself"),
        Routine::Home => {
            parts.push(if no { "Velkommen hjem." } else { "Welcome home." }.into());
            if let Some(target) = home_target(ha, settings.home_room, here)? {
                let on = ha.lights_on()?;
                if !on.iter().any(|l| target.ids.contains(&l.id)) {
                    ha.set(&target, true)?;
                    let label = target.label.to_lowercase();
                    parts.push(if no {
                        format!("Jeg har skrudd på lyset i {label}.")
                    } else {
                        format!("I've turned on the {label} lights.")
                    });
                    switched_lights = Some((target, true));
                }
            }
            parts.extend(alerts::active(ha)?.iter().map(|a| alerts::sentence(a, lang)));
            if snapshot(ha)?.alarm.starts_with("armed") {
                parts.push(if no { "Alarmen er fortsatt på." } else { "The alarm is still armed." }.into());
            }
        }
    }
    let lights = match switched_lights {
        Some((target, on)) => Lights::Changed(target, LightChange::Switched(on)),
        None => Lights::Same,
    };
    Ok(Done { lights, ..Done::say(parts.join(" ")) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asks_before_privacy_mode_and_the_alarm() {
        assert_eq!(confirm_question(&HouseCommand::Camera(Switch::Off), Lang::English), Some("Turn on privacy mode?"));
        assert_eq!(confirm_question(&HouseCommand::Alarm(Switch::On), Lang::English), Some("Arm the alarm?"));
        assert_eq!(confirm_question(&HouseCommand::Camera(Switch::Status), Lang::English), None);
    }

    #[test]
    fn routine_answers_are_rendered_ahead() {
        let light =
            Light { id: "light.a".into(), name: "A".into(), area: "Kitchen".into(), group: false, members: Vec::new() };
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

    fn words(text: &str) -> Vec<String> {
        crate::dialog::words(text)
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
        // Humidity without a place is this room's: the weather never says it.
        assert_eq!(house("what's the humidity"), climate(Here, Humidity));
        assert_eq!(house("hva er luftfuktigheten"), climate(Here, Humidity));
        assert_eq!(house("what's the humidity outside"), None);
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
            "It's 24 degrees in the living room. It's 19 degrees in the bedroom. It's 22 degrees in the bathroom."
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
        assert_eq!(house("when does it get light"), Some(HouseCommand::Sun(Sunrise)));
        assert_eq!(house("when will it be light"), Some(HouseCommand::Sun(Sunrise)));
        assert_eq!(house("is it dark outside"), Some(HouseCommand::Sun(IsDark(true))));
        assert_eq!(house("is it light yet"), Some(HouseCommand::Sun(IsDark(false))));
        assert_eq!(house("er det mørkt ute"), Some(HouseCommand::Sun(IsDark(true))));
        assert_eq!(house("is it too dark in here"), None);
    }

    fn in_hours(hours: i64) -> String {
        jiff::Timestamp::now().checked_add(jiff::SignedDuration::from_hours(hours)).unwrap().to_string()
    }

    fn hhmm(at: &str) -> String {
        at.parse::<jiff::Timestamp>().unwrap().to_zoned(jiff::tz::TimeZone::system()).strftime("%H:%M").to_string()
    }

    #[test]
    fn says_whether_it_is_dark() {
        let soon = in_hours(3);
        let sun = |elevation: f64| json!({ "attributes": { "elevation": elevation, "next_rising": soon, "next_dusk": soon } });
        let answer =
            |elevation: f64, dark: bool| sun_answer(&sun(elevation), SunEvent::IsDark(dark), Lang::English).unwrap();
        assert!(answer(-20.0, true).starts_with("Yes, it's dark. It gets light at about "));
        assert!(answer(-20.0, false).starts_with("No, it's still dark. "));
        assert!(answer(15.0, true).starts_with("No, it's still light. It gets dark at about "));
        assert!(answer(-3.0, false).starts_with("Yes, it's light. It gets dark at about "));
        // Light comes at dawn, the -6 degree line, before sunrise.
        let (rising, dawn) = (in_hours(5), in_hours(4));
        let sun = json!({ "attributes": { "elevation": -20.0, "next_rising": rising, "next_dawn": dawn } });
        assert_eq!(
            sun_answer(&sun, SunEvent::IsDark(true), Lang::English).unwrap(),
            format!("Yes, it's dark. It gets light at about {}.", hhmm(&dawn))
        );
    }

    #[test]
    fn when_it_gets_dark_while_light_at_night() {
        // Light near midnight in June, with dusk after midnight (tomorrow's date): not "already dark".
        let dusk = in_hours(1);
        let sun = json!({ "attributes": { "elevation": -3.0, "next_dusk": dusk } });
        assert_eq!(
            sun_answer(&sun, SunEvent::Dark, Lang::English).unwrap(),
            format!("It gets dark at about {}.", hhmm(&dusk))
        );
        // Weeks of light far north: the date of the next dusk.
        let weeks = in_hours(24 * 20);
        let sun = json!({ "attributes": { "elevation": 5.0, "next_dusk": weeks } });
        let answer = sun_answer(&sun, SunEvent::Dark, Lang::English).unwrap();
        assert!(answer.starts_with("It doesn't get dark for now. It next gets dark on "), "{answer}");
        // No dusk at all.
        let sun = json!({ "attributes": { "elevation": 5.0 } });
        assert_eq!(sun_answer(&sun, SunEvent::Dark, Lang::English).unwrap(), "It doesn't get dark for now.");
        let sun = json!({ "attributes": { "elevation": -10.0 } });
        assert_eq!(
            sun_answer(&sun, SunEvent::IsDark(true), Lang::Norwegian).unwrap(),
            "Ja, det er mørkt. Det blir ikke lyst med det første."
        );
    }

    #[test]
    fn battery_and_leak_questions_are_the_sensors() {
        use crate::sensors::SensorQuery;
        assert_eq!(house("which batteries are low"), Some(HouseCommand::Sensors(SensorQuery::Battery(None))));
        assert_eq!(house("is there a leak"), Some(HouseCommand::Sensors(SensorQuery::Leaks)));
        // The phone's battery is the phone skill's.
        assert_eq!(house("what's the battery on my phone"), Some(HouseCommand::PhoneBattery(None)));
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
        assert_eq!(house("is the alarm on"), Some(Alarm(Switch::Status)));
        assert_eq!(house("Is the alarm set?"), Some(Alarm(Switch::Status)));
        assert_eq!(house("is the house alarm on"), Some(Alarm(Switch::Status)));
        assert_eq!(house("is my alarm set"), None);
    }

    #[test]
    fn routines() {
        let routine = |t: &str| routine(&words(t));
        assert_eq!(routine("I'm leaving"), Some(Routine::Leaving));
        assert_eq!(routine("Leaving."), None);
        assert_eq!(routine("we're leaving"), Some(Routine::Leaving));
        assert_eq!(routine("I am leaving now"), Some(Routine::Leaving));
        assert_eq!(routine("you're leaving"), None);
        assert_eq!(routine("leaving now"), Some(Routine::Leaving));
        assert_eq!(routine("Look down."), Some(Routine::Lockdown));
        assert_eq!(routine("look down at the floor"), None);
        assert_eq!(routine("the leaving party"), None);
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
        assert_eq!(routine("what do you think about open source"), Some(Routine::OpenSource));
        assert_eq!(routine("What do you think of open-source?"), Some(Routine::OpenSource));
        assert_eq!(routine("do you like open source"), Some(Routine::OpenSource));
        assert_eq!(routine("how do you feel about open source software"), Some(Routine::OpenSource));
        assert_eq!(routine("hva synes du om åpen kildekode"), Some(Routine::OpenSource));
        assert_eq!(routine("liker du åpen kildekode"), Some(Routine::OpenSource));
        assert_eq!(routine("what is open source"), None);
    }

    #[test]
    fn leaving_misheard_alone() {
        let misheard = |t: &str| misheard_leaving(&words(t));
        assert!(misheard("Leaving."));
        assert!(misheard("You're leaving."));
        assert!(misheard("okay, leaving"));
        assert!(misheard("you are leaving"));
        assert!(!misheard("you're leaving already?"));
        assert!(!misheard("the leaving party"));
        assert!(!misheard("I'm leaving"));
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
        let lights = vec![Light {
            id: "light.a".into(),
            name: "A".into(),
            area: "Kitchen".into(),
            group: false,
            members: Vec::new(),
        }];
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
        let house = Light {
            id: "light.house".into(),
            name: "House Lights".into(),
            area: String::new(),
            group: true,
            members: Vec::new(),
        };
        assert_eq!(rooms_lit(&[house, lights[0].clone()]), ["kitchen"]);
    }

    /// The batteries as the shared template fragment lists them: a device with two level
    /// sensors (one low), a binary "battery low" sensor, a dead sensor and a fine one.
    const BATTERY_STATES: &str = r#"[
        {"name": "Hall Climate Battery Level", "level": 64.0, "state": "64", "device": "d1", "area": "Hall"},
        {"name": "Hall Climate Battery", "level": 15.4, "state": "15.4", "device": "d1", "area": "Hall"},
        {"name": "Smoke Alarm Battery Low", "low": true, "state": "on", "device": "d2", "area": ""},
        {"name": "Door Contact Battery", "level": null, "state": "unavailable", "device": "d3", "area": ""},
        {"name": "Sink Leak Battery", "level": 90.0, "state": "90", "device": "d4", "area": "Kitchen"}]"#;

    #[test]
    fn status_says_the_batteries_as_the_battery_skill_does() {
        let snapshot = parse_snapshot(&format!(r#"{{"alarm": "", "batteries": {BATTERY_STATES}}}"#)).unwrap();
        let status = status_answer(&[], &[], &snapshot, Lang::English);
        let skill = sensors::battery_answer(
            &sensors::from_states(serde_json::from_str(BATTERY_STATES).unwrap()),
            None,
            Lang::English,
        );
        let batteries = "The hall climate battery is at 15 percent. The smoke alarm battery is low. The door contact \
                         battery isn't reporting.";
        assert_eq!(skill, batteries);
        assert_eq!(status, format!("All is well. All the lights are off. {batteries}"));
        let norwegian = status_answer(&[], &[], &snapshot, Lang::Norwegian);
        assert!(norwegian.ends_with(&sensors::battery_answer(&snapshot.batteries, None, Lang::Norwegian)));
    }

    #[test]
    fn status_leaves_phones_to_the_phone_skill() {
        // Phones go by integration, not by entity name, in both the summary and the battery skill.
        assert!(SNAPSHOT_TEMPLATE.contains("integration_entities('mobile_app')"));
        assert!(!SNAPSHOT_TEMPLATE.contains("_battery_level"));
        assert!(!SNAPSHOT_TEMPLATE.contains("< 20"));
    }

    #[test]
    fn status_without_low_batteries_says_nothing_of_them() {
        let snapshot = parse_snapshot(
            r#"{"batteries": [{"name": "Sink Leak Battery", "level": 90.0, "state": "90", "device": "d4"}]}"#,
        )
        .unwrap();
        assert_eq!(status_answer(&[], &[], &snapshot, Lang::English), "All is well. All the lights are off.");
    }
}
