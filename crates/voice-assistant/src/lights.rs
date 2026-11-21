//! Lights: which ones a request means, switching and dimming them, what is on, and the answers.
//! The parser is [`intent`](crate::intent); the engine keeps which lights "them" and "undo" mean
//! and passes them in.

use anyhow::Result;
use tracing::info;

use crate::dialog::normalize;
use crate::ha::{self, HomeAssistant, Target};
use crate::house;
use crate::intent::{says_off, LightLevel};
use crate::lang::Lang;
use crate::skill::{Ask, Done, LightChange, Lights};

/// The scene a light level change keeps the lights' previous look in, for "undo".
pub const UNDO_SCENE: &str = "lights_undo";

/// "In here", "this room", "her inne", "dette rommet".
fn mentions_this_room(request: &str) -> bool {
    let text = format!(" {} ", request.to_lowercase().replace(|c: char| !c.is_alphanumeric() && c != ' ', " "));
    [" here ", " this room ", " her inne ", " her ", " dette rommet ", " rommet her "].iter().any(|p| text.contains(p))
}

/// "All the lights", "every light", "the whole house": every light, not this room's. Only asked
/// when the request names no room or light ("all the kitchen lights" is the kitchen).
fn mentions_everywhere(request: &str) -> bool {
    request.split_whitespace().map(normalize).any(|w| {
        ["all", "every", "everything", "everywhere", "house", "alle", "alt", "overalt", "huset"].contains(&w.as_str())
    })
}

/// "Them", "it", "those": the lights just switched rather than the room.
fn refers_back(request: &str) -> bool {
    request
        .split_whitespace()
        .map(normalize)
        .any(|w| ["them", "it", "those", "these", "they", "that", "dem", "den", "det", "de"].contains(&w.as_str()))
}

/// The lights a request means.
#[derive(Debug, Clone, PartialEq)]
pub enum Which {
    Lights(Target),
    /// No idea which room.
    Unknown,
    /// It names a light that is not there.
    NotFound,
}

/// The lights a request means: the ones it names; "in here" and no name at all mean `room`,
/// where the microphone is; "them" the `recent` lights.
pub fn target(
    ha: &mut HomeAssistant,
    request: &str,
    room: Option<&str>,
    recent: Option<Target>,
    lang: Lang,
) -> Result<Which> {
    let found = ha.find(request)?;
    let here = room.and_then(|room| ha.find(room).ok().flatten());
    let lights = match found {
        Some(target) => Some(target),
        None if mentions_everywhere(request) => {
            let lights = ha.lights()?;
            // Each lamp, not the room groups as well, so nothing is switched twice.
            let ids = lights.iter().filter(|l| !l.group && !l.status_led()).map(|l| l.id.clone()).collect();
            let label = if lang == Lang::Norwegian { "huset" } else { "house" }.to_owned();
            Some(Target { label, ids, area: true })
        }
        None if mentions_this_room(request) => here,
        None if !ha::names_something(request) => {
            if refers_back(request) {
                recent.or(here)
            } else {
                here.or(recent)
            }
        }
        None => return Ok(Which::NotFound),
    };
    Ok(lights.map_or(Which::Unknown, Which::Lights))
}

/// The answer to a light that is not there.
pub fn not_found(lang: Lang) -> String {
    if lang == Lang::Norwegian { "Jeg fant ikke det lyset." } else { "I couldn't find that light." }.into()
}

/// "Turn on the lights": the lights the request means, or "Which room?".
pub fn turn(
    ha: &mut HomeAssistant,
    request: &str,
    on: bool,
    room: Option<&str>,
    recent: Option<Target>,
    lang: Lang,
) -> Result<Done> {
    Ok(match target(ha, request, room, recent, lang)? {
        Which::Lights(target) => switch(ha, &target, on, lang, false)?,
        Which::Unknown => Done {
            ask: Some(Ask::LightsRoom(on)),
            ..Done::say(if lang == Lang::Norwegian { "Hvilket rom?" } else { "Which room?" })
        },
        Which::NotFound => Done::say(not_found(lang)),
    })
}

/// Switches `target` on or off. `undo`: it puts back what the last change did.
pub fn switch(ha: &HomeAssistant, target: &Target, on: bool, lang: Lang, undo: bool) -> Result<Done> {
    ha.set(target, on)?;
    info!(label = target.label, ids = ?target.ids, on, undo, "lights switched");
    Ok(Done {
        lights: Lights::Changed(target.clone(), LightChange::Switched(on)),
        ..Done::say(switched_answer(target, on, undo, lang == Lang::Norwegian))
    })
}

fn switched_answer(target: &Target, on: bool, undo: bool, no: bool) -> String {
    let label = target.label.to_lowercase();
    let state = match (no, on, undo) {
        (false, true, false) => "on",
        (false, false, false) => "off",
        (false, true, true) => "back on",
        (false, false, true) => "back off",
        (true, true, _) => "på",
        (true, false, _) => "av",
    };
    match (no, target.area) {
        (false, true) => format!("Okay, the {label} lights are {state}."),
        (false, false) => format!("Okay, the {label} is {state}."),
        (true, true) => format!("Ok, lyset i {label} er {state}."),
        (true, false) => format!("Ok, {label} er {state}."),
    }
}

/// "Dim the kitchen", "make it blue": a snapshot of the lights' look first, for "undo".
pub fn level(
    ha: &mut HomeAssistant,
    request: &str,
    level: &LightLevel,
    room: Option<&str>,
    recent: Option<Target>,
    lang: Lang,
) -> Result<Done> {
    let no = lang == Lang::Norwegian;
    let target = match target(ha, request, room, recent, lang)? {
        Which::Lights(target) => target,
        Which::Unknown if no => return Ok(Done::say("Si hvilket rom, for eksempel: demp lyset i stua.")),
        Which::Unknown => return Ok(Done::say("Say which room, for example: dim the living room.")),
        Which::NotFound => return Ok(Done::say(not_found(lang))),
    };
    let lamps = ha::each_lamp(ha.lights()?, &target.ids);
    // Queued ahead of the change, so the snapshot holds the look from before it.
    let snapshot = serde_json::json!({ "scene_id": UNDO_SCENE, "snapshot_entities": lamps });
    ha.send("scene", "create", snapshot)?;
    ha.set_level(&target, level)?;
    info!(label = target.label, ids = ?target.ids, ?level, "light level set");
    let answer = level_answer(&target, level, no);
    Ok(Done { lights: Lights::Changed(target, LightChange::Level), ..Done::say(answer) })
}

fn level_answer(target: &Target, level: &LightLevel, no: bool) -> String {
    let label = target.label.to_lowercase();
    let lights = match (no, target.area) {
        (false, true) => format!("the {label} lights are"),
        (false, false) => format!("the {label} is"),
        (true, _) => format!("lyset i {label} er"),
    };
    match (no, level) {
        (false, LightLevel::Percent(0)) => format!("Okay, {lights} off."),
        (true, LightLevel::Percent(0)) => format!("Ok, {lights} av."),
        (false, LightLevel::Percent(n)) => format!("Okay, {lights} at {n} percent."),
        (true, LightLevel::Percent(n)) => format!("Ok, {lights} på {n} prosent."),
        (false, LightLevel::Brighter) => "Okay, brighter.".into(),
        (true, LightLevel::Brighter) => "Ok, lysere.".into(),
        (false, LightLevel::Dimmer) => "Okay, dimmed.".into(),
        (true, LightLevel::Dimmer) => "Ok, dempet.".into(),
        (false, LightLevel::Color(colour)) => format!("Okay, {lights} {colour}."),
        (true, LightLevel::Color(_)) => "Ok, fargen er endret.".into(),
        (false, LightLevel::Warm) => "Okay, warmer light.".into(),
        (true, LightLevel::Warm) => "Ok, varmere lys.".into(),
        (false, LightLevel::Cool) => "Okay, cooler light.".into(),
        (true, LightLevel::Cool) => "Ok, kaldere lys.".into(),
    }
}

/// The lights back as they were before a brightness or colour change.
pub fn restore_level(ha: &HomeAssistant, target: &Target, lang: Lang) -> Result<Done> {
    let no = lang == Lang::Norwegian;
    let scene = serde_json::json!({ "entity_id": format!("scene.{UNDO_SCENE}") });
    ha.send("scene", "turn_on", scene)?;
    info!(label = target.label, ids = ?target.ids, "light level undone");
    let label = target.label.to_lowercase();
    let answer = match (no, target.area) {
        (false, true) => format!("Okay, the {label} lights are back as they were."),
        (false, false) => format!("Okay, the {label} is back as it was."),
        (true, true) => format!("Ok, lyset i {label} er som før."),
        (true, false) => format!("Ok, {label} er som før."),
    };
    Ok(Done { lights: Lights::Undone, ..Done::say(answer) })
}

/// The answer when there is nothing to undo.
pub fn nothing_to_undo(lang: Lang) -> String {
    if lang == Lang::Norwegian { "Det er ingenting å angre." } else { "There's nothing to undo." }.into()
}

/// "Which lights are on?": the rooms with lights on; with a room or light named, yes or no.
pub fn status(ha: &mut HomeAssistant, request: &str, lang: Lang) -> Result<String> {
    let no = lang == Lang::Norwegian;
    let on = ha.lights_on()?;
    let question: String = request
        .split_whitespace()
        .filter(|w| {
            !["anything", "everything", "something", "left", "still", "noe", "alt", "fortsatt"]
                .contains(&normalize(w).as_str())
        })
        .collect::<Vec<_>>()
        .join(" ");
    if ha::names_something(&question) && !mentions_this_room(request) {
        if let Ok(Some(target)) = ha.find(&question) {
            let lit = on.iter().any(|l| target.ids.contains(&l.id));
            let words: Vec<String> = request.split_whitespace().map(normalize).collect();
            return Ok(named_light_answer(&target.label.to_lowercase(), target.area, lit, says_off(&words), no));
        }
    }
    // Network gear and the Voice PE report their status LEDs as lights; asked by name, they
    // answer for themselves, but nobody counts them among the lights left on.
    let on: Vec<_> = on.into_iter().filter(|l| !l.status_led()).collect();
    let places = house::rooms_lit(&on);
    let list = crate::say::join(&places, lang);
    Ok(match (no, places.is_empty()) {
        (false, true) => "All the lights are off.".into(),
        (true, true) => "Alt lyset er av.".into(),
        (false, false) => format!("Lights are on in the {list}."),
        (true, false) => format!("Lyset er på i {list}."),
    })
}

/// "Is the kitchen light on?" or "...off?": yes when the light is as asked.
fn named_light_answer(label: &str, area: bool, lit: bool, asked_off: bool, no: bool) -> String {
    let yes = lit != asked_off;
    let state = if lit { "on" } else { "off" };
    if no {
        let (word, state) = (if yes { "Ja" } else { "Nei" }, if lit { "på" } else { "av" });
        return format!("{word}, lyset i {label} er {state}.");
    }
    let word = if yes { "Yes" } else { "No" };
    if area {
        format!("{word}, the {label} lights are {state}.")
    } else {
        format!("{word}, the {label} is {state}.")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kitchen() -> Target {
        Target { label: "Kitchen".into(), ids: vec!["light.kitchen".into()], area: true }
    }

    #[test]
    fn this_room_and_back_references() {
        assert!(mentions_this_room("turn off the lights in here"));
        assert!(mentions_this_room("turn on the light in this room"));
        assert!(mentions_this_room("skru av lyset her inne"));
        assert!(!mentions_this_room("turn off the lights"));
        assert!(!mentions_this_room("turn off the kitchen lights"));
        assert!(refers_back("turn them off"));
        assert!(refers_back("skru dem på igjen"));
        assert!(!refers_back("turn off the lights"));
        assert!(mentions_everywhere("turn off all the lights"));
        assert!(!mentions_everywhere("turn off the lights"));
    }

    #[test]
    fn a_question_about_off_answers_yes_when_off() {
        assert_eq!(named_light_answer("kitchen", true, false, true, false), "Yes, the kitchen lights are off.");
        assert_eq!(named_light_answer("kitchen", true, true, true, false), "No, the kitchen lights are on.");
        assert_eq!(named_light_answer("kitchen", true, true, false, false), "Yes, the kitchen lights are on.");
        assert_eq!(named_light_answer("kitchen", true, false, false, false), "No, the kitchen lights are off.");
        assert_eq!(named_light_answer("desk lamp", false, false, true, false), "Yes, the desk lamp is off.");
        assert_eq!(named_light_answer("stua", true, false, true, true), "Ja, lyset i stua er av.");
        assert_eq!(named_light_answer("stua", true, true, true, true), "Nei, lyset i stua er på.");
    }

    #[test]
    fn switch_and_level_answers() {
        let lamp = Target { label: "Desk Lamp".into(), ids: vec!["light.desk".into()], area: false };
        assert_eq!(switched_answer(&kitchen(), false, false, false), "Okay, the kitchen lights are off.");
        assert_eq!(switched_answer(&kitchen(), true, true, false), "Okay, the kitchen lights are back on.");
        assert_eq!(switched_answer(&lamp, true, false, false), "Okay, the desk lamp is on.");
        assert_eq!(switched_answer(&kitchen(), true, true, true), "Ok, lyset i kitchen er på.");
        assert_eq!(
            level_answer(&kitchen(), &LightLevel::Percent(40), false),
            "Okay, the kitchen lights are at 40 percent."
        );
        assert_eq!(level_answer(&lamp, &LightLevel::Percent(0), false), "Okay, the desk lamp is off.");
        assert_eq!(
            level_answer(&kitchen(), &LightLevel::Color("blue".into()), false),
            "Okay, the kitchen lights are blue."
        );
        assert_eq!(level_answer(&kitchen(), &LightLevel::Dimmer, true), "Ok, dempet.");
    }

    /// Without Home Assistant every call fails, and the engine says it could not reach it.
    #[test]
    fn an_unreachable_home_assistant_is_an_error() {
        let mut ha = HomeAssistant::new("http://127.0.0.1:9", "test");
        assert!(target(&mut ha, "turn on the kitchen", None, None, Lang::English).is_err());
        assert!(turn(&mut ha, "turn on the kitchen", true, None, None, Lang::English).is_err());
        assert!(status(&mut ha, "which lights are on", Lang::English).is_err());
    }
}
