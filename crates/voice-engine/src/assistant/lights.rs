//! The lights the conversation refers to: "them", "undo" and the question "Which room?". The
//! skill itself, its Home Assistant calls and its answers are in [`voice_assistant::lights`].

use std::time::{Duration, Instant};

use tracing::info;
use voice_assistant::ha::Target;
use voice_assistant::intent::LightLevel;
use voice_assistant::lang::Lang;
use voice_assistant::lights::{self, Which};
use voice_assistant::shopping::Change;
use voice_assistant::skill::LightChange;

use super::{not_connected, unreachable, Worker};

/// "Them" and "that" refer to the last lights for this long.
const MEMORY: Duration = Duration::from_secs(120);

/// Whether a light change made at `at` can still be undone: not when a scene, party mode or a
/// lockdown has changed the lights since (`gone`).
fn undoable(at: Instant, gone: Option<Instant>) -> bool {
    gone.is_none_or(|gone| at > gone)
}

impl Worker {
    pub(super) fn lights(&mut self, request: &str, on: bool, lang: Lang) -> String {
        let (recent, room) = (self.recent_target(), self.room.clone());
        self.home_skill(lang, |ha| lights::turn(ha, request, on, room.as_deref(), recent, lang))
    }

    /// The lights a request means: the ones it names; "in here" and no name at all mean this room;
    /// "them" the last lights. `Ok(None)`: no idea which room. `Err` is the answer to speak.
    pub(super) fn light_target(&mut self, request: &str, lang: Lang) -> Result<Option<Target>, String> {
        let no = lang == Lang::Norwegian;
        let recent = self.recent_target();
        let Some(ha) = &mut self.home_assistant else {
            return Err(not_connected(no));
        };
        match lights::target(ha, request, self.room.as_deref(), recent, lang) {
            Ok(Which::Lights(target)) => Ok(Some(target)),
            Ok(Which::Unknown) => Ok(None),
            Ok(Which::NotFound) => Err(lights::not_found(lang)),
            Err(error) => Err(unreachable(&error, no)),
        }
    }

    pub(super) fn light_level(&mut self, request: &str, level: &LightLevel, lang: Lang) -> String {
        let (recent, room) = (self.recent_target(), self.room.clone());
        self.home_skill(lang, |ha| lights::level(ha, request, level, room.as_deref(), recent, lang))
    }

    /// "Undo": whichever changed last, the lights or the shopping list.
    pub(super) fn undo(&mut self, lang: Lang) -> String {
        let no = lang == Lang::Norwegian;
        let gone = self.light_undo_gone;
        let lights_at = self
            .last_lights
            .as_ref()
            .filter(|(.., at)| at.elapsed() < MEMORY && undoable(*at, gone))
            .map(|(.., at)| *at);
        let list_at = self.last_list.as_ref().filter(|(_, at)| at.elapsed() < MEMORY).map(|(_, at)| *at);
        if list_at.is_some() && (lights_at.is_none() || list_at > lights_at) {
            let (changes, _) = self.last_list.take().expect("checked above");
            let Some(ha) = &self.home_assistant else { return not_connected(no) };
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
        match self.recent_lights().filter(|_| lights_at.is_some()) {
            Some((target, LightChange::Switched(on))) => {
                self.home_skill(lang, |ha| lights::switch(ha, &target, !on, lang, true))
            }
            Some((target, LightChange::Level)) => self.home_skill(lang, |ha| lights::restore_level(ha, &target, lang)),
            None => lights::nothing_to_undo(lang),
        }
    }

    /// "Which lights are on?": the rooms with lights on; with a room or light named, yes or no.
    pub(super) fn lights_status(&mut self, request: &str, lang: Lang) -> String {
        let no = lang == Lang::Norwegian;
        let Some(ha) = &mut self.home_assistant else {
            return not_connected(no);
        };
        lights::status(ha, request, lang).unwrap_or_else(|error| unreachable(&error, no))
    }

    /// A scene, party mode or a lockdown changed the lights: "undo" has nothing to go back to,
    /// though "them" still means the lights last named.
    pub(super) fn forget_light_undo(&mut self) {
        self.light_undo_gone = Some(Instant::now());
    }

    pub(super) fn recent_lights(&self) -> Option<(Target, LightChange)> {
        self.last_lights
            .as_ref()
            .filter(|(.., at)| at.elapsed() < MEMORY)
            .map(|(target, change, _)| (target.clone(), *change))
    }

    fn recent_target(&self) -> Option<Target> {
        self.recent_lights().map(|(target, _)| target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_scene_since_a_change_leaves_nothing_to_undo() {
        let dimmed = Instant::now();
        assert!(undoable(dimmed, None));
        let scene = dimmed + Duration::from_millis(5);
        assert!(!undoable(dimmed, Some(scene)));
        // A change after the scene can be undone again.
        assert!(undoable(scene + Duration::from_millis(5), Some(scene)));
    }
}
