//! Safety alerts spoken without being asked: smoke, carbon monoxide, gas, water and breaking glass,
//! from Home Assistant's binary sensors. A sensor counts by its device class, or by its id for
//! cameras that report sounds without one (UniFi Protect's "smoke alarm detected").

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::ha::HomeAssistant;
use crate::lang::Lang;

/// A sensor still reporting is announced again after this long.
pub const REPEAT: Duration = Duration::from_secs(300);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Smoke,
    CarbonMonoxide,
    Gas,
    Water,
    GlassBreak,
    Safety,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Alert {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub class: String,
    #[serde(default)]
    pub area: String,
}

impl Alert {
    pub fn kind(&self) -> Kind {
        match self.class.as_str() {
            "smoke" => Kind::Smoke,
            "carbon_monoxide" => Kind::CarbonMonoxide,
            "gas" => Kind::Gas,
            "moisture" => Kind::Water,
            _ if self.id.contains("smoke") => Kind::Smoke,
            _ if self.id.contains("co_alarm") => Kind::CarbonMonoxide,
            _ if self.id.contains("glass_break") => Kind::GlassBreak,
            _ => Kind::Safety,
        }
    }
}

/// Binary sensors that are on and mean danger.
const ALERTS_TEMPLATE: &str = r#"[{% for s in states.binary_sensor if s.state == 'on' and (s.attributes.device_class in ['smoke', 'carbon_monoxide', 'gas', 'moisture', 'safety'] or 'smoke_alarm' in s.entity_id or 'co_alarm' in s.entity_id or 'glass_break' in s.entity_id) %}{"id": {{ s.entity_id | tojson }}, "name": {{ s.name | tojson }}, "class": {{ (s.attributes.device_class or '') | tojson }}, "area": {{ (area_name(s.entity_id) or '') | tojson }}}{% if not loop.last %},{% endif %}{% endfor %}]"#;

pub fn active(ha: &HomeAssistant) -> Result<Vec<Alert>> {
    serde_json::from_str(&ha.template(ALERTS_TEMPLATE)?).context("parsing the alerts from Home Assistant")
}

/// The warning to speak: "Warning. A smoke alarm is going off in the living room."
pub fn sentence(alert: &Alert, lang: Lang) -> String {
    let no = lang == Lang::Norwegian;
    let area = alert.area.to_lowercase();
    let in_area = match (area.is_empty(), no) {
        (true, _) => String::new(),
        (false, false) => format!(" in the {area}"),
        (false, true) => format!(" i {area}"),
    };
    let name = alert.name.to_lowercase();
    match (alert.kind(), no) {
        (Kind::Smoke, false) => format!("Warning. A smoke alarm is going off{in_area}."),
        (Kind::Smoke, true) => format!("Advarsel. En røykvarsler piper{in_area}."),
        (Kind::CarbonMonoxide, false) => format!("Warning. A carbon monoxide alarm is going off{in_area}."),
        (Kind::CarbonMonoxide, true) => format!("Advarsel. En kullosvarsler piper{in_area}."),
        (Kind::Gas, false) => format!("Warning. Gas detected{in_area}."),
        (Kind::Gas, true) => format!("Advarsel. Gass oppdaget{in_area}."),
        (Kind::Water, false) => format!("Warning. The {name} sensor has detected water."),
        (Kind::Water, true) => format!("Advarsel. Sensoren {name} har oppdaget vann."),
        (Kind::GlassBreak, false) => format!("Warning. Breaking glass was heard{in_area}."),
        (Kind::GlassBreak, true) => format!("Advarsel. Det høres glass som knuses{in_area}."),
        (Kind::Safety, false) => format!("Warning. The {name} sensor reports a problem."),
        (Kind::Safety, true) => format!("Advarsel. Sensoren {name} melder om et problem."),
    }
}

/// Which alerts to announce: new ones at once, those still on every [`REPEAT`]. A sensor that
/// clears and comes back is new again.
#[derive(Default)]
pub struct Watch {
    announced: HashMap<String, Instant>,
}

impl Watch {
    pub fn due<'a>(&mut self, active: &'a [Alert], now: Instant) -> Vec<&'a Alert> {
        self.announced.retain(|id, _| active.iter().any(|a| &a.id == id));
        let mut due = Vec::new();
        for alert in active {
            let last = self.announced.get(&alert.id);
            if last.is_none_or(|at| now.duration_since(*at) >= REPEAT) {
                self.announced.insert(alert.id.clone(), now);
                due.push(alert);
            }
        }
        due
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alert(id: &str, name: &str, class: &str, area: &str) -> Alert {
        Alert { id: id.into(), name: name.into(), class: class.into(), area: area.into() }
    }

    #[test]
    fn sentences_by_kind() {
        let smoke = alert(
            "binary_sensor.g6_instant_smoke_alarm_detected",
            "G6 Instant Smoke alarm detected",
            "",
            "Living Room",
        );
        assert_eq!(sentence(&smoke, Lang::English), "Warning. A smoke alarm is going off in the living room.");
        let leak = alert("binary_sensor.kitchen_sink_leak", "Kitchen Sink Leak", "moisture", "");
        assert_eq!(sentence(&leak, Lang::English), "Warning. The kitchen sink leak sensor has detected water.");
        let glass = alert("binary_sensor.g6_instant_glass_break_detected", "Glass", "", "");
        assert_eq!(sentence(&glass, Lang::English), "Warning. Breaking glass was heard.");
    }

    #[test]
    fn announces_new_alerts_and_repeats_slowly() {
        let mut watch = Watch::default();
        let leak = vec![alert("binary_sensor.leak", "Leak", "moisture", "")];
        let start = Instant::now();
        assert_eq!(watch.due(&leak, start).len(), 1);
        assert!(watch.due(&leak, start + Duration::from_secs(10)).is_empty());
        assert_eq!(watch.due(&leak, start + REPEAT).len(), 1);
        // Cleared, then back: new again.
        assert!(watch.due(&[], start + REPEAT + Duration::from_secs(1)).is_empty());
        assert_eq!(watch.due(&leak, start + REPEAT + Duration::from_secs(2)).len(), 1);
    }
}
