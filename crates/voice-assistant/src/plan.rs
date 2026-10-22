//! What a light request would do, lamp by lamp, without doing it: the hub's preview. The engine
//! picks the lights with the same code the request runs; this expands room groups to their lamps
//! and words each change.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::ha::HomeAssistant;
use crate::intent::LightLevel;

/// Every light now: on or off, a group's members, and whether it shows colours.
const LAMPS_TEMPLATE: &str = r#"[{% for s in states.light %}{"id": {{ s.entity_id | tojson }}, "on": {{ (s.state == 'on') | tojson }}, "members": {{ (s.attributes.entity_id or []) | list | tojson }}, "colour": {{ ((s.attributes.supported_color_modes or []) | select('in', ['hs', 'xy', 'rgb', 'rgbw', 'rgbww']) | list | count > 0) | tojson }}}{% if not loop.last %},{% endif %}{% endfor %}]"#;

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Lamp {
    pub id: String,
    pub on: bool,
    #[serde(default)]
    pub members: Vec<String>,
    #[serde(default)]
    pub colour: bool,
}

pub fn lamps(ha: &HomeAssistant) -> Result<Vec<Lamp>> {
    serde_json::from_str(&ha.template(LAMPS_TEMPLATE)?).context("parsing the lights for a preview")
}

/// What happens to one lamp. `kind` is how the hub colours it: on, off, level, scene, red, party,
/// restore or surprise.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Effect {
    pub kind: &'static str,
    pub text: String,
}

impl Effect {
    pub fn new(kind: &'static str, text: impl Into<String>) -> Self {
        Self { kind, text: text.into() }
    }

    pub fn switch(on: bool) -> Self {
        if on {
            Self::new("on", "on")
        } else {
            Self::new("off", "off")
        }
    }

    /// A brightness or colour change; `on` is whether the lamp is on now, since a step or a colour
    /// switches a dark lamp on.
    pub fn level(level: &LightLevel, on: bool) -> Self {
        let text = match level {
            LightLevel::Percent(0) => return Self::switch(false),
            LightLevel::Percent(n) => format!("{n} %"),
            LightLevel::Brighter => "+25 %".into(),
            LightLevel::Dimmer => "−25 %".into(),
            LightLevel::Color(name) if name == "white" => "white, 4000 K".into(),
            LightLevel::Color(name) => name.clone(),
            LightLevel::Warm => "warm, 2700 K".into(),
            LightLevel::Cool => "cool, 5500 K".into(),
        };
        match (on, level) {
            (false, LightLevel::Dimmer) => Self::new("surprise", format!("comes on, {text}")),
            (false, _) => Self::new("level", format!("on, {text}")),
            (true, _) => Self::new("level", text),
        }
    }
}

/// The lamps a request changes, by entity id, and what else it does ("The music pauses.").
#[derive(Debug, Default, Serialize)]
pub struct Plan {
    pub lights: BTreeMap<String, Effect>,
    pub also: Vec<String>,
}

impl Plan {
    /// Sets `ids`, a room group as each of its lamps, to what `effect` says for each lamp.
    pub fn set(&mut self, lamps: &[Lamp], ids: &[impl AsRef<str>], effect: impl Fn(&Lamp) -> Effect) {
        for id in ids {
            for lamp in expand(lamps, id.as_ref(), 0) {
                self.lights.insert(lamp.id.clone(), effect(lamp));
            }
        }
    }
}

/// A light, or a group's lamps (a group of groups, "House Lights", as theirs).
fn expand<'a>(lamps: &'a [Lamp], id: &str, depth: u8) -> Vec<&'a Lamp> {
    let Some(lamp) = lamps.iter().find(|l| l.id == id) else { return Vec::new() };
    if lamp.members.is_empty() || depth > 3 {
        return vec![lamp];
    }
    lamp.members.iter().flat_map(|m| expand(lamps, m, depth + 1)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lamp(id: &str, on: bool, members: &[&str]) -> Lamp {
        Lamp { id: id.into(), on, members: members.iter().map(|m| (*m).into()).collect(), colour: true }
    }

    #[test]
    fn groups_become_their_lamps() {
        let lamps = vec![
            lamp("light.bed", false, &[]),
            lamp("light.window", true, &[]),
            lamp("light.bedroom", true, &["light.bed", "light.window"]),
            lamp("light.house", true, &["light.bedroom"]),
        ];
        let mut plan = Plan::default();
        plan.set(&lamps, &["light.house"], |l| Effect::level(&LightLevel::Dimmer, l.on));
        assert_eq!(plan.lights.len(), 2);
        assert_eq!(plan.lights["light.bed"].kind, "surprise");
        assert_eq!(plan.lights["light.window"], Effect::new("level", "−25 %"));
        assert!(!plan.lights.contains_key("light.bedroom"));
    }

    #[test]
    fn levels_read_plainly() {
        assert_eq!(Effect::level(&LightLevel::Percent(0), true), Effect::switch(false));
        assert_eq!(Effect::level(&LightLevel::Percent(30), false).text, "on, 30 %");
        assert_eq!(Effect::level(&LightLevel::Color("red".into()), true).text, "red");
    }
}
