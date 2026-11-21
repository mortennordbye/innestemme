//! Home Assistant skills run for the conversation: applying what a skill did, the house, routines
//! and safety alerts. The skills themselves are in `voice_assistant` (`house`, `lights`, `scenes`).

use std::sync::mpsc;
use std::time::{Duration, Instant};

use tracing::{info, warn};
use voice_assistant::alerts::{self, Watch};
use voice_assistant::ha::HomeAssistant;
use voice_assistant::house::{self, HouseCommand, Routine, RoutineSettings};
use voice_assistant::intent::MusicCommand;
use voice_assistant::lang::Lang;
use voice_assistant::skill::{Ask, Done, Lights};

use super::screen::SCREEN_MOMENT;
use super::{not_connected, unreachable, Job, Worker};

/// How often Home Assistant's safety sensors are checked.
const ALERT_POLL: Duration = Duration::from_secs(5);

/// Checks the safety sensors every few seconds and hands new warnings to the worker, which speaks
/// them even while listening is paused.
pub(super) fn watch_alerts(ha: HomeAssistant, jobs: mpsc::Sender<Job>) {
    let mut watch = Watch::default();
    let mut failing = false;
    loop {
        std::thread::sleep(ALERT_POLL);
        match alerts::active(&ha) {
            Ok(active) => {
                failing = false;
                for alert in watch.due(&active, Instant::now()) {
                    info!(id = alert.id, "safety sensor on");
                    if jobs.send(Job::Alert(alerts::sentence(alert, Lang::English))).is_err() {
                        return;
                    }
                }
            }
            // Once per outage: Home Assistant restarts now and then.
            Err(error) if !failing => {
                failing = true;
                warn!(error = format!("{error:#}"), "could not check the safety sensors");
            }
            Err(_) => {}
        }
    }
}

/// "Yes" or "no" to a question, if the reply is one.
pub(super) fn confirmation(text: &str) -> Option<bool> {
    const YES: &[&str] = &[
        "yes", "yeah", "yep", "yup", "sure", "ok", "okay", "please", "correct", "do", "it", "ja", "jepp", "gjerne",
        "takk", "sir",
    ];
    const NO: &[&str] =
        &["no", "nope", "nah", "don't", "dont", "not", "cancel", "never", "mind", "nei", "ikke", "la", "være"];
    let words = voice_assistant::dialog::words(text);
    let all = |list: &[&str]| !words.is_empty() && words.len() <= 4 && words.iter().all(|w| list.contains(&w.as_str()));
    let first = |list: &[&str]| words.first().is_some_and(|w| list[..4].contains(&w.as_str()));
    if all(NO) || first(NO) {
        Some(false)
    } else if all(YES) {
        Some(true)
    } else {
        None
    }
}

impl Worker {
    /// Runs a Home Assistant skill and applies what it did to the conversation; the answer to
    /// speak, or why there is none.
    pub(super) fn home_skill(
        &mut self,
        lang: Lang,
        skill: impl FnOnce(&mut HomeAssistant) -> anyhow::Result<Done>,
    ) -> String {
        let no = lang == Lang::Norwegian;
        let Some(ha) = &mut self.home_assistant else {
            return not_connected(no);
        };
        match skill(ha) {
            Ok(done) => self.apply(done),
            Err(error) => unreachable(&error, no),
        }
    }

    /// What a skill's answer asks of the conversation: the last lights, a question asked back, a
    /// screen for a moment. The answer.
    pub(super) fn apply(&mut self, done: Done) -> String {
        match done.lights {
            Lights::Same => {}
            Lights::Changed(target, change) => self.last_lights = Some((target, change, Instant::now())),
            Lights::Undone => self.last_lights = None,
            Lights::UndoGone => self.forget_light_undo(),
        }
        match done.ask {
            Some(Ask::LightsRoom(on)) => self.conversation.pending_lights = Some((on, Instant::now())),
            Some(Ask::SceneRoom(request)) => self.conversation.pending_scene = Some((request, Instant::now())),
            None => {}
        }
        if let Some((module, data)) = done.moment {
            self.show(module, data, Some(SCREEN_MOMENT));
        }
        done.answer
    }

    pub(super) fn house(&mut self, command: &HouseCommand, lang: Lang) -> String {
        let name = self.config.wake.name().to_owned();
        let here = self.room.clone();
        self.home_skill(lang, |ha| {
            let answer = house::answer(ha, command, here.as_deref(), &name, lang)?;
            info!(?command, "house");
            Ok(Done::say(answer))
        })
    }

    /// Leaving, coming home and bedtime; coming home may also start the music.
    pub(super) fn routine(&mut self, routine: Routine, lang: Lang) -> String {
        let no = lang == Lang::Norwegian;
        let here = self.room.clone();
        let Some(ha) = &mut self.home_assistant else {
            return not_connected(no);
        };
        let settings =
            RoutineSettings { home_room: self.config.home_room.as_deref(), bedtime_light: self.config.bedtime_light };
        let mut answer = match house::run_routine(ha, routine, here.as_deref(), settings, lang) {
            Ok(done) => self.apply(done),
            Err(error) => return unreachable(&error, no),
        };
        if let (Routine::Home, Some(music)) = (routine, &self.config.home_music) {
            let command = if music.eq_ignore_ascii_case("liked") {
                MusicCommand::Liked
            } else {
                MusicCommand::Playlist(music.clone())
            };
            let started = self.player.run(self.home_assistant.as_ref(), &command, lang);
            answer = format!("{answer} {started}");
        }
        info!(?routine, answer, "routine");
        if routine == Routine::Home {
            self.show("welcome", serde_json::json!({ "text": answer }), Some(SCREEN_MOMENT));
        }
        answer
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yes_and_no_replies() {
        assert_eq!(confirmation("Yes."), Some(true));
        assert_eq!(confirmation("Yes, please do it."), Some(true));
        assert_eq!(confirmation("Ja takk"), Some(true));
        assert_eq!(confirmation("No."), Some(false));
        assert_eq!(confirmation("No, leave it."), Some(false));
        assert_eq!(confirmation("Nei"), Some(false));
        assert_eq!(confirmation("What time is it?"), None);
        assert_eq!(confirmation("Do you love me?"), None);
    }
}
