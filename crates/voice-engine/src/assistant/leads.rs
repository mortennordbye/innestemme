//! Lead-in sentences for slow answers.

use voice_assistant::house::HouseCommand;
use voice_assistant::intent::{self, Intent};
use voice_assistant::lang::Lang;
use voice_assistant::persona::Persona;
use voice_assistant::tv::TvCommand;

use super::Worker;

const LEADS_WEATHER: &[&str] = &["Checking the forecast", "Let me look at the weather", "Consulting the skies"];
const LEADS_TRANSIT: &[&str] =
    &["Checking the departures", "Let me look up the next departures", "Checking the timetable"];
const LEADS_POWER: &[&str] =
    &["Checking the electricity prices", "Let me look at the power prices", "Checking the price of power"];
const LEADS_LIGHTS: &[&str] = &["Let me check the lights", "Checking the lights", "Looking around the house"];
const LEADS_HOME: &[&str] = &["Let me see who's in", "Checking who's home", "Let me take a head count"];
const LEADS_THINK: &[&str] = &["Let me think", "Give me a moment", "Let me consider that"];
const LEADS_NEWS: &[&str] = &["Fetching the headlines", "Let me check the news", "Checking the papers"];
const LEADS_LOOKUP: &[&str] = &["Let me look that up", "One moment", "Let me find out"];
const LEADS_HOUSE: &[&str] = &["Let me check the house", "Checking the house", "Doing the rounds"];
const LEADS_TV: &[&str] = &["Let me find it", "Looking it up", "Finding it for you"];
pub(super) const LEADS: &[&[&str]] = &[
    LEADS_WEATHER,
    LEADS_TRANSIT,
    LEADS_POWER,
    LEADS_LIGHTS,
    LEADS_HOME,
    LEADS_THINK,
    LEADS_NEWS,
    LEADS_LOOKUP,
    LEADS_HOUSE,
    LEADS_TV,
];

/// "Checking the forecast, sir." or "Checking the forecast."
pub(super) fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map_or_else(String::new, |c| c.to_uppercase().chain(chars).collect())
}

pub(super) fn lead_sentence(lead: &str, honorific: Option<&str>) -> String {
    match honorific {
        Some(h) if !h.is_empty() => format!("{lead}, {h}."),
        _ => format!("{lead}."),
    }
}

impl Worker {
    /// The lead-in for a request whose answer is slow to get or to say, when speech is rendered
    /// ahead of time. Not when a pending question takes the words ("Which room?" "The kitchen.").
    pub(super) fn lead_in(&mut self, text: &str, lang: Lang) -> Option<String> {
        let pending = self.pending_lights.is_some()
            || self.list_is_open()
            || self.pending_scene.is_some()
            || self.pending_confirm.is_some()
            || self.timers.awaiting_duration();
        if lang != Lang::English || self.render.is_none() || pending {
            return None;
        }
        // Several requests: the first one's lead-in.
        let first = intent::split_commands(text).and_then(|parts| parts.into_iter().next());
        let leads = match intent::parse(first.as_deref().unwrap_or(text)) {
            // A lead-in plays before the answer, so with the data kept it only delays it.
            _ if self.data_kept(text, lang) => return None,
            Intent::Weather { .. } => LEADS_WEATHER,
            Intent::Transit(_) => LEADS_TRANSIT,
            Intent::Power(_) => LEADS_POWER,
            Intent::News => LEADS_NEWS,
            Intent::Lookup(_) => LEADS_LOOKUP,
            Intent::LightsStatus { .. } => LEADS_LIGHTS,
            Intent::WhosHome(_) => LEADS_HOME,
            Intent::House(HouseCommand::Status) => LEADS_HOUSE,
            Intent::Tv(TvCommand::Nrk(_) | TvCommand::Plex(_)) => LEADS_TV,
            Intent::Unknown if self.config.llm.is_some() => LEADS_THINK,
            _ => return None,
        };
        self.lead_turn = self.lead_turn.wrapping_add(1);
        Some(lead_sentence(leads[self.lead_turn % leads.len()], self.persona.as_ref().map(Persona::honorific)))
    }
}
