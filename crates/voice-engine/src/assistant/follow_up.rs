//! What is heard without the name: the room talking, and the answer coming back from the speaker.

use std::collections::HashSet;
use std::time::Duration;

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
    "remember",
    "forget",
    "note",
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
    "husk",
    "glem",
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
    if said.last().is_none_or(|last| last.trim_end().ends_with('?')) {
        return false;
    }
    let said: HashSet<String> = said.iter().flat_map(|text| echo_words(text)).collect();
    let heard = echo_words(heard);
    let new = heard.iter().filter(|w| !said.contains(*w)).count();
    // A long stretch of a long answer comes back garbled ("Barnett" as "barn it"), but mostly in its
    // words; nobody asks a follow-up of that many words made of the answer's.
    let garbled = heard.len() >= ECHO_LONG && new * 4 <= heard.len();
    heard.len() >= 2 && (new <= 1 && new * 5 <= heard.len() || garbled)
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
        assert!(!is_echo(&[], "Turn on the kitchen."));
        let wiki = "According to Wikipedia, Danilo Restivo is an Italian convicted murderer and suspected serial \
                    killer. Restivo is serving a life sentence with a 40-year tariff for murdering his neighbour \
                    Heather Barnett in Bournemouth, England, in November 2002.";
        assert!(is_echo(&[wiki], "With a 40 year tariff for murdering his barn it 40 stop"));
        assert!(!is_echo(&["Tomorrow in Oslo: heavy rain, 3 to 6 degrees."], "And what about tomorrow in Bergen?"));
    }

    #[test]
    fn room_talk_in_the_follow_up() {
        assert!(room_talk("With a 40 year tariff for murdering his barn it 40 stop"));
        // The TV after a wake word without the name (Loki, 31 Oct): parsed as "volume up by 2".
        assert!(room_talk("Everyone knows that you'd mad. It weren't your fault that I is dead. Just because 2 bitches was giving you that."));
        assert!(!room_talk("And the kitchen too."));
        assert!(!room_talk("Turn on all the lights in the living room and the kitchen."));
        assert!(!room_talk("What is the temperature in the bedroom right now, please?"));
        // A note is a long sentence of its own.
        assert!(!room_talk("Remember that the spare key is in the kitchen drawer by the sink."));
        assert!(!room_talk("Husk at reservenøkkelen ligger i den øverste skuffen på kjøkkenet."));
    }
}
