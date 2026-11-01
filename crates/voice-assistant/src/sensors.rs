//! Battery levels and leak sensors: "which batteries are low", "what's the battery on the leak
//! sensor", "is there a leak". Phones are left to the phone skill. A leak that is found is said in
//! the safety alert's own words.

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::alerts::{self, Alert};
use crate::ha::HomeAssistant;
use crate::lang::Lang;
use crate::names::Names;

/// Below this a battery is low; the house status uses the same.
pub const LOW: f64 = 20.0;

const BATTERY_WORDS: &[&str] =
    &["battery", "batteries", "batteri", "batteriet", "batterier", "batteriene", "batterinivå", "batterinivået"];
const LEAK_WORDS: &[&str] = &[
    "leak",
    "leaks",
    "leaking",
    "leakage",
    "flood",
    "flooding",
    "flooded",
    "lekkasje",
    "lekkasjer",
    "lekker",
    "vannlekkasje",
];
/// Words around the device's name that do not name it.
const FILLER: &[&str] = &[
    "the",
    "a",
    "my",
    "our",
    "of",
    "on",
    "in",
    "for",
    "at",
    "is",
    "are",
    "what's",
    "whats",
    "what",
    "how",
    "much",
    "level",
    "left",
    "does",
    "have",
    "has",
    "charge",
    "sensor",
    "sensors",
    "device",
    "low",
    "status",
    "check",
    "den",
    "det",
    "på",
    "i",
    "til",
    "for",
    "hva",
    "er",
    "hvor",
    "mye",
    "har",
    "nivå",
    "nivået",
    "sensoren",
    "sensorene",
    "igjen",
    "lav",
    "lavt",
    "lave",
    "which",
    "any",
    "hvilke",
    "noen",
    "all",
    "alle",
    "there",
    "tell",
    "me",
    "running",
    "getting",
    "dying",
    "flat",
    "dead",
    "empty",
    "need",
    "needs",
    "to",
    "be",
    "changing",
    "changed",
    "replacing",
    "replaced",
    "do",
    "i",
    "we",
    "can",
    "you",
    "tom",
    "tomt",
    "tomme",
    "flate",
    "byttes",
    "skiftes",
    "må",
    "trenger",
];

/// What a sensor question asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SensorQuery {
    /// One device's battery by name, or `None` for the low ones.
    Battery(Option<String>),
    /// "Is there a leak?"
    Leaks,
}

/// A battery or leak question, from the request's words.
pub fn parse(words: &[String]) -> Option<SensorQuery> {
    let has = |list: &[&str]| words.iter().any(|w| list.contains(&w.as_str()));
    if has(BATTERY_WORDS) || words.iter().any(|w| w.starts_with("batteri")) {
        let name: Vec<&str> = words
            .iter()
            .map(String::as_str)
            .filter(|w| !FILLER.contains(w) && !BATTERY_WORDS.contains(w) && !w.starts_with("batteri"))
            .collect();
        return Some(SensorQuery::Battery((!name.is_empty()).then(|| name.join(" "))));
    }
    let first = words.first().map(String::as_str).unwrap_or_default();
    let asks = ["is", "are", "any", "do", "does", "have", "has", "check", "er", "har", "lekker", "sjekk", "noen"]
        .contains(&first);
    let leak = has(LEAK_WORDS) || words.iter().any(|w| w.starts_with("lekkasje"));
    (asks && leak && !about_floods(words)).then_some(SensorQuery::Leaks)
}

/// "Are there any flood warnings", "is there flooding in Bergen": the weather or the news, not
/// the water sensors.
fn about_floods(words: &[String]) -> bool {
    let flood = words.iter().any(|w| w.starts_with("flood"));
    let other = ["leak", "leaks", "leaking", "leakage", "lekker"];
    if !flood || words.iter().any(|w| other.contains(&w.as_str()) || w.contains("lekkasje")) {
        return false;
    }
    let outside = [
        "warning",
        "warnings",
        "alert",
        "alerts",
        "risk",
        "forecast",
        "river",
        "rivers",
        "road",
        "roads",
        "town",
        "city",
        "country",
        "news",
        "farevarsel",
        "varsel",
        "flom",
    ];
    let in_a_place = words.windows(2).any(|p| p[0] == "in" && crate::geo::is_known_place(&p[1]));
    in_a_place || words.iter().any(|w| outside.contains(&w.as_str()))
}

/// A device's battery, one per device (some report it twice, through two integrations).
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct Battery {
    pub name: String,
    /// Percent; `None` when the sensor has no number, as a dead battery's goes unavailable.
    #[serde(default)]
    pub level: Option<f64>,
    /// A battery binary sensor's state, on meaning low; `None` for a level sensor or no state.
    #[serde(default)]
    pub low: Option<bool>,
    #[serde(default)]
    pub device: String,
    #[serde(default)]
    pub area: String,
    /// The entity's state as Home Assistant has it: a level, on/off, "unavailable" or "unknown".
    #[serde(default)]
    pub state: String,
    /// Kept by Home Assistant for an entity whose integration no longer provides it.
    #[serde(default)]
    pub restored: bool,
}

/// More silent batteries than this are counted, not named.
const MOST_SILENT_NAMED: usize = 3;

impl Battery {
    /// Whether it says anything about the battery. "Unknown" is a sensor that has not reported
    /// since a restart, and a restored entity is a leftover of a removed device: neither is a
    /// dead battery.
    fn counts(&self) -> bool {
        self.reporting() || (self.state == "unavailable" && !self.restored)
    }

    fn is_low(&self) -> bool {
        self.level.is_some_and(|l| l < LOW) || self.low == Some(true)
    }

    fn reporting(&self) -> bool {
        self.level.is_some() || self.low.is_some()
    }
}

/// Battery sensors and battery binary sensors (on is low, as Zigbee2MQTT and Ring report it),
/// except the companion app's (phones). Sensors without a number are kept: a dead battery is the
/// one that stops reporting.
const BATTERY_TEMPLATE: &str = r#"{% set phones = integration_entities('mobile_app') %}{% set ns = namespace(out=[]) %}{% for s in states.sensor if s.attributes.device_class == 'battery' and s.entity_id not in phones %}{% set ns.out = ns.out + [{"name": s.name, "level": (s.state | float) if s.state | is_number else none, "state": s.state, "restored": s.attributes.restored or false, "device": device_id(s.entity_id) or s.entity_id, "area": area_name(s.entity_id) or ''}] %}{% endfor %}{% for s in states.binary_sensor if s.attributes.device_class == 'battery' and s.entity_id not in phones %}{% set ns.out = ns.out + [{"name": s.name, "low": (s.state == 'on') if s.state in ['on', 'off'] else none, "state": s.state, "restored": s.attributes.restored or false, "device": device_id(s.entity_id) or s.entity_id, "area": area_name(s.entity_id) or ''}] %}{% endfor %}{{ ns.out | tojson }}"#;

pub fn batteries(ha: &HomeAssistant) -> Result<Vec<Battery>> {
    let all: Vec<Battery> =
        serde_json::from_str(&ha.template(BATTERY_TEMPLATE)?).context("parsing the battery sensors")?;
    Ok(per_device(all.into_iter().filter(Battery::counts).collect()))
}

/// One battery per device: the lowest level, under the shortest name.
fn per_device(all: Vec<Battery>) -> Vec<Battery> {
    let mut out: Vec<Battery> = Vec::new();
    for mut battery in all {
        battery.name = device_name(&battery.name);
        match out.iter_mut().find(|b| b.device == battery.device) {
            Some(seen) => {
                seen.level = match (seen.level, battery.level) {
                    (Some(a), Some(b)) => Some(a.min(b)),
                    (a, b) => a.or(b),
                };
                seen.low = match (seen.low, battery.low) {
                    (Some(a), Some(b)) => Some(a || b),
                    (a, b) => a.or(b),
                };
                if battery.name.len() < seen.name.len() {
                    seen.name = battery.name;
                }
            }
            None => out.push(battery),
        }
    }
    out
}

/// "Kitchen Sink Leak Battery Level" is the kitchen sink leak.
fn device_name(name: &str) -> String {
    let mut name = name.trim().to_lowercase();
    // "Smoke Alarm Battery Low" is a battery binary sensor's name.
    for suffix in [" battery low", " low battery", " level", " battery", " batteri"] {
        if let Some(rest) = name.strip_suffix(suffix) {
            name = rest.trim_end().to_owned();
        }
    }
    name
}

/// Words a device's name stands for, so "the leak sensor" finds the kitchen sink leak and
/// "fuktsensoren" the bathroom climate sensor.
const KINDS: &[&[&str]] = &[
    &["leak", "water", "flood", "moisture", "lekkasje", "vann", "lekk"],
    &["climate", "temperature", "humidity", "thermometer", "klima", "temperatur", "fukt", "termometer"],
    &["vibration", "vibrasjon", "shake"],
    &["dimmer", "switch", "remote", "button", "bryter", "fjernkontroll", "knapp"],
    &["motion", "presence", "bevegelse"],
    &["door", "window", "contact", "dør", "vindu"],
];

/// Whether a spoken word means a word of a device's name: equal, or one starts with the other
/// ("lekkasjesensoren" and "lekkasje").
fn word_matches(said: &str, name_word: &str) -> bool {
    said == name_word
        || (name_word.len() >= 4 && said.starts_with(name_word))
        || (said.len() >= 4 && name_word.starts_with(said))
}

/// More devices than this sharing the best match say too little to list them all.
const MOST_TIED: usize = 4;

/// The batteries a name means: a sound-alike of its whole name, else those sharing most words
/// with it (their area and kind count). Several when the name fits more than one equally ("the
/// leak sensor" with two of them).
pub fn find<'a>(batteries: &'a [Battery], name: &str) -> Vec<&'a Battery> {
    let names = Names::new(batteries.iter().map(|b| b.name.as_str()));
    if let Some(found) = names.best(name) {
        return batteries.iter().filter(|b| b.name == found.name).take(1).collect();
    }
    let said: Vec<&str> = name.split_whitespace().collect();
    let score = |battery: &Battery| {
        let mut words: Vec<String> =
            format!("{} {}", battery.name, battery.area.to_lowercase()).split_whitespace().map(str::to_owned).collect();
        for kind in KINDS {
            if words.iter().any(|w| kind.contains(&w.as_str())) {
                words.extend(kind.iter().map(|w| (*w).to_owned()));
            }
        }
        said.iter().filter(|s| words.iter().any(|w| word_matches(s, w))).count()
    };
    let best = batteries.iter().map(score).max().unwrap_or(0);
    if best == 0 {
        return Vec::new();
    }
    let tied: Vec<&Battery> = batteries.iter().filter(|b| score(b) == best).collect();
    if tied.len() > MOST_TIED {
        return Vec::new();
    }
    tied
}

/// "The kitchen sink leak battery is at 15 percent.", or that it is low or not reporting when it
/// has no level.
pub fn level_sentence(battery: &Battery, lang: Lang) -> String {
    let no = lang == Lang::Norwegian;
    let name = &battery.name;
    match (battery.level, battery.low, no) {
        (Some(level), _, false) => format!("The {name} battery is at {} percent.", level.round()),
        (Some(level), _, true) => format!("Batteriet i {name} er på {} prosent.", level.round()),
        (None, Some(true), false) => format!("The {name} battery is low."),
        (None, Some(true), true) => format!("Batteriet i {name} er lavt."),
        (None, Some(false), false) => format!("The {name} battery is fine."),
        (None, Some(false), true) => format!("Batteriet i {name} er i orden."),
        (None, None, false) => format!("The {name} battery isn't reporting."),
        (None, None, true) => format!("Batteriet i {name} svarer ikke."),
    }
}

/// The answer to a battery question: one device's level, or the low ones.
pub fn battery_answer(batteries: &[Battery], name: Option<&str>, lang: Lang) -> String {
    let no = lang == Lang::Norwegian;
    if batteries.is_empty() {
        return if no {
            "Jeg finner ingen batterier i Home Assistant.".into()
        } else {
            "I don't see any batteries in Home Assistant.".into()
        };
    }
    if let Some(name) = name {
        let found = find(batteries, name);
        if found.is_empty() {
            return if no {
                format!("Jeg finner ikke noe batteri for {name}.")
            } else {
                format!("I can't find a battery for the {name}.")
            };
        }
        return found.iter().map(|b| level_sentence(b, lang)).collect::<Vec<_>>().join(" ");
    }
    let low: Vec<&Battery> = batteries.iter().filter(|b| b.is_low()).collect();
    let silent: Vec<&Battery> = batteries.iter().filter(|b| !b.reporting()).collect();
    let mut parts: Vec<String> = low.iter().map(|b| level_sentence(b, lang)).collect();
    if parts.is_empty() {
        parts.push(if no { "Ingen batterier er lave." } else { "No batteries are low." }.into());
    }
    parts.extend(silent.iter().take(MOST_SILENT_NAMED).map(|b| level_sentence(b, lang)));
    let more = silent.len().saturating_sub(MOST_SILENT_NAMED);
    if more > 0 {
        parts.push(if no {
            format!("Og {more} til svarer ikke.")
        } else {
            format!("And {more} more aren't reporting.")
        });
    }
    parts.join(" ")
}

/// A water sensor and whether it is reporting.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct WaterSensor {
    #[serde(flatten)]
    pub alert: Alert,
    pub state: String,
}

const WATER_TEMPLATE: &str = r#"[{% for s in states.binary_sensor if s.attributes.device_class == 'moisture' %}{"id": {{ s.entity_id | tojson }}, "name": {{ s.name | tojson }}, "class": "moisture", "area": {{ (area_name(s.entity_id) or '') | tojson }}, "state": {{ s.state | tojson }}}{% if not loop.last %},{% endif %}{% endfor %}]"#;

pub fn water_sensors(ha: &HomeAssistant) -> Result<Vec<WaterSensor>> {
    serde_json::from_str(&ha.template(WATER_TEMPLATE)?).context("parsing the water sensors")
}

/// "No leaks.", or the warning for each wet sensor; sensors that are not reporting are named.
pub fn leak_answer(sensors: &[WaterSensor], lang: Lang) -> String {
    let no = lang == Lang::Norwegian;
    if sensors.is_empty() {
        return if no {
            "Jeg finner ingen lekkasjesensorer i Home Assistant.".into()
        } else {
            "I don't see any leak sensors in Home Assistant.".into()
        };
    }
    let wet: Vec<String> =
        sensors.iter().filter(|s| s.state == "on").map(|s| alerts::sentence(&s.alert, lang)).collect();
    let mut parts =
        if wet.is_empty() { vec![if no { "Ingen lekkasjer." } else { "No leaks." }.to_owned()] } else { wet };
    for sensor in sensors.iter().filter(|s| !["on", "off"].contains(&s.state.as_str())) {
        let name = sensor.alert.name.to_lowercase();
        parts.push(if no {
            format!("Sensoren {name} svarer ikke.")
        } else {
            format!("The {name} sensor isn't reporting.")
        });
    }
    parts.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(text: &str) -> Vec<String> {
        text.split_whitespace().map(crate::dialog::normalize).filter(|w| !w.is_empty()).collect()
    }

    fn query(text: &str) -> Option<SensorQuery> {
        parse(&words(text))
    }

    #[test]
    fn battery_and_leak_questions() {
        use SensorQuery::*;
        assert_eq!(query("which batteries are low"), Some(Battery(None)));
        assert_eq!(query("any low batteries?"), Some(Battery(None)));
        assert_eq!(query("what's the battery on the leak sensor"), Some(Battery(Some("leak".into()))));
        assert_eq!(
            query("how much battery does the bedroom dimmer have"),
            Some(Battery(Some("bedroom dimmer".into())))
        );
        assert_eq!(query("hvilke batterier er lave"), Some(Battery(None)));
        assert_eq!(query("hva er batteriet på lekkasjesensoren"), Some(Battery(Some("lekkasjesensoren".into()))));
        assert_eq!(query("is there a leak"), Some(Leaks));
        assert_eq!(query("any leaks?"), Some(Leaks));
        assert_eq!(query("is anything leaking"), Some(Leaks));
        assert_eq!(query("er det noen lekkasje"), Some(Leaks));
        assert_eq!(query("lekker det noe sted"), Some(Leaks));
        assert_eq!(query("play Leaks by the Beatles"), None);
        assert_eq!(query("is the basement flooded"), Some(Leaks));
        assert_eq!(query("are there any flood warnings"), None);
        assert_eq!(query("is there flooding in Bergen"), None);
    }

    fn battery(name: &str, level: f64, device: &str, area: &str) -> Battery {
        Battery {
            name: name.into(),
            level: Some(level),
            low: None,
            device: device.into(),
            area: area.into(),
            ..Default::default()
        }
    }

    fn batteries() -> Vec<Battery> {
        per_device(vec![
            battery("Hall Climate Battery Level", 100.0, "d1", "Hall"),
            battery("Hall Climate Battery", 64.5, "d1", "Hall"),
            battery("Sink Leak Battery", 100.0, "d2", "Kitchen"),
            battery("Study Vibration Battery", 12.4, "d3", "Study"),
            battery("Hue dimmer switch Attic Battery", 90.0, "d4", ""),
        ])
    }

    #[test]
    fn one_battery_per_device() {
        let all = batteries();
        assert_eq!(all.len(), 4);
        assert_eq!(all[0], battery("hall climate", 64.5, "d1", "Hall"));
    }

    #[test]
    fn finds_a_battery_by_what_it_is() {
        let all = batteries();
        let name = |said: &str| match find(&all, said).as_slice() {
            [one] => Some(one.name.as_str()),
            _ => None,
        };
        assert_eq!(name("leak"), Some("sink leak"));
        assert_eq!(name("water"), Some("sink leak"));
        assert_eq!(name("lekkasjesensoren"), Some("sink leak"));
        assert_eq!(name("kitchen"), Some("sink leak"));
        assert_eq!(name("attic dimmer"), Some("hue dimmer switch attic"));
        assert_eq!(name("hall climate"), Some("hall climate"));
        assert_eq!(name("garage door"), None);
    }

    #[test]
    fn battery_answers() {
        let all = batteries();
        assert_eq!(battery_answer(&all, None, Lang::English), "The study vibration battery is at 12 percent.");
        assert_eq!(battery_answer(&all, Some("leak"), Lang::English), "The sink leak battery is at 100 percent.");
        assert_eq!(
            battery_answer(&all, Some("garage door"), Lang::English),
            "I can't find a battery for the garage door."
        );
        let fine: Vec<Battery> = all.into_iter().filter(|b| !b.is_low()).collect();
        assert_eq!(battery_answer(&fine, None, Lang::English), "No batteries are low.");
        assert_eq!(battery_answer(&fine, None, Lang::Norwegian), "Ingen batterier er lave.");
    }

    fn silent(name: &str, device: &str) -> Battery {
        Battery { name: name.into(), device: device.into(), state: "unavailable".into(), ..Default::default() }
    }

    #[test]
    fn dead_and_binary_batteries() {
        let mut all = batteries();
        // A dead battery's sensor goes unavailable: no number, but it is the one to change.
        all.push(silent("door contact", "d5"));
        // A battery binary sensor: on is low.
        all.push(Battery { name: "smoke alarm".into(), low: Some(true), device: "d6".into(), ..Default::default() });
        assert_eq!(
            battery_answer(&all, None, Lang::English),
            "The study vibration battery is at 12 percent. The smoke alarm battery is low. The door contact battery \
             isn't reporting."
        );
        let quiet: Vec<Battery> = all.into_iter().filter(|b| !b.is_low()).collect();
        assert_eq!(
            battery_answer(&quiet, None, Lang::English),
            "No batteries are low. The door contact battery isn't reporting."
        );
        // A device reporting through both a dead and a working sensor keeps the level.
        let mut level = silent("Hall Battery Level", "d1");
        level.level = Some(50.0);
        let merged = per_device(vec![silent("Hall Battery", "d1"), level]);
        assert_eq!(merged[0].level, Some(50.0));
    }

    #[test]
    fn only_unavailable_batteries_are_silent() {
        // "Unknown" since a restart, or a restored leftover of a removed device: not a dead battery.
        let mut unknown = silent("hall", "d1");
        unknown.state = "unknown".into();
        let mut restored = silent("old remote", "d2");
        restored.restored = true;
        assert!(!unknown.counts());
        assert!(!restored.counts());
        assert!(silent("door", "d3").counts());
        assert!(batteries().iter().all(Battery::counts));
    }

    #[test]
    fn many_silent_batteries_are_counted() {
        let all: Vec<Battery> = ["a", "b", "c", "d", "e"].iter().map(|n| silent(&format!("{n} sensor"), n)).collect();
        assert_eq!(
            battery_answer(&all, None, Lang::English),
            "No batteries are low. The a sensor battery isn't reporting. The b sensor battery isn't reporting. The c \
             sensor battery isn't reporting. And 2 more aren't reporting."
        );
    }

    #[test]
    fn a_low_battery_sensor_is_named_for_its_device() {
        assert_eq!(device_name("Smoke Alarm Battery Low"), "smoke alarm");
        assert_eq!(device_name("Front Door Low Battery"), "front door");
    }

    #[test]
    fn names_every_battery_a_name_fits() {
        let mut all = batteries();
        all.push(battery("Bath Leak Battery", 40.0, "d7", "Bathroom"));
        let all = per_device(all);
        assert_eq!(
            battery_answer(&all, Some("leak"), Lang::English),
            "The sink leak battery is at 100 percent. The bath leak battery is at 40 percent."
        );
    }

    #[test]
    fn reads_the_template_output() {
        let json = r#"[{"name": "Sink Leak Battery", "level": 80.0, "device": "d1", "area": "Kitchen"},
            {"name": "Door Battery", "level": null, "device": "d2", "area": ""},
            {"name": "Smoke Battery", "low": false, "device": "d3", "area": ""}]"#;
        let all: Vec<Battery> = serde_json::from_str(json).unwrap();
        assert_eq!(all[1].level, None);
        assert_eq!(all[2].low, Some(false));
    }

    fn sensor(name: &str, state: &str) -> WaterSensor {
        let alert =
            Alert { id: "binary_sensor.x".into(), name: name.into(), class: "moisture".into(), area: String::new() };
        WaterSensor { alert, state: state.into() }
    }

    #[test]
    fn leak_answers() {
        assert_eq!(leak_answer(&[sensor("Sink Leak", "off")], Lang::English), "No leaks.");
        assert_eq!(
            leak_answer(&[sensor("Sink Leak", "on"), sensor("Washer", "off")], Lang::English),
            "Warning. The sink leak sensor has detected water."
        );
        assert_eq!(
            leak_answer(&[sensor("Sink Leak", "off"), sensor("Washer", "unavailable")], Lang::English),
            "No leaks. The washer sensor isn't reporting."
        );
        assert_eq!(leak_answer(&[], Lang::English), "I don't see any leak sensors in Home Assistant.");
    }
}
