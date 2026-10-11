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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Routine {
    /// "I'm leaving": everything off.
    Leaving,
    /// "I'm home": a welcome, and the room's lights when it is dark.
    Home,
    /// "I'm going to bed": everything off except the bedroom.
    Bedtime,
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
    is_status(words).then_some(HouseCommand::Status)
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
    "jeg legger meg",
    "vi legger oss",
    "leggetid",
];

pub fn routine(words: &[String]) -> Option<Routine> {
    if words.len() > 9 {
        return None;
    }
    let text = phrase(words);
    let starts = |list: &[&str]| list.iter().any(|p| text == *p || text.starts_with(&format!("{p} ")));
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
    ha.service(
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
        ha.service("light", "turn_off", json!({ "entity_id": ids }))?;
    }
    let snapshot = snapshot(ha)?;
    if !snapshot.playing.is_empty() {
        let ids: Vec<&str> = snapshot.playing.iter().map(|p| p.id.as_str()).collect();
        ha.service("media_player", "media_pause", json!({ "entity_id": ids }))?;
    }
    let tvs = crate::tv::on_tvs(ha)?;
    if !tvs.is_empty() {
        ha.service("media_player", "turn_off", json!({ "entity_id": tvs }))?;
    }
    Ok(Switched { lights, music: !snapshot.playing.is_empty(), tvs: !tvs.is_empty() })
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
            "how's the house?",
            "Is there anything I should know?",
            "er alt i orden?",
        ] {
            assert_eq!(house(text), Some(HouseCommand::Status), "{text}");
        }
        assert_eq!(house("is everything okay with you and your family over there today"), None);
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
