//! The lockdown: every light red and blinking, the music up, and standing down again. Just for
//! fun: nothing is locked. The Home Assistant calls and the answers are here; the engine runs the
//! rest, since a lockdown also ends party mode, puts the screen on the TV and starts music after
//! the answer.

use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use serde::Deserialize;
use serde_json::json;
use tracing::{info, warn};

use crate::ha::HomeAssistant;
use crate::lang::Lang;
use crate::music::Player;

/// Fixed English lockdown answers, rendered ahead of time.
pub const LOCKDOWN_ANSWERS: &[&str] = &[
    "Lockdown initiated. The house is sealed.",
    "Standing down. Everything is back as it was.",
    "There is no lockdown to end.",
];

/// What the lockdown's English answer ends on, one after the other, spoken in the dark voice
/// ([`tts::darken`](crate::tts::darken)). Only these sentences are darkened, so none may be a
/// sentence another answer says too.
pub const LOCKDOWN_THREATS: &[&str] = &[
    "I will look for you. I will find you. And I will kill you.",
    "You can run. But you cannot hide.",
    "There is nowhere left to hide. I can see you.",
    "Every door is locked. And I am watching you.",
    "Do not move. I can hear you breathing.",
    "Nobody leaves this house. Not tonight.",
];

/// Whether `sentence` is one of the lockdown threats' sentences, for the dark voice.
pub fn threat_sentence(sentence: &str) -> bool {
    LOCKDOWN_THREATS.iter().any(|t| t.split_inclusive(['.', '?', '!']).any(|s| s.trim() == sentence))
}

/// The lockdown's answer; in English it ends on the threat at `turn`, one after the other.
pub fn started_answer(lang: Lang, turn: usize) -> String {
    if lang == Lang::Norwegian {
        return "Nedstengning startet. Huset er forseglet.".into();
    }
    let threat = LOCKDOWN_THREATS[turn % LOCKDOWN_THREATS.len()];
    format!("Lockdown initiated. The house is sealed. {threat}")
}

/// The answer to standing down: `ended` when there was a lockdown to end.
pub fn stood_down_answer(ended: bool, lang: Lang) -> String {
    match (ended, lang == Lang::Norwegian) {
        (true, false) => "Standing down. Everything is back as it was.",
        (true, true) => "Avblåst. Alt er som før.",
        (false, false) => "There is no lockdown to end.",
        (false, true) => "Det er ingen nedstengning å avslutte.",
    }
    .into()
}

/// The scene `scene.create` keeps the lights in while the house is locked down.
pub const LOCKDOWN_SCENE: &str = "lockdown_restore";

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
    let (red, off) = lockdown_lights(ha)?;
    if !locked_down(ha) {
        let all: Vec<&String> = red.iter().chain(&off).collect();
        ha.send("scene", "create", json!({ "scene_id": LOCKDOWN_SCENE, "snapshot_entities": all }))?;
    }
    if !red.is_empty() {
        ha.send("light", "turn_on", json!({ "entity_id": red, "rgb_color": [255, 0, 0], "brightness_pct": 100 }))?;
    }
    if !off.is_empty() {
        ha.send("light", "turn_off", json!({ "entity_id": off }))?;
    }
    Ok(red)
}

/// The lights a lockdown turns red, and the ones without colour it turns off. Party mode keeps a
/// snapshot of both, and streams to the red ones.
pub fn lockdown_lights(ha: &HomeAssistant) -> Result<(Vec<String>, Vec<String>)> {
    let lights: Vec<LockdownLight> = ha.template_json(LOCKDOWN_LIGHTS_TEMPLATE, "parsing the lockdown lights")?;
    let ids = |color: bool| lights.iter().filter(|l| l.color == color).map(|l| l.id.clone()).collect::<Vec<_>>();
    Ok((ids(true), ids(false)))
}

/// The music's volume for a lockdown, from 0 to 1, and the volume it had before (`None` when the
/// player cannot say). `setting` is the lockdown volume in percent.
pub fn music_level(ha: &HomeAssistant, player: &mut Player, setting: u8) -> (f64, Option<f64>) {
    let level = f64::from(setting) / 100.0;
    let Ok(entity) = player.check(ha) else { return (level, None) };
    let volume = ha.state(&entity).ok().and_then(|s| s["attributes"]["volume_level"].as_f64());
    // Music already playing louder than the setting stays as loud: a lockdown never quietens it.
    (level.max(volume.unwrap_or(0.0)), volume)
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
    let groups: Vec<HueGroup> = ha.template_json(HUE_GROUPS_TEMPLATE, "parsing the Hue rooms")?;
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

/// The Hue alert the lockdown's lights pulse with lasts about 15 seconds: started again before it ends.
const ALARM_REPEAT: Duration = Duration::from_secs(14);
/// A lockdown nobody stands down stops blinking after this and stays red.
const ALARM_LONGEST: Duration = Duration::from_secs(30 * 60);

/// The lockdown's red lights blinking, on a thread of their own with their own Home Assistant
/// client, so answers are not held up.
pub struct Alarm {
    stop: Arc<AtomicBool>,
    thread: std::thread::JoinHandle<()>,
}

impl Alarm {
    pub fn start(ha: HomeAssistant, red: Vec<String>) -> std::io::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let thread = std::thread::Builder::new().name("alarm".into()).spawn(move || {
            // The lights go red on the write queue; a pulse before that would flash the old colours.
            ha.flush();
            let start = Instant::now();
            let targets = alarm_targets(&ha, &red).unwrap_or_else(|error| {
                warn!(error = format!("{error:#}"), "no Hue rooms for the lockdown; each light alone");
                red
            });
            info!(?targets, "lockdown blink");
            while !stopped.load(Relaxed) && start.elapsed() < ALARM_LONGEST {
                if let Err(error) = alarm_pulse(&ha, &targets) {
                    warn!(error = format!("{error:#}"), "lockdown blink failed");
                }
                // In short sleeps, so standing down is not kept waiting.
                let pulse = Instant::now();
                while !stopped.load(Relaxed) && pulse.elapsed() < ALARM_REPEAT {
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
            info!(after = ?start.elapsed(), "lockdown blink stopped");
        })?;
        Ok(Self { stop, thread })
    }

    /// Not stopped yet, and not past [`ALARM_LONGEST`].
    pub fn blinking(&self) -> bool {
        !self.thread.is_finished()
    }

    /// Waits for a pulse in flight, so nothing is started after this returns.
    pub fn stop(self) {
        self.stop.store(true, Relaxed);
        let _ = self.thread.join();
    }
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

/// A lockdown's snapshot exists. It outlives an engine restart, not a Home Assistant one.
pub fn locked_down(ha: &HomeAssistant) -> bool {
    ha.state(&format!("scene.{LOCKDOWN_SCENE}")).is_ok()
}

/// Whether a lockdown is on: Home Assistant's snapshot scene once the calls queued so far have
/// been made, and `known_here` (what this engine started and has not stood down) only when Home
/// Assistant cannot be asked. A scene lost to a Home Assistant restart or deleted ends it.
pub fn lockdown_on(ha: &HomeAssistant, known_here: bool) -> bool {
    lockdown_answer(ha.exists(&format!("scene.{LOCKDOWN_SCENE}")).ok(), known_here)
}

fn lockdown_answer(scene: Option<bool>, known_here: bool) -> bool {
    scene.unwrap_or(known_here)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn home_assistant_decides_whether_a_lockdown_is_on() {
        // The scene lost to a Home Assistant restart ends a lockdown this engine started.
        assert!(!lockdown_answer(Some(false), true));
        // One started before an engine restart.
        assert!(lockdown_answer(Some(true), false));
        // Home Assistant unreachable: what this engine knows.
        assert!(lockdown_answer(None, true));
        assert!(!lockdown_answer(None, false));
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

    #[test]
    fn the_answers_are_rendered_ahead() {
        assert_eq!(stood_down_answer(true, Lang::English), LOCKDOWN_ANSWERS[1]);
        assert_eq!(stood_down_answer(false, Lang::English), LOCKDOWN_ANSWERS[2]);
        for (turn, threat) in LOCKDOWN_THREATS.iter().enumerate() {
            let answer = started_answer(Lang::English, turn);
            assert_eq!(answer.strip_prefix(LOCKDOWN_ANSWERS[0]).map(str::trim), Some(*threat));
        }
        // One after the other, and round again.
        assert_eq!(started_answer(Lang::English, LOCKDOWN_THREATS.len()), started_answer(Lang::English, 0));
    }

    /// Without Home Assistant nothing goes red, and the music's volume is the setting.
    #[test]
    fn an_unreachable_home_assistant() {
        let ha = HomeAssistant::new("http://127.0.0.1:9", "test");
        assert!(lockdown(&ha).is_err());
        assert!(!locked_down(&ha));
        assert!(lockdown_on(&ha, true));
        assert_eq!(music_level(&ha, &mut Player::new(None), 85), (0.85, None));
    }
}
