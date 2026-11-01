//! Lights: which ones a request means, switching and dimming them, and undo.

use std::time::{Duration, Instant};

use tracing::info;
use voice_assistant::ha::{self, Target};
use voice_assistant::house;
use voice_assistant::intent::{says_off, LightLevel};
use voice_assistant::lang::Lang;
use voice_assistant::shopping::Change;

use super::{unreachable, Worker};

/// The rolling KV cache is fine for long sessions, but position ids keep growing; start over
/// between requests once this many steps have passed (about 13 minutes).
/// "Them" and "that" refer to the last lights for this long.
const MEMORY: Duration = Duration::from_secs(120);

/// The scene a light level change keeps the lights' previous look in, for "undo".
const UNDO_SCENE: &str = "lights_undo";

/// The last change to the lights, for "undo".
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum LightChange {
    /// Switched on or off.
    Switched(bool),
    /// A brightness or colour change; how they were before is kept in [`UNDO_SCENE`].
    Level,
}

/// "In here", "this room", "her inne", "dette rommet".
fn mentions_this_room(request: &str) -> bool {
    let text = format!(" {} ", request.to_lowercase().replace(|c: char| !c.is_alphanumeric() && c != ' ', " "));
    [" here ", " this room ", " her inne ", " her ", " dette rommet ", " rommet her "].iter().any(|p| text.contains(p))
}

/// "All the lights", "every light", "the whole house": every light, not this room's. Only asked
/// when the request names no room or light ("all the kitchen lights" is the kitchen).
fn mentions_everywhere(request: &str) -> bool {
    request.split_whitespace().map(voice_assistant::dialog::normalize).any(|w| {
        ["all", "every", "everything", "everywhere", "house", "alle", "alt", "overalt", "huset"].contains(&w.as_str())
    })
}

/// "Them", "it", "those": the lights just switched rather than the room.
fn refers_back(request: &str) -> bool {
    request
        .split_whitespace()
        .map(voice_assistant::dialog::normalize)
        .any(|w| ["them", "it", "those", "these", "they", "that", "dem", "den", "det", "de"].contains(&w.as_str()))
}

impl Worker {
    pub(super) fn lights(&mut self, request: &str, on: bool, lang: Lang) -> String {
        match self.light_target(request, lang) {
            Ok(Some(target)) => self.switch(&target, on, lang, false),
            Ok(None) => {
                self.pending_lights = Some(on);
                if lang == Lang::Norwegian { "Hvilket rom?" } else { "Which room?" }.into()
            }
            Err(answer) => answer,
        }
    }

    /// The lights a request means: the ones it names; "in here" and no name at all mean this room;
    /// "them" the last lights. `Ok(None)`: no idea which room. `Err` is the answer to speak.
    pub(super) fn light_target(&mut self, request: &str, lang: Lang) -> Result<Option<Target>, String> {
        let no = lang == Lang::Norwegian;
        let recent = self.recent_lights().map(|(target, _)| target);
        let Some(ha) = &mut self.home_assistant else {
            return Err(if no {
                "Home Assistant er ikke koblet til ennå."
            } else {
                "Home Assistant isn't connected yet."
            }
            .into());
        };
        let found = ha.find(request).map_err(|error| unreachable(&error, no))?;
        let here = self.room.as_deref().and_then(|room| ha.find(room).ok().flatten());
        match found {
            Some(target) => Ok(Some(target)),
            None if mentions_everywhere(request) => {
                let lights = ha.lights().map_err(|error| unreachable(&error, no))?;
                // Each lamp, not the room groups as well, so nothing is switched twice.
                let ids = lights.iter().filter(|l| !l.group && !l.status_led()).map(|l| l.id.clone()).collect();
                let label = if no { "huset" } else { "house" }.to_owned();
                Ok(Some(Target { label, ids, area: true }))
            }
            None if mentions_this_room(request) => Ok(here),
            None if !ha::names_something(request) => {
                Ok(if refers_back(request) { recent.or(here) } else { here.or(recent) })
            }
            None if no => Err("Jeg fant ikke det lyset.".into()),
            None => Err("I couldn't find that light.".into()),
        }
    }

    pub(super) fn light_level(&mut self, request: &str, level: &LightLevel, lang: Lang) -> String {
        let no = lang == Lang::Norwegian;
        let target = match self.light_target(request, lang) {
            Ok(Some(target)) => target,
            Ok(None) if no => return "Si hvilket rom, for eksempel: demp lyset i stua.".into(),
            Ok(None) => return "Say which room, for example: dim the living room.".into(),
            Err(answer) => return answer,
        };
        let Some(ha) = &mut self.home_assistant else { return "Home Assistant isn't connected yet.".into() };
        let lamps = match ha.lights() {
            Ok(lights) => ha::each_lamp(lights, &target.ids),
            Err(error) => return unreachable(&error, no),
        };
        // Queued ahead of the change, so the snapshot holds the look from before it.
        let snapshot = serde_json::json!({ "scene_id": UNDO_SCENE, "snapshot_entities": lamps });
        if let Err(error) = ha.send("scene", "create", snapshot).and_then(|()| ha.set_level(&target, level)) {
            return unreachable(&error, no);
        }
        info!(label = target.label, ids = ?target.ids, ?level, "light level set");
        self.last_lights = Some((target.clone(), LightChange::Level, Instant::now()));
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

    /// "Undo": whichever changed last, the lights or the shopping list.
    pub(super) fn undo(&mut self, lang: Lang) -> String {
        let no = lang == Lang::Norwegian;
        let lights_at = self.last_lights.as_ref().filter(|(.., at)| at.elapsed() < MEMORY).map(|(.., at)| *at);
        let list_at = self.last_list.as_ref().filter(|(_, at)| at.elapsed() < MEMORY).map(|(_, at)| *at);
        if list_at.is_some() && (lights_at.is_none() || list_at > lights_at) {
            let (changes, _) = self.last_list.take().expect("checked above");
            let Some(ha) = &self.home_assistant else { return "Home Assistant isn't connected yet.".into() };
            // Latest first, so a reply that removed and then added comes back in order.
            let mut answers = Vec::new();
            for change in changes.into_iter().rev() {
                if let Err(error) = self.shopping.undo(ha, &change) {
                    answers.push(unreachable(&error, no));
                    break;
                }
                info!(?change, "shopping list change undone");
                answers.push(match (no, change) {
                    (false, Change::Added(items)) => {
                        format!("Okay, took {} off again.", items.join(", ").to_lowercase())
                    }
                    (true, Change::Added(items)) => format!("Ok, fjernet {} igjen.", items.join(", ").to_lowercase()),
                    (false, Change::Removed(items)) => {
                        format!("Okay, {} is back on the list.", items.join(", ").to_lowercase())
                    }
                    (true, Change::Removed(items)) => {
                        format!("Ok, {} er tilbake på lista.", items.join(", ").to_lowercase())
                    }
                });
            }
            return answers.join(" ");
        }
        match self.recent_lights() {
            Some((target, LightChange::Switched(on))) => self.switch(&target, !on, lang, true),
            Some((target, LightChange::Level)) => self.restore_level(&target, lang),
            None if no => "Det er ingenting å angre.".into(),
            None => "There's nothing to undo.".into(),
        }
    }

    /// The lights back as they were before a brightness or colour change.
    fn restore_level(&mut self, target: &Target, lang: Lang) -> String {
        let no = lang == Lang::Norwegian;
        let Some(ha) = &self.home_assistant else { return "Home Assistant isn't connected yet.".into() };
        let scene = serde_json::json!({ "entity_id": format!("scene.{UNDO_SCENE}") });
        if let Err(error) = ha.send("scene", "turn_on", scene) {
            return unreachable(&error, no);
        }
        info!(label = target.label, ids = ?target.ids, "light level undone");
        // The look from before the change is gone now: a second "undo" has nothing to go back to.
        self.last_lights = None;
        let label = target.label.to_lowercase();
        match (no, target.area) {
            (false, true) => format!("Okay, the {label} lights are back as they were."),
            (false, false) => format!("Okay, the {label} is back as it was."),
            (true, true) => format!("Ok, lyset i {label} er som før."),
            (true, false) => format!("Ok, {label} er som før."),
        }
    }

    /// "Which lights are on?": the rooms with lights on; with a room or light named, yes or no.
    pub(super) fn lights_status(&mut self, request: &str, lang: Lang) -> String {
        let no = lang == Lang::Norwegian;
        let Some(ha) = &mut self.home_assistant else {
            return if no { "Home Assistant er ikke koblet til ennå." } else { "Home Assistant isn't connected yet." }
                .into();
        };
        let on = match ha.lights_on() {
            Ok(on) => on,
            Err(error) => return unreachable(&error, no),
        };
        let question: String = request
            .split_whitespace()
            .filter(|w| {
                !["anything", "everything", "something", "left", "still", "noe", "alt", "fortsatt"]
                    .contains(&voice_assistant::dialog::normalize(w).as_str())
            })
            .collect::<Vec<_>>()
            .join(" ");
        if ha::names_something(&question) && !mentions_this_room(request) {
            if let Ok(Some(target)) = ha.find(&question) {
                let lit = on.iter().any(|l| target.ids.contains(&l.id));
                let words: Vec<String> = request.split_whitespace().map(voice_assistant::dialog::normalize).collect();
                return named_light_answer(&target.label.to_lowercase(), target.area, lit, says_off(&words), no);
            }
        }
        // Network gear and the Voice PE report their status LEDs as lights; asked by name, they
        // answer for themselves, but nobody counts them among the lights left on.
        let on: Vec<_> = on.into_iter().filter(|l| !l.status_led()).collect();
        let places = house::rooms_lit(&on);
        let list = house::join(&places, lang);
        match (no, places.is_empty()) {
            (false, true) => "All the lights are off.".into(),
            (true, true) => "Alt lyset er av.".into(),
            (false, false) => format!("Lights are on in the {list}."),
            (true, false) => format!("Lyset er på i {list}."),
        }
    }

    pub(super) fn recent_lights(&self) -> Option<(Target, LightChange)> {
        self.last_lights
            .as_ref()
            .filter(|(.., at)| at.elapsed() < MEMORY)
            .map(|(target, change, _)| (target.clone(), *change))
    }

    pub(super) fn switch(&mut self, target: &Target, on: bool, lang: Lang, undo: bool) -> String {
        let no = lang == Lang::Norwegian;
        let Some(ha) = &self.home_assistant else {
            return "Home Assistant isn't connected yet.".into();
        };
        if let Err(error) = ha.set(target, on) {
            return unreachable(&error, no);
        }
        info!(label = target.label, ids = ?target.ids, on, undo, "lights switched");
        self.last_lights = Some((target.clone(), LightChange::Switched(on), Instant::now()));
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
}
