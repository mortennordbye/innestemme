//! What is heard without the name: the room talking, and the answer coming back from the speaker.

use std::collections::HashSet;
use std::time::Duration;

use tracing::info;
use voice_assistant::intent::{self, Intent};

use super::Worker;

/// Speech without the name that repeats what was said this recently is the answer coming back from
/// the speaker: the player starts late, so the end of an answer can reach the follow-up window.
pub(super) const ECHO_WINDOW: Duration = Duration::from_secs(30);

/// How requests open, after any "hey", "please" or the name: a command or a question. Talk from
/// the TV or the room mostly opens otherwise ("I'm...", "That's...", "The...").
const REQUEST_OPENINGS: &[&str] = &[
    "turn",
    "switch",
    "set",
    "start",
    "stop",
    "pause",
    "play",
    "resume",
    "cancel",
    "add",
    "remove",
    "put",
    "show",
    "showed",
    "tell",
    "give",
    "make",
    "open",
    "close",
    "lock",
    "unlock",
    "dim",
    "brighten",
    "raise",
    "lower",
    "increase",
    "decrease",
    "mute",
    "unmute",
    "skip",
    "next",
    "previous",
    "remind",
    "wake",
    "snooze",
    "find",
    "ring",
    "call",
    "check",
    "read",
    "arm",
    "disarm",
    "activate",
    "deactivate",
    "enable",
    "disable",
    "shuffle",
    "repeat",
    "louder",
    "quieter",
    "volume",
    "lights",
    "light",
    "timer",
    "music",
    "what",
    "what's",
    "whats",
    "when",
    "where",
    "who",
    "who's",
    "whos",
    "how",
    "how's",
    "hows",
    "why",
    "which",
    "is",
    "are",
    "do",
    "does",
    "can",
    "could",
    "would",
    "will",
    "should",
    "any",
    "slå",
    "skru",
    "sett",
    "spill",
    "stopp",
    "start",
    "vis",
    "fortell",
    "legg",
    "hva",
    "hvor",
    "hvem",
    "hvordan",
    "hvorfor",
    "når",
    "kan",
    "er",
];
const REQUEST_FILLER: &[&str] =
    &["hey", "hi", "ok", "okay", "please", "so", "um", "uh", "oh", "and", "now", "just", "hei", "kjære"];

/// Sounds, not words: what Parakeet hears in a breath or the end of the wake word.
const SOUNDS: &[&str] =
    &["oh", "ah", "uh", "um", "umm", "hm", "hmm", "mm", "mhm", "er", "erm", "eh", "huh", "ooh", "aah"];

/// Whether `text` holds only sounds. "Hey Jarvis" and a breath was heard as "Oh", and the chat
/// model answered it with chamomile tea (28 Nov).
pub(super) fn only_sounds(text: &str) -> bool {
    voice_assistant::dialog::words(text).iter().all(|w| SOUNDS.contains(&w.as_str()))
}

/// Words that open a command for the house, not talk: what no rule takes with one of them is a
/// command misheard, and the chat model would pretend to do it ("Turn off the sticks." got "The
/// sticks are currently positioned where they need to be.", 27 Nov).
const COMMAND_OPENINGS: &[&str] = &[
    "turn", "switch", "dim", "brighten", "play", "pause", "resume", "skip", "mute", "unmute", "volume", "louder",
    "quieter", "lock", "unlock", "arm", "disarm", "shuffle", "slå", "skru", "spill",
];
/// Asking words before a command: "can you turn off ...".
const COMMAND_POLITE: &[&str] = &["can", "could", "would", "will", "you", "kan", "du"];

/// Whether unrecognised words open like a command for the house.
pub(super) fn sounds_like_a_command(text: &str) -> bool {
    text.split_whitespace()
        .map(voice_assistant::dialog::normalize)
        .find(|w| !w.is_empty() && !REQUEST_FILLER.contains(&w.as_str()) && !COMMAND_POLITE.contains(&w.as_str()))
        .is_some_and(|w| COMMAND_OPENINGS.contains(&w.as_str()))
}

/// Words in a follow-up without the name, past which it has to open like a request: a long
/// sentence that does not is the TV or the room.
const FOLLOW_UP_MOST_WORDS: usize = 8;

/// Speech in the follow-up window that is too long and does not open like a request.
pub(super) fn room_talk(text: &str) -> bool {
    text.split_whitespace().count() > FOLLOW_UP_MOST_WORDS && !sounds_like_a_request(text)
}

/// Whether unrecognised speech opens like a request at all.
pub(super) fn sounds_like_a_request(text: &str) -> bool {
    text.split_whitespace()
        .map(voice_assistant::dialog::normalize)
        .find(|w| !w.is_empty() && !REQUEST_FILLER.contains(&w.as_str()))
        .is_some_and(|w| REQUEST_OPENINGS.contains(&w.as_str()))
}

/// Whether `heard` repeats what was `said`: all its words but at most one were in it ("Left on the
/// one minute timer, sir." after "37 seconds left on the 1 minute timer, sir."). A follow-up adds
/// words of its own ("stop the timer"). After a question the answer may well repeat its words
/// ("Which one?" "The 1 minute timer."), so nothing is an echo then.
fn is_echo(said: &[&str], heard: &str) -> bool {
    let Some(last) = said.last() else { return false };
    // A reply to a question may use its words ("You have 2 timers. Which one?" "The 1 minute
    // timer."); only the question itself, with a word misheard at most, is its echo: "Anything else,
    // sir?" came back as "Than anything else." and went on the shopping list (15 Nov).
    if last.trim_end().ends_with('?') {
        return echoes_question(last, heard);
    }
    let said: HashSet<String> = said.iter().flat_map(|text| echo_words(text)).collect();
    let heard = echo_words(heard);
    let new = heard.iter().filter(|w| !said.contains(*w)).count();
    // A long stretch of a long answer comes back garbled ("Barnett" as "barn it"), but mostly in its
    // words; nobody asks a follow-up of that many words made of the answer's.
    let garbled = heard.len() >= ECHO_LONG && new * 4 <= heard.len();
    heard.len() >= 2 && (new <= 1 && new * 5 <= heard.len() || garbled)
}

/// Whether `heard` is the closing question of `said`, with one word more or misheard at most.
fn echoes_question(said: &str, heard: &str) -> bool {
    const HONORIFICS: &[&str] = &["sir", "ma'am", "madam"];
    let question = said.trim_end().rsplit_terminator(['.', '!']).next().unwrap_or(said);
    let question: Vec<String> =
        echo_words(question).into_iter().filter(|w| !HONORIFICS.contains(&w.as_str())).collect();
    let heard: Vec<String> = echo_words(heard).into_iter().filter(|w| !HONORIFICS.contains(&w.as_str())).collect();
    // "Ikke noe mer." and "Not anything else." answer "Noe mer?" and "Anything else?".
    const NEGATIONS: &[&str] = &["not", "no", "nope", "nothing", "ikke", "nei", "ingenting"];
    let extra = &heard[..heard.len().saturating_sub(question.len())];
    question.len() >= 2
        && heard.len() <= question.len() + 1
        && heard.ends_with(&question)
        && !extra.iter().any(|w| NEGATIONS.contains(&w.as_str()))
}

/// Words from which a heard stretch may be a garbled echo.
const ECHO_LONG: usize = 8;

/// Words for comparing what was heard with what was said, with numbers as digits: the speech
/// says "1 minute", the transcriber hears "one minute".
fn echo_words(text: &str) -> Vec<String> {
    const NUMBERS: &[&str] =
        &["zero", "one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten", "eleven", "twelve"];
    // "40-year" is said as "40 year".
    text.split(|c: char| c.is_whitespace() || c == '-')
        .map(voice_assistant::dialog::normalize)
        .filter(|w| !w.is_empty())
        .map(|w| NUMBERS.iter().position(|n| *n == w).map_or(w, |n| n.to_string()))
        .collect()
}

impl Worker {
    /// The request words go on to from the last one answered ("what about inside?" after the
    /// weather), and its intent. Only while nothing asked back waits for the words, which are the
    /// reply to it then, and while the last request is recent: the follow-up window, a wake, or the
    /// name within minutes of it. The words must already be meant for the assistant; that is
    /// decided before.
    pub(super) fn follow_on(&self, request: &str, parsed: &Intent) -> Option<(String, Intent)> {
        if self.awaiting().is_some() {
            return None;
        }
        let last = intent::parse(self.conversation.last_request()?);
        if !intent::open_to_follow_on(&last, parsed) {
            return None;
        }
        let (meant, intent) = intent::follow_on(&last, request)?;
        info!(skill = intent.meta().skill.unwrap_or_default(), meant, "follow-on");
        Some((meant, intent))
    }

    /// While the party is on, "<verb> the party" without a word that starts one is a misheard
    /// "stop": "Slope the party." restarted it (16 Nov).
    pub(super) fn party_stop(&self, request: &str, parsed: &Intent) -> Option<(String, Intent)> {
        // A question asked back takes the words as its reply: a list item, a note.
        let partying = (self.config.party.mode().is_some() || self.open_source_party) && self.awaiting().is_none();
        let words = voice_assistant::dialog::words(request);
        if !partying || *parsed != Intent::Unknown || !voice_assistant::party::unclear(&words) {
            return None;
        }
        info!("party request while it is on: stop");
        let meant = "stop the party".to_owned();
        let intent = intent::parse(&meant);
        Some((meant, intent))
    }

    /// Whether words heard without the name are the end of the answer heard back, in a
    /// conversation: a run of the words of its last two sentences, as the microphone opens while
    /// the speaker still plays it ("Sir.", "Reality of this home.", 22 Nov). Not all the words of
    /// the answers lately, as `echo` takes them: a chat answer has so many that "Tell me." was
    /// dropped as one (22 Nov).
    pub(super) fn tail_echo(&self, heard: &str) -> bool {
        let tail: Vec<String> = self
            .said
            .iter()
            .rev()
            .take(2)
            .filter(|(at, _)| at.elapsed() < ECHO_WINDOW)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .flat_map(|(_, text)| echo_words(text))
            .collect();
        let heard = echo_words(heard);
        if heard.is_empty() {
            return true;
        }
        // Up to four words exactly; a longer stretch may have one word misheard.
        let misheard = usize::from(heard.len() > 4);
        tail.windows(heard.len())
            .any(|window| window.iter().zip(&heard).filter(|(said, heard)| said != heard).count() <= misheard)
    }

    /// Whether words heard without the name are an answer spoken lately.
    pub(super) fn echo(&self, heard: &str) -> bool {
        let recent: Vec<&str> =
            self.said.iter().filter(|(at, _)| at.elapsed() < ECHO_WINDOW).map(|(_, text)| text.as_str()).collect();
        is_echo(&recent, heard)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Unrecognised speech after wakes in the living room (unhandled log, 12-17 October 2026).
    #[test]
    fn replies_only_to_what_sounds_like_a_request() {
        for talk in [
            "I'm not a fan of the music video.",
            "Please allow me to sing.",
            "welcome to Tune-In.",
            "it's my one bike playlist.",
            "And clap!",
            "That's for you.",
            "Peace out.",
            "I can't hear you.",
            "I'm a big brother.",
            "Good attire in one minute.",
            "Right.",
            "",
        ] {
            assert!(!sounds_like_a_request(talk), "{talk}");
        }
        for request in [
            "Why are you laughing?",
            "Set a towel for 10 minutes.",
            "Turn on the kitchen.",
            "Hey, um, what's on tonight?",
            "Please dim the hall.",
            "Kan du skru på lyset?",
        ] {
            assert!(sounds_like_a_request(request), "{request}");
        }
    }

    #[test]
    fn tells_its_own_answer_from_a_follow_up() {
        let said = ["37 seconds left on the 1 minute timer, sir."];
        assert!(is_echo(&said, "Left on the one minute timer, sir."));
        assert!(is_echo(&["Timer set for 1 minute, sir."], "One minute sir."));
        assert!(!is_echo(&said, "Stop the timer."));
        assert!(!is_echo(&["It's 20 degrees in the bedroom."], "What about the living room?"));
        assert!(!is_echo(&said, "Timer"));
        assert!(!is_echo(&["You have 2 timers. Which one?"], "The 1 minute timer."));
        let added = ["Added egg. It's on the shopping list, sir. Anything else, sir?"];
        assert!(is_echo(&added, "Than anything else."));
        assert!(is_echo(&added, "Anything else?"));
        assert!(!is_echo(&added, "Milk."));
        assert!(!is_echo(&added, "Milk and anything else."));
        assert!(!is_echo(&added, "Not anything else."));
        assert!(!is_echo(&["La til egg på handlelista. Noe mer?"], "Ikke noe mer."));
        assert!(is_echo(&["La til egg på handlelista. Noe mer?"], "Noe mer?"));
        assert!(!is_echo(&["You have 2 timers. Which one?"], "The first one."));
        assert!(!is_echo(&[], "Turn on the kitchen."));
        let wiki = "According to Wikipedia, Danilo Restivo is an Italian convicted murderer and suspected serial \
                    killer. Restivo is serving a life sentence with a 40-year tariff for murdering his neighbour \
                    Heather Barnett in Bournemouth, England, in November 2002.";
        assert!(is_echo(&[wiki], "With a 40 year tariff for murdering his barn it 40 stop"));
        assert!(!is_echo(&["Tomorrow in Oslo: heavy rain, 3 to 6 degrees."], "And what about tomorrow in Bergen?"));
    }

    #[test]
    fn commands_by_their_opening() {
        assert!(sounds_like_a_command("Turn off the sticks."));
        assert!(sounds_like_a_command("Can you please turn on the kitchen?"));
        assert!(sounds_like_a_command("Okay, play something nice."));
        assert!(!sounds_like_a_command("Tell me a joke."));
        assert!(!sounds_like_a_command("How are you feeling?"));
        assert!(!sounds_like_a_command("I feel sick."));
    }

    #[test]
    fn room_talk_in_the_follow_up() {
        assert!(room_talk("With a 40 year tariff for murdering his barn it 40 stop"));
        // The TV after a wake word without the name (Loki, 31 Oct): parsed as "volume up by 2".
        assert!(room_talk("Everyone knows that you'd mad. It weren't your fault that I is dead. Just because 2 bitches was giving you that."));
        assert!(!room_talk("And the kitchen too."));
        assert!(!room_talk("Turn on all the lights in the living room and the kitchen."));
        assert!(!room_talk("What is the temperature in the bedroom right now, please?"));
    }
}
