//! Open conversation: what no rule takes goes to the local chat model (`chat-url`), and its answer
//! is spoken a sentence at a time while the model writes the rest. After such an answer the
//! follow-up window takes a reply without the name, so a conversation can go on.

use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::anyhow;
use tracing::{info, warn};
use voice_assistant::intent::TALK_FILLERS;
use voice_assistant::lang::Lang;
use voice_assistant::llm::Turn;
use voice_assistant::talk::{self, TalkContext};

use super::conversation::HISTORY_MAX_AGE;
use super::leads::lead_sentence;
use super::speech::Spoken;
use super::{today, AssistantEvent, Worker};
use crate::unhandled::Reason;

/// The longest reply without the name that goes on with a conversation: longer is the room or the TV.
const TALK_REPLY_WORDS: usize = 12;
/// How long after the lead-in has played a quiet chat model gets a filler ("Let me see."): a pause
/// longer than this reads as the assistant having stopped.
const FILLER_AFTER: Duration = Duration::from_millis(400);
/// How often the household's people are read again from Home Assistant for the prompt.
const PEOPLE_REFRESH: Duration = Duration::from_secs(30 * 60);

/// The words of `heard`, a sentence said again counted once: "What about do you believe in
/// Skibberty Toilet? Do you believe in Skibberty Toilet?" was 14 words, and a reply said twice for want
/// of an answer was taken for the room (23 Nov).
fn words_said_once(heard: &str) -> usize {
    let sentences: Vec<Vec<String>> =
        heard.split(['.', '?', '!']).map(voice_assistant::dialog::words).filter(|words| !words.is_empty()).collect();
    let repeat = |i: usize| sentences[..i].iter().any(|earlier| earlier.ends_with(&sentences[i]));
    (0..sentences.len()).filter(|&i| !repeat(i)).map(|i| sentences[i].len()).sum()
}

/// A chat answer begun on a draft: its sentences wait in `sentences` until the utterance's end
/// asks the same.
pub(super) struct DraftRun {
    request: String,
    lang: Lang,
    system: String,
    history: Vec<Turn>,
    opening: Option<String>,
    sentences: Receiver<String>,
    reader: JoinHandle<anyhow::Result<talk::Answered>>,
    stop: Arc<AtomicBool>,
    at: Instant,
}

impl DraftRun {
    /// Whether this run is the answer `converse` would start now: same words, prompt, history and
    /// lead-in, so the model read just what it would read.
    fn asked(&self, request: &str, lang: Lang, system: &str, history: &[Turn], wants_lead: bool) -> bool {
        self.request == request
            && self.lang == lang
            && self.system == system
            && self.opening.is_some() == wants_lead
            && self.history.len() == history.len()
            && self.history.iter().zip(history).all(|(a, b)| a.user == b.user && a.assistant == b.assistant)
    }

    /// Whether it was begun on `request`.
    pub(super) fn for_request(&self, request: &str) -> bool {
        self.request == request
    }

    /// Ends the run: the reader closes the stream at the server's next piece, and the server stops.
    fn stop(&self) {
        self.stop.store(true, Relaxed);
    }
}

/// The chat model's answer to `text` on a thread of its own, each sentence sent on as it is whole.
fn start_reader(
    chat: talk::Talk,
    system: String,
    history: Vec<Turn>,
    text: &str,
    opening: Option<String>,
    lang: Lang,
    stop: Arc<AtomicBool>,
) -> Option<(Receiver<String>, JoinHandle<anyhow::Result<talk::Answered>>)> {
    let (sentences, heard) = mpsc::channel::<String>();
    let request = talk::as_meant(text).into_owned();
    if request != text {
        info!(heard = text, meant = request, "chat request put right");
    }
    let reader = std::thread::Builder::new().name("chat".into()).spawn(move || {
        chat.answer(&system, &history, &request, opening.as_deref(), lang, &stop, &mut |sentence| {
            sentences.send(sentence.to_owned()).is_ok()
        })
    });
    match reader {
        Ok(reader) => Some((heard, reader)),
        Err(error) => {
            warn!(%error, "could not start the chat model's reader");
            None
        }
    }
}

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
        let wants_lead = aloud && lang == Lang::English && self.render.is_some() && !self.answer_announced;
        // The run begun on the draft, when it was asked exactly this; else it stops.
        let draft = self.draft_run.take();
        let draft = match draft {
            Some(run) if run.asked(text, lang, &system, &history, wants_lead) => Some(run),
            Some(run) => {
                run.stop();
                info!(draft = run.request, heard = text, "chat model's draft run stopped");
                None
            }
            None => None,
        };
        let on_draft = draft.as_ref().map(|run| start.duration_since(run.at));
        let (heard, reader, opening) = match draft {
            Some(run) => (run.sentences, run.reader, run.opening),
            None => {
                let opening = wants_lead.then(|| self.pick_talk_lead(text));
                match start_reader(chat, system, history, text, opening.clone(), lang, Arc::default()) {
                    Some((heard, reader)) => (heard, reader, opening),
                    None => return None,
                }
            }
        };
        if let Some(lead) = &opening {
            self.announce_answer(lead);
            self.speak(lead, lang, &mut spoken);
        }
        // The lead-in was queued at once: it has played this long from now on.
        let mut filler_at = (aloud && spoken.started()).then(|| Instant::now() + spoken.duration() + FILLER_AFTER);
        let mut said = Vec::new();
        let mut interrupted = false;
        loop {
            let next = match filler_at {
                Some(at) => heard.recv_timeout(at.saturating_duration_since(Instant::now())),
                None => heard.recv().map_err(|_| RecvTimeoutError::Disconnected),
            };
            match next {
                Ok(sentence) => {
                    filler_at = None;
                    first.get_or_insert_with(|| start.elapsed());
                    if aloud {
                        self.announce_answer(&sentence);
                        // Each answer is new words: synthesized live, not kept in the speech cache.
                        self.live.insert(sentence.clone());
                        self.speak(&sentence, lang, &mut spoken);
                    }
                    said.push(sentence);
                    if self.interrupted.load(Relaxed) {
                        interrupted = true;
                        break;
                    }
                }
                // Silence after the lead-in sounds like a hang: one more phrase, from the cache.
                Err(RecvTimeoutError::Timeout) => {
                    filler_at = None;
                    let filler = lead_sentence(TALK_FILLERS[self.lead_turn % TALK_FILLERS.len()], None);
                    info!(filler, "chat model still quiet");
                    self.speak(&filler, lang, &mut spoken);
                }
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        drop(heard);
        // Interrupted, the reader stops at its next sentence; nobody waits for it.
        let result = if interrupted {
            let said = said.join(" ");
            Ok(talk::Answered { raw: said.clone(), said, ..Default::default() })
        } else {
            reader.join().unwrap_or_else(|_| Err(anyhow!("the chat model's reader panicked")))
        };
        let (answer, raw) = match result {
            Ok(answered) if !answered.said.is_empty() => {
                // `read_tokens` is the whole prompt; `read` is the time for what the server's cache did
                // not hold (about 150 tokens a second on the node). A long one means the prompt or the
                // history changed near its start.
                info!(
                    first_sentence = ?first,
                    started_on_draft = ?on_draft,
                    took = ?start.elapsed(),
                    read_tokens = answered.read_tokens,
                    read = ?answered.read,
                    tokens_per_second = format!("{:.1}", answered.speed()),
                    "chat model answered"
                );
                self.unhandled(Reason::LanguageModel, text, Some(&answered.said));
                (answered.said, answered.raw)
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
                (fallback.to_owned(), fallback.to_owned())
            }
        };
        if aloud {
            self.talked = Some((spoken, raw));
        }
        // The server's cache now holds this conversation; the base goes back in once it is over.
        self.talk_base_gone = true;
        Some(answer)
    }

    /// The lead-in for a chat answer to `text`, the next of those that fit it.
    fn pick_talk_lead(&mut self, text: &str) -> String {
        self.lead_turn = self.lead_turn.wrapping_add(1);
        let leads = talk::leads_for(text);
        let honorific = self.persona.as_ref().map(|persona| persona.honorific().to_owned());
        lead_sentence(leads[self.lead_turn % leads.len()], honorific.as_deref())
    }

    /// Starts the chat model on a draft that would go to it: the 640 ms closing silence then
    /// overlaps the model's reading and first words (`converse` waited for it, 23 Nov). Its
    /// sentences wait until the utterance ends with the same words; nothing is spoken before.
    pub(super) fn start_on_draft(&mut self, request: &str, lang: Lang) {
        let Some(chat) = self.config.talk.clone() else { return };
        if self.draft_run.as_ref().is_some_and(|run| run.request == request) {
            return;
        }
        self.stop_draft_run();
        let system = self.talk_prompt();
        let history = self.conversation.recent_turns();
        // As `converse` will want it; it asks again at the end and stops this run if not. Not
        // `answer_announced`: it still holds the last answer's, and `answer` clears it for a request
        // no rule takes.
        let wants_lead = lang == Lang::English && self.render.is_some();
        let opening = wants_lead.then(|| self.pick_talk_lead(request));
        let stop = Arc::new(AtomicBool::new(false));
        let started = start_reader(chat, system.clone(), history.clone(), request, opening.clone(), lang, stop.clone());
        let Some((sentences, reader)) = started else { return };
        info!(request, "chat model started on the draft");
        self.draft_run = Some(DraftRun {
            request: request.to_owned(),
            lang,
            system,
            history,
            opening,
            sentences,
            reader,
            stop,
            at: Instant::now(),
        });
    }

    /// Whether the words of a draft, as no rule takes them, would go to the chat model when the
    /// utterance ends: said with the name, after a wake word with the name, or a reply in a
    /// conversation. The same tests as `heard`, in short; a miss only costs a stopped run.
    pub(super) fn chat_takes_draft(&self, request: &str, named: bool) -> bool {
        let conversation = &self.conversation;
        self.config.talk.is_some()
            && self.config.llm.is_none()
            && !self.previewing
            && self.awaiting().is_none()
            // A wake counts once its name check said yes: most wakes are the TV, and while the check
            // is pending `unnamed` is false.
            && (named || conversation.wake_named() == Some(true) || self.talk_reply(request))
    }

    /// Stops a run begun on a draft that no answer took.
    pub(super) fn stop_draft_run(&mut self) {
        if let Some(run) = self.draft_run.take() {
            run.stop();
            info!(draft = run.request, "chat model's draft run stopped");
        }
    }

    /// The `Answer` event, once per answer and before its audio: a satellite starts its reply on it.
    fn announce_answer(&mut self, text: &str) {
        if !std::mem::replace(&mut self.answer_announced, true) {
            self.emit(AssistantEvent::Answer { text: text.to_owned() });
        }
    }

    /// Words without the name in the follow-up window that go on with the conversation: after an
    /// answer of the chat model's ("Tired. Long day." after "And you?"), or after any answer to a
    /// request said with the name ("I feel sick." after the morning briefing, 23 Nov). Short, and
    /// not that answer heard back.
    pub(super) fn talk_reply(&self, heard: &str) -> bool {
        let short = words_said_once(heard) <= TALK_REPLY_WORDS;
        if self.in_talk() {
            return short && !self.tail_echo(heard);
        }
        let after_named = self.config.talk.is_some()
            && self.conversation.follow_up()
            && self.conversation.wake.is_none()
            && self.conversation.window_named;
        after_named && short && !self.echo(heard)
    }

    /// A conversation with the chat model is going on: its echo filter is `tail_echo`.
    pub(super) fn in_talk(&self) -> bool {
        self.config.talk.is_some()
            && self.conversation.follow_up()
            && !self.conversation.unnamed()
            && self.conversation.last_talk.is_some_and(|at| at.elapsed() < HISTORY_MAX_AGE)
    }

    /// The system prompt, from what is known now: no call to Home Assistant, so it costs nothing
    /// before an answer, and it stays the same text, so the server's prompt cache holds it.
    fn talk_prompt(&self) -> String {
        let people = &self.talk_people.0;
        let notes: Vec<String> = self.memory.sentences().map(str::to_owned).collect();
        let today = today();
        talk::system_prompt(&TalkContext {
            name: self.config.wake.name(),
            home: self.config.home.as_deref(),
            room: self.room.as_deref(),
            today: &today,
            honorific: self.persona.as_ref().map(|persona| persona.honorific()),
            people,
            notes: &notes,
        })
    }

    /// The household's people from Home Assistant, at most every `PEOPLE_REFRESH`; a failed call
    /// keeps the list it had, as an empty one would change the prompt.
    fn refresh_talk_people(&mut self) {
        if self.talk_people.1.is_some_and(|at| at.elapsed() < PEOPLE_REFRESH) {
            return;
        }
        self.talk_people.1 = Some(Instant::now());
        let Some(ha) = &self.home_assistant else { return };
        match ha.people() {
            Ok(people) => self.talk_people.0 = people.into_iter().map(|person| person.name).collect(),
            Err(error) => warn!(%error, "could not read the household's people for the chat model"),
        }
    }

    /// The base, and the conversation while it goes on, back into the chat model's cache in the
    /// background: when the system prompt changed (a new day, a new note, the room), when the
    /// history was cut, and once a conversation is over. The next request then extends what is
    /// cached and reads only its own words (a miss read it all again: 2.6-3.6 s on 22 Nov). Run
    /// after each answer and while idle; most times it finds nothing to do.
    pub(super) fn keep_talk_warm(&mut self) {
        let Some(chat) = self.config.talk.clone() else { return };
        self.refresh_talk_people();
        let system = self.talk_prompt();
        let cut = std::mem::take(&mut self.conversation.history_cut);
        // Within `HISTORY_MAX_AGE` a request still carries the history and extends what is cached.
        let over = self.talk_base_gone && !self.conversation.going_on();
        if system == self.talk_warmed && !over && !cut {
            return;
        }
        let history = self.conversation.recent_turns();
        self.talk_warmed = system.clone();
        self.talk_base_gone = !history.is_empty();
        let spawned = std::thread::Builder::new().name("chat-warm".into()).spawn(move || {
            let start = Instant::now();
            match chat.warm_up(&system, &history) {
                Ok(()) => info!(elapsed = ?start.elapsed(), exchanges = history.len(), "chat model's cache read again"),
                Err(error) => warn!(%error, "chat model warm-up failed"),
            }
        });
        if let Err(error) = spawned {
            warn!(%error, "could not start the chat model warm-up");
        }
    }

    /// The chat model gave an answer in this conversation: small talk goes to it too, as a fixed
    /// answer breaks the thread ("I'm sad" got "Sorry about that. Tomorrow's another day.", 22 Nov).
    pub(super) fn talking_with_model(&self) -> bool {
        self.config.talk.is_some()
            && self.conversation.going_on()
            && self.conversation.model_talk.is_some_and(|at| at.elapsed() < HISTORY_MAX_AGE)
    }

    /// Loads the chat model with the prompt it will see, so the first question pays for neither.
    pub(super) fn warm_talk(&mut self) {
        let Some(chat) = self.config.talk.clone() else { return };
        self.refresh_talk_people();
        let start = Instant::now();
        let system = self.talk_prompt();
        match chat.warm_up(&system, &[]) {
            Ok(()) => {
                info!(model = chat.model(), elapsed = ?start.elapsed(), "chat model warmed up");
                self.talk_warmed = system;
            }
            Err(error) => warn!(%error, "chat model not reachable; open talk gets the fallback"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sentence_said_again_counts_once() {
        let twice = "What about do you believe in Skibberty Toilet? Do you believe in Skibberty Toilet?";
        assert_eq!(words_said_once(twice), 8);
        assert_eq!(words_said_once("I'm sick. I'm sick."), 2);
        assert_eq!(words_said_once("I found out you can go to China. I said no more."), 12);
    }
}
