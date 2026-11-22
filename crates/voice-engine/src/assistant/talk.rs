//! Open conversation: what no rule takes goes to the local chat model (`chat-url`), and its answer
//! is spoken a sentence at a time while the model writes the rest. After such an answer the
//! follow-up window takes a reply without the name, so a conversation can go on.

use std::sync::atomic::Ordering::Relaxed;
use std::time::Instant;

use tracing::{info, warn};
use voice_assistant::intent::LEADS_TALK;
use voice_assistant::lang::Lang;
use voice_assistant::talk::{self, TalkContext};

use super::conversation::HISTORY_MAX_AGE;
use super::leads::lead_sentence;
use super::speech::Spoken;
use super::{today, AssistantEvent, Worker};
use crate::unhandled::Reason;

/// The longest reply without the name that goes on with a conversation: longer is the room or the TV.
const TALK_REPLY_WORDS: usize = 12;

impl Worker {
    /// The chat model's answer to `text`, already spoken; `None` when there is no chat model, or
    /// it failed and nothing was spoken, so the rules' fallback answers.
    pub(super) fn converse(&mut self, text: &str, lang: Lang) -> Option<String> {
        let chat = self.config.talk.clone()?;
        let system = self.talk_prompt();
        let history = self.conversation.recent_turns();
        let start = Instant::now();
        let mut spoken = Spoken::default();
        let mut first = None;
        // A typed request (a phone, Assist) gets the words back, not speech.
        let aloud = !self.previewing;
        // A person's first sound while the model reads the request, from the speech cache. Here and
        // not in `lead_in`: only a request that reaches the model gets it.
        if aloud && lang == Lang::English && self.render.is_some() && !self.answer_announced {
            self.lead_turn = self.lead_turn.wrapping_add(1);
            let honorific = self.persona.as_ref().map(|persona| persona.honorific().to_owned());
            let lead = lead_sentence(LEADS_TALK[self.lead_turn % LEADS_TALK.len()], honorific.as_deref());
            self.announce_answer(&lead);
            self.speak(&lead, lang, &mut spoken);
        }
        let result = chat.answer(&system, &history, text, lang, &mut |sentence| {
            first.get_or_insert_with(|| start.elapsed());
            if aloud {
                self.announce_answer(sentence);
                // Each answer is new words: synthesized live, not kept in the speech cache.
                self.live.insert(sentence.to_owned());
                self.speak(sentence, lang, &mut spoken);
            }
            !self.interrupted.load(Relaxed)
        });
        let answer = match result {
            Ok(answer) if !answer.is_empty() => {
                info!(first_sentence = ?first, took = ?start.elapsed(), "chat model answered");
                self.unhandled(Reason::LanguageModel, text, Some(&answer));
                answer
            }
            failed => {
                match failed {
                    Err(error) => warn!(%error, "chat model failed"),
                    Ok(_) => warn!("chat model said nothing"),
                }
                if !spoken.started() {
                    return None;
                }
                // The lead-in went out: the fallback follows it here rather than from the rules.
                let fallback = "Sorry, I can't help with that yet.";
                self.speak(fallback, lang, &mut spoken);
                fallback.to_owned()
            }
        };
        if aloud {
            self.talked = Some((spoken, Instant::now()));
        }
        Some(answer)
    }

    /// The `Answer` event, once per answer and before its audio: a satellite starts its reply on it.
    fn announce_answer(&mut self, text: &str) {
        if !std::mem::replace(&mut self.answer_announced, true) {
            self.emit(AssistantEvent::Answer { text: text.to_owned() });
        }
    }

    /// Words without the name, after an answer of the chat model's, that go on with the
    /// conversation ("Tired. Long day." after "And you?"): short, and not that answer heard back.
    pub(super) fn talk_reply(&self, heard: &str) -> bool {
        self.in_talk() && heard.split_whitespace().count() <= TALK_REPLY_WORDS && !self.tail_echo(heard)
    }

    /// A conversation with the chat model is going on: its echo filter is `tail_echo`.
    pub(super) fn in_talk(&self) -> bool {
        self.config.talk.is_some()
            && self.conversation.follow_up()
            && !self.conversation.unnamed()
            && self.conversation.last_talk.is_some_and(|at| at.elapsed() < HISTORY_MAX_AGE)
    }

    fn talk_prompt(&self) -> String {
        let people: Vec<String> = self
            .home_assistant
            .as_ref()
            .and_then(|ha| ha.people().ok())
            .unwrap_or_default()
            .into_iter()
            .map(|person| person.name)
            .collect();
        let notes: Vec<String> = self.memory.sentences().map(str::to_owned).collect();
        let today = today();
        talk::system_prompt(&TalkContext {
            name: self.config.wake.name(),
            home: self.config.home.as_deref(),
            room: self.room.as_deref(),
            today: &today,
            honorific: self.persona.as_ref().map(|persona| persona.honorific()),
            people: &people,
            notes: &notes,
        })
    }

    /// Loads the chat model with the prompt it will see, so the first question pays for neither.
    pub(super) fn warm_talk(&self) {
        let Some(chat) = &self.config.talk else { return };
        let start = Instant::now();
        match chat.warm_up(&self.talk_prompt()) {
            Ok(()) => info!(model = chat.model(), elapsed = ?start.elapsed(), "chat model warmed up"),
            Err(error) => warn!(%error, "chat model not reachable; open talk gets the fallback"),
        }
    }
}
