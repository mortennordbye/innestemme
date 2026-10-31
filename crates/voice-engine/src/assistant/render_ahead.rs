//! Sentences rendered ahead of time for the speech cache, and data fetched before anyone asks.

use std::time::Instant;

use tracing::warn;
use voice_assistant::house::{self, HouseCommand, Switch};
use voice_assistant::intent::{self, Intent};
use voice_assistant::jokes::Jokes;
use voice_assistant::lang::Lang;
use voice_assistant::persona::Persona;
use voice_assistant::timer;

use crate::render::Priority;

use super::home::confirm_question;
use super::leads::{lead_sentence, LEADS};
use super::Worker;

/// Questions answered from the forecast and prices fetched ahead, rendered after each refresh.
const DATA_QUESTIONS: &[&str] = &[
    "What's the weather?",
    "What's the weather tomorrow?",
    "What's the electricity price now?",
    "When is electricity cheapest today?",
    "When is electricity cheapest tomorrow?",
];

impl Worker {
    /// Whether a weather or electricity price request is answered from data fetched ahead.
    pub(super) fn data_kept(&self, text: &str, lang: Lang) -> bool {
        let first = intent::split_commands(text).and_then(|parts| parts.into_iter().next());
        lang == Lang::English
            && match intent::parse(first.as_deref().unwrap_or(text)) {
                Intent::Weather { place: None, .. } => {
                    self.home_location.as_ref().is_some_and(|home| self.weather.is_fresh(home))
                }
                Intent::Power(query) => self.power.as_ref().is_some_and(|p| p.is_ready(query)),
                _ => false,
            }
    }

    /// Every fixed English sentence the assistant says, as spoken (a sentence at a time): what is
    /// said most, then the long tail.
    pub(super) fn fixed_sentences(&self) -> (Vec<String>, Vec<String>) {
        let honorifics: Vec<Option<&str>> = match &self.persona {
            // Plain as well: a voice the persona is unsure of hears the lead-in without one.
            Some(persona) => persona.honorifics().iter().map(|h| Some(h.as_str())).chain([None]).collect(),
            None => vec![None],
        };
        let mut texts: Vec<String> = LEADS
            .iter()
            .flat_map(|leads| leads.iter())
            .flat_map(|l| honorifics.iter().map(|h| lead_sentence(l, *h)))
            .collect();
        texts.extend(self.persona.as_ref().map(Persona::phrases).unwrap_or_default());
        let questions =
            [HouseCommand::Camera(Switch::Off), HouseCommand::Camera(Switch::On), HouseCommand::Alarm(Switch::On)]
                .map(|c| confirm_question(&c, Lang::English).unwrap_or_default());
        let answers = voice_assistant::music::ENGLISH_ANSWERS
            .iter()
            .chain(&questions)
            .chain(&["Okay, I'll leave it.", "Anything else?", "Okay, that's everything."])
            .chain(voice_assistant::tv::ENGLISH_ANSWERS)
            .chain(&[
                "Okay, party mode is on.",
                "Okay, fast party mode is on.",
                "Okay, relaxed party mode is on.",
                "Okay, the party's over.",
            ])
            .chain(house::LOCKDOWN_ANSWERS)
            .chain(house::LOCKDOWN_THREATS)
            .chain(house::ROUTINE_ANSWERS)
            .chain(Jokes::english())
            .chain(&["Sorry, I didn't catch that."])
            .map(|a| a.to_string())
            .chain(voice_assistant::smalltalk::all_english(self.config.wake.name()));
        match &self.persona {
            Some(persona) => texts.extend(answers.flat_map(|a| persona.forms(&a))),
            None => texts.extend(answers),
        }
        let mut sentences: Vec<String> = texts
            .iter()
            .flat_map(|t| {
                t.split_inclusive(['.', '?', '!']).map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned)
            })
            .filter(|s| voice_assistant::speech_cache::CachedTts::cacheable(s))
            .collect();
        sentences.sort();
        sentences.dedup();
        // Then the answers with numbers that come up most, which the cache keeps too.
        let mut numbered: Vec<String> = (0..=100).map(|n| format!("Volume {n} percent.")).collect();
        for minutes in [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 12, 15, 20, 25, 30, 40, 45, 60, 90, 120] {
            numbered.push(format!("Timer set for {}.", timer::say_duration(minutes * 60, Lang::English)));
            if minutes < 60 {
                numbered.push(format!("The {minutes} minute timer is cancelled."));
            }
        }
        for seconds in [10, 15, 20, 30, 45, 90] {
            numbered.push(format!("Timer set for {}.", timer::say_duration(seconds, Lang::English)));
        }
        // Every time of day, from now on: the next hours are ready soon after a start, the rest in
        // idle time over a day, and then kept on disk.
        let now = jiff::Zoned::now();
        let minute = u32::from(now.hour() as u8) * 60 + u32::from(now.minute() as u8);
        let mut times: Vec<String> = (0..24 * 60)
            .map(|i| {
                let m = (minute + i) % (24 * 60);
                format!("It's {:02}:{:02}.", m / 60, m % 60)
            })
            .collect();
        let mut later = times.split_off(120);
        numbered.extend(times);
        let styled = |answers: Vec<String>| -> Vec<String> {
            answers
                .into_iter()
                .flat_map(|a| match &self.persona {
                    Some(persona) => persona.forms(&a),
                    None => vec![a],
                })
                .collect()
        };
        sentences.extend(styled(numbered));
        // Answers after a lead-in are spoken as they are, without the persona's opener: the weather,
        // departures and electricity prices, a sentence at a time. Today's own are rendered first,
        // after each refresh (`data_sentences`).
        let mut tail = Vec::new();
        let home = self.home_location.as_ref().map(|l| l.label.clone()).or_else(|| self.config.home.clone());
        if let Some(home) = home {
            tail.extend(voice_assistant::weather::english_sentences(&home));
        }
        tail.extend(voice_assistant::transit::english_sentences());
        tail.extend(voice_assistant::power::english_sentences());
        later.extend(timer::english_left_sentences());
        tail.extend(styled(later));
        let cacheable = voice_assistant::speech_cache::CachedTts::cacheable;
        sentences.retain(|s| cacheable(s));
        tail.retain(|s| cacheable(s));
        (sentences, tail)
    }

    /// Answers about this home's own lights and rooms, which Home Assistant names: the first
    /// "turn off the kitchen lights" would otherwise wait for its sentence to be synthesized.
    pub(super) fn home_sentences(&mut self) -> Vec<String> {
        let Some(ha) = &mut self.home_assistant else { return Vec::new() };
        let states = ["on", "off", "back on", "back off"];
        let mut answers: Vec<String> = [
            "I couldn't find that light.",
            "I couldn't find that scene.",
            "Sorry, I couldn't reach Home Assistant.",
            "Sorry, Home Assistant couldn't do all of that.",
            "Sorry, I couldn't find that place.",
            "Okay, I'm ringing your phone.",
        ]
        .map(String::from)
        .into();
        if let Ok(lights) = ha.lights() {
            let mut areas: Vec<String> =
                lights.iter().map(|l| l.area.to_lowercase()).filter(|a| !a.is_empty()).collect();
            // "Turn off all the lights": every lamp, under the house's name.
            areas.push("house".to_owned());
            areas.sort();
            areas.dedup();
            for area in &areas {
                answers.extend(states.map(|state| format!("Okay, the {area} lights are {state}.")));
            }
            for light in lights.iter().filter(|l| !l.group) {
                let name = light.name.to_lowercase();
                answers.extend(states.map(|state| format!("Okay, the {name} is {state}.")));
            }
        }
        // Indoor temperatures, the ones a home has most of the year.
        if let Ok(rooms) = house::climate_readings(ha) {
            for room in rooms.iter().filter(|r| r.temperature.is_some()) {
                let area = room.area.to_lowercase();
                answers.extend((15..=30).map(|t| format!("It's {t} degrees in the {area}.")));
            }
        }
        self.spoken_sentences(&answers)
    }

    /// The sentences of these answers in every way the persona says them, as kept in the cache.
    fn spoken_sentences(&self, answers: &[String]) -> Vec<String> {
        let mut sentences: Vec<String> = answers
            .iter()
            .flat_map(|a| match &self.persona {
                Some(persona) => persona.forms(a),
                None => vec![a.clone()],
            })
            .flat_map(|t| {
                t.split_inclusive(['.', '?', '!'])
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            })
            .filter(|s| voice_assistant::speech_cache::CachedTts::cacheable(s))
            .collect();
        sentences.sort();
        sentences.dedup();
        sentences
    }

    /// Today's answers from the forecast and prices fetched ahead, to render before anyone asks.
    fn data_sentences(&mut self) -> Vec<String> {
        let kept: Vec<&str> = DATA_QUESTIONS.iter().copied().filter(|q| self.data_kept(q, Lang::English)).collect();
        let answers: Vec<String> = kept.into_iter().filter_map(|q| self.compose(q, Lang::English)).collect();
        self.spoken_sentences(&answers)
    }

    /// The home's forecast and the electricity prices, fetched before anyone asks.
    pub(super) fn refresh_data(&mut self) {
        self.refreshed = Some(Instant::now());
        if let Some(home) = self.home_location.clone() {
            if let Err(error) = self.weather.refresh(&home) {
                warn!(error = format!("{error:#}"), "could not refresh the forecast");
            }
        }
        if let Some(power) = &mut self.power {
            if let Err(error) = power.refresh() {
                warn!(error = format!("{error:#}"), "could not refresh the electricity prices");
            }
        }
        if let Some(render) = self.render.clone() {
            render.add(Priority::Soon, self.data_sentences());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_questions_ask_for_the_data_fetched_ahead() {
        for question in DATA_QUESTIONS {
            let intent = intent::parse(question);
            assert!(matches!(intent, Intent::Weather { place: None, .. } | Intent::Power(_)), "{question}: {intent:?}");
        }
    }
}
