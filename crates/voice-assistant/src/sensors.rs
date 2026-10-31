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
    (asks && (has(LEAK_WORDS) || words.iter().any(|w| w.starts_with("lekkasje")))).then_some(SensorQuery::Leaks)
}

/// A device's battery, one per device (some report it twice, through two integrations).
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Battery {
    pub name: String,
    pub level: f64,
    #[serde(default)]
    pub device: String,
    #[serde(default)]
    pub area: String,
}

/// Battery sensors with a level, except the companion app's (phones).
const BATTERY_TEMPLATE: &str = r#"{% set phones = integration_entities('mobile_app') %}[{% for s in states.sensor if s.attributes.device_class == 'battery' and s.state | is_number and s.entity_id not in phones %}{"name": {{ s.name | tojson }}, "level": {{ s.state | float }}, "device": {{ (device_id(s.entity_id) or s.entity_id) | tojson }}, "area": {{ (area_name(s.entity_id) or '') | tojson }}}{% if not loop.last %},{% endif %}{% endfor %}]"#;

pub fn batteries(ha: &HomeAssistant) -> Result<Vec<Battery>> {
    let all: Vec<Battery> =
        serde_json::from_str(&ha.template(BATTERY_TEMPLATE)?).context("parsing the battery sensors")?;
    Ok(per_device(all))
}

/// One battery per device: the lowest level, under the shortest name.
fn per_device(all: Vec<Battery>) -> Vec<Battery> {
    let mut out: Vec<Battery> = Vec::new();
    for mut battery in all {
        battery.name = device_name(&battery.name);
        match out.iter_mut().find(|b| b.device == battery.device) {
            Some(seen) => {
                seen.level = seen.level.min(battery.level);
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
    for suffix in [" level", " battery", " batteri"] {
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

/// The battery a name means: a sound-alike of its whole name, else the one sharing most words
/// with it (its area and kind count).
pub fn find<'a>(batteries: &'a [Battery], name: &str) -> Option<&'a Battery> {
    let names = Names::new(batteries.iter().map(|b| b.name.as_str()));
    if let Some(found) = names.best(name) {
        return batteries.iter().find(|b| b.name == found.name);
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
    let best = batteries.iter().map(|b| (score(b), b)).max_by_key(|(s, _)| *s)?;
    // A tie means the name does not say which.
    let ties = batteries.iter().filter(|b| score(b) == best.0).count();
    (best.0 > 0 && ties == 1).then_some(best.1)
}

/// "The kitchen sink leak battery is at 15 percent."
pub fn level_sentence(battery: &Battery, lang: Lang) -> String {
    let level = battery.level.round();
    if lang == Lang::Norwegian {
        format!("Batteriet i {} er på {level} prosent.", battery.name)
    } else {
        format!("The {} battery is at {level} percent.", battery.name)
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
        return match find(batteries, name) {
            Some(battery) => level_sentence(battery, lang),
            None if no => format!("Jeg finner ikke noe batteri for {name}."),
            None => format!("I can't find a battery for the {name}."),
        };
    }
    let low: Vec<&Battery> = batteries.iter().filter(|b| b.level < LOW).collect();
    if low.is_empty() {
        return if no { "Ingen batterier er lave." } else { "No batteries are low." }.into();
    }
    low.iter().map(|b| level_sentence(b, lang)).collect::<Vec<_>>().join(" ")
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
    }

    fn battery(name: &str, level: f64, device: &str, area: &str) -> Battery {
        Battery { name: name.into(), level, device: device.into(), area: area.into() }
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
        let name = |said: &str| find(&all, said).map(|b| b.name.as_str());
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
        let fine: Vec<Battery> = all.into_iter().filter(|b| b.level >= LOW).collect();
        assert_eq!(battery_answer(&fine, None, Lang::English), "No batteries are low.");
        assert_eq!(battery_answer(&fine, None, Lang::Norwegian), "Ingen batterier er lave.");
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
