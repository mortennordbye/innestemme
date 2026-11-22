//! The state of one conversation: the questions asked back, the wake that opened it, the follow-up
//! window, the music paused for it and what follows its answer. Kept apart from the worker's
//! services so that a conversation can later be kept for each satellite.

use std::collections::VecDeque;
use std::sync::atomic::Ordering::Relaxed;
use std::time::{Duration, Instant};

use voice_assistant::house::HouseCommand;
use voice_assistant::intent::MusicCommand;
use voice_assistant::llm::Turn;
use voice_assistant::sound_alike::SkillWord;
use voice_assistant::whisper::Transcript;

use super::hush::Hushed;
use super::shopping::LIST_OPEN;
use super::speech::ANSWER_START;
use super::{AssistantEvent, Worker, FOLLOW_UP};

/// Earlier exchanges the language model sees, for follow-ups ("and tomorrow?").
const HISTORY_TURNS: usize = 4;
const HISTORY_MAX_AGE: Duration = Duration::from_secs(300);
/// How long "Turn on privacy mode?" waits for its "yes".
pub(super) const CONFIRM_WINDOW: Duration = Duration::from_secs(30);
/// How long "Which room?" waits for the room, for the lights and for a scene. As long as the
/// confirm and the timers' "Which one?": a reply comes within seconds, and "the kitchen" said much
/// later is about something else, not a light to switch.
pub(super) const ROOM_WINDOW: Duration = Duration::from_secs(30);
/// How long "Did you mean the temperature? Or should I look up therapy?" waits for its reply: as
/// long as the other questions asked back.
pub(super) const CHOICE_WINDOW: Duration = Duration::from_secs(30);

/// A look-up topic that sounds like a skill word, both readings kept until the reply picks one.
pub(super) struct PendingChoice {
    pub(super) skill: &'static SkillWord,
    pub(super) topic: String,
    pub(super) at: Instant,
}

/// The wake word that opened the conversation, from a satellite.
pub(super) struct Wake {
    pub(super) at: Instant,
    /// Whether its audio held the assistant's name, when the engine detected it.
    pub(super) named: Option<bool>,
}

/// Music to start or stop, and the volume to set, once the answer has played: the lockdown's music
/// and its stand-down, the open source party's song.
pub(super) struct AfterAnswer {
    pub(super) music: MusicCommand,
    pub(super) volume: Option<f64>,
    /// It starts as the answer starts playing, not when it has played.
    pub(super) under_answer: bool,
    /// When the answer will have played; `None` until it is spoken.
    pub(super) at: Option<Instant>,
}

impl AfterAnswer {
    pub(super) fn new(music: MusicCommand, volume: Option<f64>, under_answer: bool) -> Self {
        Self { music, volume, under_answer, at: None }
    }
}

/// A question the assistant asked back, which the next words may answer. Derived from the
/// conversation, the timers and the notes each time: each question keeps its own expiry, and the
/// timers and the notes own theirs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Awaiting {
    /// "For how long?" or "When should I remind you?"
    TimerDuration,
    /// "Which one?" among the timers.
    WhichTimer,
    /// "Which room?" for the lights.
    LightsRoom,
    /// The shopping list is open: short replies are more items.
    ListItems,
    /// "Forget everything I remember?" or "Forget all of them?"
    Forget,
    /// "Remember what?"
    RememberWhat,
    /// "Turn on privacy mode?": a camera or alarm change waits for "yes".
    Confirm,
    /// "Which room?" for a scene.
    SceneRoom,
    /// "Did you mean the temperature? Or should I look up therapy?"
    Choice,
}

/// How a turn ends.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum TurnEnd {
    /// Nothing was asked after the wake word: no event, and the follow-up window stays as it is.
    Silent,
    /// The conversation ended without an answer ("never mind"): no follow-up window.
    Ended,
    /// Done without words: the follow-up window opens now.
    Done,
    /// The answer was spoken: the follow-up window opens when it has played, `seconds` from now.
    Spoken { seconds: f32 },
}

#[derive(Default)]
pub(super) struct Conversation {
    /// "Turn off the lights" named no room and there was nothing to refer back to: the next answer
    /// is the room. Whether to switch them on, and when it was asked.
    pub(super) pending_lights: Option<(bool, Instant)>,
    /// A scene request that exists in several rooms: the next answer is the room.
    pub(super) pending_scene: Option<(String, Instant)>,
    /// A camera or alarm change waiting for "yes": a misheard request must not switch them.
    pub(super) pending_confirm: Option<(HouseCommand, Instant)>,
    /// "Did you mean the temperature?": the skill or the look-up, as the reply picks.
    pub(super) pending_choice: Option<PendingChoice>,
    /// When an item was last added: the list takes more without "add" until `LIST_OPEN` later.
    pub(super) list_open: Option<Instant>,
    /// A satellite woke on its own wake word and nothing has been answered since.
    pub(super) wake: Option<Wake>,
    /// What follows the answer.
    pub(super) after_answer: Option<AfterAnswer>,
    /// The music was paused for a request, and when something was last heard. It plays on after
    /// the answer, unless the request was to stop it.
    pub(super) hushed: Option<Hushed>,
    /// Until then, speech counts without the name: after a bare name, and for a while after each
    /// answer, like a person who is still looking at you. Only recognised requests are acted on.
    pub(super) follow_up_until: Option<Instant>,
    /// The answer opens no follow-up window.
    pub(super) follow_up_closed: bool,
    /// Recent exchanges, oldest first, for the language model.
    pub(super) history: VecDeque<(Instant, Turn)>,
    /// The last request answered, and when: "what about inside?" goes on from it. As text, which
    /// parses back to its intent.
    pub(super) last_request: Option<(String, Instant)>,
    /// The request a reply to a question asked back made whole ("the kitchen" after "Which room?"),
    /// kept as the last request in its place.
    pub(super) answered_as: Option<String>,
    /// The latest draft and its transcripts.
    pub(super) draft: Option<(Vec<i16>, Vec<Transcript>)>,
    /// A command was acted on from this draft, with its words (the name taken off): a later draft
    /// or the utterance that starts with the same audio is that speech going on, and is not
    /// answered again.
    pub(super) answered_draft: Option<(Vec<i16>, Vec<String>)>,
    /// What undoes the music command answered from the draft, for a request that goes on to name
    /// another room.
    pub(super) draft_undo: Option<MusicCommand>,
}

impl Conversation {
    pub(super) fn follow_up(&self) -> bool {
        self.follow_up_until.is_some_and(|until| Instant::now() < until)
    }

    /// When what follows the answer is due.
    pub(super) fn after_answer_at(&self) -> Option<Instant> {
        self.after_answer.as_ref().and_then(|after| after.at)
    }

    /// The wake's name check, when there is a wake and the check is done.
    pub(super) fn wake_named(&self) -> Option<bool> {
        self.wake.as_ref().and_then(|wake| wake.named)
    }

    /// The name check's result, for the wake it was made for. A check only runs during a wake.
    pub(super) fn set_wake_named(&mut self, named: Option<bool>) {
        if let Some(wake) = &mut self.wake {
            wake.named = named;
        }
    }

    /// A wake whose audio did not hold the name.
    pub(super) fn unnamed(&self) -> bool {
        self.wake_named() == Some(false)
    }

    pub(super) fn list_is_open(&self) -> bool {
        self.list_open.is_some_and(|at| at.elapsed() < LIST_OPEN)
    }

    pub(super) fn lights_asked(&self) -> bool {
        self.pending_lights.is_some_and(|(_, at)| at.elapsed() < ROOM_WINDOW)
    }

    pub(super) fn scene_asked(&self) -> bool {
        self.pending_scene.as_ref().is_some_and(|(_, at)| at.elapsed() < ROOM_WINDOW)
    }

    pub(super) fn confirm_asked(&self) -> bool {
        self.pending_confirm.as_ref().is_some_and(|(_, at)| at.elapsed() < CONFIRM_WINDOW)
    }

    /// Drops the questions this conversation holds; the timers and the notes hold their own.
    pub(super) fn forget_questions(&mut self) {
        (self.pending_lights, self.pending_scene, self.pending_confirm, self.list_open) = (None, None, None, None);
        self.pending_choice = None;
    }

    pub(super) fn choice_asked(&self) -> Option<&PendingChoice> {
        self.pending_choice.as_ref().filter(|choice| choice.at.elapsed() < CHOICE_WINDOW)
    }

    /// An exchange for the language model's history; the oldest go.
    pub(super) fn remember(&mut self, user: &str, assistant: String) {
        self.history.push_back((Instant::now(), Turn { user: user.to_owned(), assistant }));
        while self.history.len() > HISTORY_TURNS {
            self.history.pop_front();
        }
    }

    /// The request answered, for follow-ons. Several requests in one sentence leave no topic.
    pub(super) fn answered(&mut self, request: &str) {
        let request = self.answered_as.take().unwrap_or_else(|| request.to_owned());
        let one = voice_assistant::intent::split_commands(&request).is_none();
        self.last_request = one.then(|| (request, Instant::now()));
    }

    /// The last request answered, while it is recent enough to go on from: as long as the language
    /// model's history, so the name keeps a topic open for minutes, the follow-up window for its
    /// few seconds.
    pub(super) fn last_request(&self) -> Option<&str> {
        self.last_request.as_ref().filter(|(_, at)| at.elapsed() < HISTORY_MAX_AGE).map(|(text, _)| text.as_str())
    }

    /// The exchanges recent enough to go on from.
    pub(super) fn recent_turns(&self) -> Vec<Turn> {
        self.history.iter().filter(|(at, _)| at.elapsed() < HISTORY_MAX_AGE).map(|(_, turn)| turn.clone()).collect()
    }

    /// A session started: nothing said before it is gone on from.
    pub(super) fn restart(&mut self) {
        (self.follow_up_until, self.pending_lights, self.wake, self.last_request) = (None, None, None, None);
        self.history.clear();
    }

    /// A typed request is done: there is no answer to wait out, so what follows it follows at once,
    /// and it opens no follow-up window to close.
    pub(super) fn typed_done(&mut self) {
        self.follow_up_closed = false;
        if let Some(after) = &mut self.after_answer {
            after.at = Some(Instant::now());
        }
    }
}

impl Worker {
    /// The question the next words may answer, if any. With several, the one `compose` takes
    /// first. The timers' "Which one?" goes stale in `heard`, which drops it before this is asked.
    pub(super) fn awaiting(&self) -> Option<Awaiting> {
        let conversation = &self.conversation;
        if self.timers.choosing() {
            Some(Awaiting::WhichTimer)
        } else if self.timers.awaiting_duration() {
            Some(Awaiting::TimerDuration)
        } else if conversation.lights_asked() {
            Some(Awaiting::LightsRoom)
        } else if conversation.list_is_open() {
            Some(Awaiting::ListItems)
        } else if self.memory.asked_to_forget() {
            Some(Awaiting::Forget)
        } else if self.memory.asked_what() {
            Some(Awaiting::RememberWhat)
        } else if conversation.confirm_asked() {
            Some(Awaiting::Confirm)
        } else if conversation.scene_asked() {
            Some(Awaiting::SceneRoom)
        } else if conversation.choice_asked().is_some() {
            Some(Awaiting::Choice)
        } else {
            None
        }
    }

    /// Drops every question waiting for an answer: the conversation's, the timers' and the notes'.
    pub(super) fn forget_questions(&mut self) {
        self.conversation.forget_questions();
        self.timers.clear_pending();
        self.memory.clear_question();
    }

    /// The end of a turn: the event that tells satellites, the follow-up window, the music paused
    /// for it, and the wake that opened it.
    pub(super) fn end_turn(&mut self, end: TurnEnd) {
        let open = !std::mem::take(&mut self.conversation.follow_up_closed);
        let window = |after: Duration| open.then(|| Instant::now() + after + FOLLOW_UP);
        match end {
            TurnEnd::Silent => {
                // An interrupt with no request after it: nothing clears the latch at the start of an
                // answer, and the processor has long seen it. Not cleared at the end of an answered
                // turn, which may be before the processor has dropped the answer's audio for it.
                self.interrupted.store(false, Relaxed);
            }
            TurnEnd::Ended => {
                // "Never mind" ends the topic too.
                (self.conversation.follow_up_until, self.conversation.last_request) = (None, None);
                self.emit(AssistantEvent::Done);
            }
            TurnEnd::Done => {
                self.conversation.follow_up_until = window(Duration::ZERO);
                self.emit(AssistantEvent::Done);
            }
            TurnEnd::Spoken { .. } => self.emit(AssistantEvent::Spoken),
        }
        // After the answer is queued: the speaker's announcement ducks the music under it.
        self.unhush();
        if let TurnEnd::Spoken { seconds } = end {
            // The reply is queued faster than it plays; the window opens when it has been heard.
            let played = Duration::from_secs_f32(seconds);
            self.conversation.follow_up_until = window(played);
            if let Some(after) = &mut self.conversation.after_answer {
                // The lockdown's and the open source party's music start under the answer; other
                // music waits for it.
                let wait = if after.under_answer { Duration::ZERO } else { played };
                after.at = Some(Instant::now() + wait + ANSWER_START);
            }
        }
        // A check still out pauses nothing now: the run is over.
        self.wake_check = None;
        self.conversation.wake = None;
        self.busy.store(false, Relaxed);
    }
}
