//! Lead-in sentences for slow answers.

use voice_assistant::intent::{self, Intent};
use voice_assistant::lang::Lang;
use voice_assistant::persona::Persona;

use super::Worker;

pub(super) use voice_assistant::intent::LEADS;

pub(super) use voice_assistant::say::capitalize;

/// "Checking the forecast, sir." or "Checking the forecast."
pub(super) fn lead_sentence(lead: &str, honorific: Option<&str>) -> String {
    match honorific {
        Some(h) if !h.is_empty() => format!("{lead}, {h}."),
        _ => format!("{lead}."),
    }
}

impl Worker {
    /// The lead-in for a request whose answer is slow to get or to say, when speech is rendered
    /// ahead of time. Not when a question asked back takes the words ("Which room?" "The kitchen.").
    pub(super) fn lead_in(&mut self, text: &str, lang: Lang) -> Option<String> {
        if lang != Lang::English || self.render.is_none() || self.awaiting().is_some() {
            return None;
        }
        // Several requests: the first one's lead-in.
        let first = intent::split_commands(text).and_then(|parts| parts.into_iter().next());
        // A lead-in plays before the answer, so with the data kept it only delays it.
        if self.data_kept(text, lang) {
            return None;
        }
        let intent = intent::parse(first.as_deref().unwrap_or(text));
        if intent == Intent::Unknown && self.config.llm.is_none() {
            return None;
        }
        // "What's therapy?" asks which was meant: "Let me look that up" would answer the question
        // before it is asked.
        if first.is_none()
            && matches!(intent, Intent::Lookup(_))
            && voice_assistant::sound_alike::sounds_like_skill(text).is_some()
        {
            return None;
        }
        let leads = intent.meta().lead?;
        self.lead_turn = self.lead_turn.wrapping_add(1);
        Some(lead_sentence(leads[self.lead_turn % leads.len()], self.persona.as_ref().map(Persona::honorific)))
    }
}
