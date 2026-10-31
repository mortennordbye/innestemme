//! Skills answered by the worker itself: weather, people, briefing, distance, party, scenes, scripts.

use std::time::Duration;

use tracing::{info, warn};
use voice_assistant::ha::{self, HomeAssistant};
use voice_assistant::house;
use voice_assistant::intent::Day;
use voice_assistant::lang::Lang;
use voice_assistant::party::{Pace, PartyCommand};
use voice_assistant::shopping::ListCommand;

use crate::party;

use super::screen::SCREEN_MOMENT;
use super::{time_of_day, unreachable, Worker};

impl Worker {
    /// "Party mode": the Hue lights follow the music, after a snapshot to put them back from.
    pub(super) fn party(&mut self, command: PartyCommand, lang: Lang) -> String {
        let no = lang == Lang::Norwegian;
        let PartyCommand::On(pace) = command else {
            return match self.party_off() {
                true if no => "Ok, festen er over.",
                true => "Okay, the party's over.",
                false if no => "Det er ingen fest på gang.",
                false => "There's no party on.",
            }
            .into();
        };
        if !self.config.party.configured() {
            return if no {
                "Festmodus trenger en Hue-bro. Sett hue-bridge i innstillingene."
            } else {
                "Party mode needs a Hue bridge. Set hue-bridge in the settings."
            }
            .into();
        }
        if let Some(ha) = &self.home_assistant {
            if let Err(error) = house::party_snapshot(ha) {
                warn!(error = format!("{error:#}"), "party mode: no snapshot of the lights");
            }
        }
        let mode = match pace {
            Pace::Relaxed => party::Mode::Relaxed,
            Pace::Normal => party::Mode::Normal,
            Pace::Fast => party::Mode::Fast,
        };
        if let Err(error) = self.config.party.start(mode) {
            warn!(error = format!("{error:#}"), "party mode did not start");
            return if no {
                "Beklager, jeg fikk ikke startet festmodus."
            } else {
                "Sorry, I couldn't start party mode."
            }
            .into();
        }
        match (pace, no) {
            (Pace::Relaxed, false) => "Okay, relaxed party mode is on.",
            (Pace::Normal, false) => "Okay, party mode is on.",
            (Pace::Fast, false) => "Okay, fast party mode is on.",
            (Pace::Relaxed, true) => "Ok, rolig festmodus er på.",
            (Pace::Normal, true) => "Ok, festmodus er på.",
            (Pace::Fast, true) => "Ok, rask festmodus er på.",
        }
        .into()
    }

    /// Ends party mode and puts the lights back once the stream has let go of them. Whether it was on.
    pub(super) fn party_off(&mut self) -> bool {
        if self.config.party.mode().is_none() {
            return false;
        }
        self.config.party.stop();
        if let Some((url, token)) = self.config.home_assistant.clone() {
            // On a thread of its own: the bridge lets go of the lights after the stream's last frame.
            let restore = std::thread::Builder::new().name("party-restore".into()).spawn(move || {
                std::thread::sleep(Duration::from_secs(1));
                if let Err(error) = house::party_restore(&HomeAssistant::new(&url, &token)) {
                    warn!(error = format!("{error:#}"), "party mode: could not put the lights back");
                }
            });
            if let Err(error) = restore {
                warn!(%error, "party mode: could not put the lights back");
            }
        }
        true
    }

    /// A named place, else the home: the address when it is set, else the `home` place name.
    pub(super) fn weather(&mut self, place: Option<String>, day: Day, lang: Lang) -> String {
        let no = lang == Lang::Norwegian;
        let named = place.or_else(|| self.home_location.is_none().then(|| self.config.home.clone()).flatten());
        let location = match named {
            None => match &self.home_location {
                Some(home) => Ok(Some(home.clone())),
                None if no => return "Hvilket sted? Si for eksempel været i Oslo.".into(),
                None => return "Which place? Say, for example, the weather in London.".into(),
            },
            Some(name) => self.weather.find(&name),
        };
        let report = location.and_then(|location| match location {
            Some(location) => self.weather.report(&location, day, lang).map(Some),
            None => Ok(None),
        });
        match report {
            Ok(Some(report)) => report,
            Ok(None) if no => "Beklager, jeg fant ikke det stedet.".into(),
            Ok(None) => "Sorry, I couldn't find that place.".into(),
            Err(error) => {
                warn!(error = format!("{error:#}"), "weather lookup failed");
                if no {
                    "Beklager, jeg får ikke kontakt med værtjenesten.".into()
                } else {
                    "Sorry, I could not reach the weather service.".into()
                }
            }
        }
    }

    /// "Who's home?": everyone and where they are; "is Ingrid home?": that person.
    pub(super) fn whos_home(&mut self, name: Option<&str>, lang: Lang) -> String {
        let no = lang == Lang::Norwegian;
        let Some(ha) = &self.home_assistant else {
            return if no { "Home Assistant er ikke koblet til ennå." } else { "Home Assistant isn't connected yet." }
                .into();
        };
        let people = match ha.people() {
            Ok(people) => people,
            Err(error) => return unreachable(&error, no),
        };
        let first_name = |p: &ha::Person| p.name.split_whitespace().next().unwrap_or(&p.name).to_owned();
        let place = |p: &ha::Person| match (no, p.state.as_str()) {
            (false, "home") => "home".to_owned(),
            (true, "home") => "hjemme".to_owned(),
            (false, "not_home") => "out".to_owned(),
            (true, "not_home") => "ute".to_owned(),
            (false, "unknown" | "unavailable") => "somewhere I can't tell".to_owned(),
            (true, "unknown" | "unavailable") => "et sted jeg ikke vet".to_owned(),
            (false, zone) => format!("at {zone}"),
            (true, zone) => format!("på {zone}"),
        };
        if let Some(name) = name {
            let names = voice_assistant::names::Names::new(people.iter().map(first_name));
            let Some(person) = names.best(name).and_then(|found| people.iter().find(|p| first_name(p) == found.name))
            else {
                return if no { format!("Jeg vet ikke hvor {name} er.") } else { format!("I don't track {name}.") };
            };
            let who = first_name(person);
            return match (no, person.state == "home") {
                (false, true) => format!("Yes, {who} is home."),
                (true, true) => format!("Ja, {who} er hjemme."),
                (false, false) => format!("No, {who} is {}.", place(person)),
                (true, false) => format!("Nei, {who} er {}.", place(person)),
            };
        }
        let home: Vec<String> = people.iter().filter(|p| p.state == "home").map(first_name).collect();
        let and = if no { "og" } else { "and" };
        let join = |names: &[String]| match names {
            [] => String::new(),
            [one] => one.clone(),
            [rest @ .., last] => format!("{} {and} {last}", rest.join(", ")),
        };
        let is = if no { "er" } else { "is" };
        let mut clauses: Vec<String> = Vec::new();
        match (no, home.len()) {
            (_, 0) => {}
            (false, n) => clauses.push(format!("{} {} home", join(&home), if n == 1 { "is" } else { "are" })),
            (true, _) => clauses.push(format!("{} er hjemme", join(&home))),
        }
        clauses.extend(
            people.iter().filter(|p| p.state != "home").map(|p| format!("{} {is} {}", first_name(p), place(p))),
        );
        match (no, home.is_empty()) {
            (false, true) if clauses.is_empty() => "Home Assistant tracks nobody.".into(),
            (true, true) if clauses.is_empty() => "Home Assistant følger ingen.".into(),
            (false, true) => format!("Nobody is home; {}.", clauses.join(", ")),
            (true, true) => format!("Ingen er hjemme; {}.", clauses.join(", ")),
            _ => format!("{}.", clauses.join("; ")),
        }
    }

    /// "Good morning": greeting and time, the weather at home, the shopping list, power prices.
    pub(super) fn briefing(&mut self, lang: Lang) -> String {
        let no = lang == Lang::Norwegian;
        let now = jiff::Zoned::now();
        let greeting = match (no, now.hour()) {
            (false, 4..=11) => "Good morning",
            (false, 12..=17) => "Good afternoon",
            (false, _) => "Good evening",
            (true, 4..=9) => "God morgen",
            (true, 10..=17) => "Hei",
            (true, _) => "God kveld",
        };
        let mut parts = vec![time_of_day(lang).replacen("It's", &format!("{greeting}! It's"), 1)];
        if no {
            parts[0] = format!("{greeting}! {}", time_of_day(lang));
        }
        if self.home_location.is_some() || self.config.home.is_some() {
            parts.push(self.weather(None, Day::Today, lang));
        }
        if let Some(ha) = &self.home_assistant {
            if let Ok((answer, _)) = self.shopping.run(ha, &ListCommand::Read, lang) {
                // Only a list with something on it is news.
                if !answer.contains("empty") && !answer.contains("tom") {
                    parts.push(answer);
                }
            }
        }
        match self.news.briefing(lang) {
            Ok(headlines) => {
                self.live.extend(headlines.live);
                parts.push(headlines.text);
            }
            Err(error) => warn!(error = format!("{error:#}"), "no news for the briefing"),
        }
        parts.join(" ")
    }

    pub(super) fn distance(&mut self, query: &voice_assistant::distance::Query, lang: Lang) -> String {
        let no = lang == Lang::Norwegian;
        let from = match &query.from {
            Some(name) => self.weather.find(name),
            None => Ok(self.home_location.clone()),
        };
        let places = from.and_then(|from| Ok((from, self.weather.find(&query.to)?)));
        match places {
            Ok((Some(from), Some(to))) => voice_assistant::distance::answer(&from, &to, lang),
            Ok((None, _)) if query.from.is_none() && no => "Hvor fra? Si for eksempel fra Oslo til Bergen.".into(),
            Ok((None, _)) if query.from.is_none() => "From where? Say, for example, from Oslo to Bergen.".into(),
            Ok(_) if no => "Beklager, jeg fant ikke det stedet.".into(),
            Ok(_) => "Sorry, I couldn't find that place.".into(),
            Err(error) => {
                warn!(error = format!("{error:#}"), "place lookup failed");
                if no {
                    "Beklager, jeg får ikke slått opp stedet nå."
                } else {
                    "Sorry, I couldn't look up the place right now."
                }
                .into()
            }
        }
    }

    pub(super) fn scene(&mut self, request: &str, lang: Lang) -> String {
        let no = lang == Lang::Norwegian;
        let here = self.room.clone();
        let Some(ha) = &mut self.home_assistant else {
            return if no { "Home Assistant er ikke koblet til ennå." } else { "Home Assistant isn't connected yet." }
                .into();
        };
        let scenes = match ha.scenes() {
            Ok(scenes) => scenes.to_vec(),
            Err(error) => return unreachable(&error, no),
        };
        let scene = match ha::resolve_scene(&scenes, request, here.as_deref()) {
            ha::SceneMatch::Found(scene) => scene.clone(),
            ha::SceneMatch::WhichRoom => {
                self.pending_scene = Some(request.to_owned());
                return if no { "Hvilket rom?" } else { "Which room?" }.into();
            }
            ha::SceneMatch::NotFound if no => return "Jeg fant ikke den scenen.".into(),
            ha::SceneMatch::NotFound => return "I couldn't find that scene.".into(),
        };
        if let Err(error) = ha.activate(&scene) {
            return unreachable(&error, no);
        }
        info!(scene = scene.id, request, "scene activated");
        self.show("scene", serde_json::json!({ "name": scene.name }), Some(SCREEN_MOMENT));
        let name = scene.name.to_lowercase();
        if no {
            format!("Ok, {name}.")
        } else {
            format!("Okay, {name}.")
        }
    }

    pub(super) fn script(&mut self, request: &str, lang: Lang) -> String {
        let no = lang == Lang::Norwegian;
        let Some(ha) = &mut self.home_assistant else {
            return if no { "Home Assistant er ikke koblet til ennå." } else { "Home Assistant isn't connected yet." }
                .into();
        };
        let script = match ha.scripts() {
            Ok(scripts) => ha::resolve_script(scripts, request).cloned(),
            Err(error) => return unreachable(&error, no),
        };
        let Some(script) = script else {
            return if no { "Jeg fant ikke det skriptet." } else { "I couldn't find that script." }.into();
        };
        if let Err(error) = ha.activate(&script) {
            return unreachable(&error, no);
        }
        info!(script = script.id, request, "script started");
        let name = script.name.to_lowercase();
        if no {
            format!("Ok, kjører {name}.")
        } else {
            format!("Okay, running {name}.")
        }
    }

    /// A script or scene whose whole name is the request; `None` when there is none.
    pub(super) fn by_exact_name(&mut self, request: &str, lang: Lang) -> Option<String> {
        let ha = self.home_assistant.as_mut()?;
        let script = ha.scripts().ok().and_then(|scripts| ha::exact(scripts, request).cloned());
        if script.is_some() {
            return Some(self.script(request, lang));
        }
        let ha = self.home_assistant.as_mut()?;
        let scene = ha.scenes().ok().and_then(|scenes| ha::exact(scenes, request).cloned())?;
        Some(self.scene(&scene.name, lang))
    }
}
