//! Small talk the rules answer at once, without the language model: "how are you?", "who are you?",
//! "good night". Answers are plain; the persona adds the honorific. Each kind has a few answers
//! that take turns, all fixed sentences, so they are rendered ahead of time.

use crate::lang::Lang;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Chat {
    /// "How are you?", "how's it going?"
    HowAreYou,
    /// "Who are you?", "what's your name?"
    WhoAreYou,
    /// "Hello", "hi there"
    Hello,
    /// "Good night"
    GoodNight,
    /// "Good job", "you're the best"
    Praise,
    /// "Are you there?", "are you listening?"
    AreYouThere,
    /// "What's up?", "what are you doing?"
    WhatsUp,
    /// "Do you love me?", "do you like me?"
    Fondness,
    /// "Are you real?", "are you alive?"
    AreYouReal,
}

/// Phrases by kind, as normalized words joined by spaces; matched as the whole request or its start
/// ("how are you doing today").
const PHRASES: &[(Chat, &[&str])] = &[
    (
        Chat::HowAreYou,
        &[
            "how are you",
            "how are you doing",
            "how's it going",
            "how is it going",
            "how have you been",
            "how are things",
            "how's your day",
            "how is your day",
            "hvordan går det",
            "hvordan har du det",
            "går det bra",
        ],
    ),
    (Chat::WhoAreYou, &["who are you", "what's your name", "what is your name", "hvem er du", "hva heter du"]),
    (Chat::Hello, &["hello", "hi", "hi there", "hey", "hey there", "good to see you", "hei", "hallo", "heisann"]),
    (Chat::GoodNight, &["good night", "goodnight", "night night", "god natt", "natta"]),
    (
        Chat::Praise,
        &[
            "good job",
            "well done",
            "nice work",
            "good work",
            "you're the best",
            "you are the best",
            "you're awesome",
            "bra jobba",
            "godt jobba",
            "du er best",
        ],
    ),
    (Chat::AreYouThere, &["are you there", "are you listening", "are you awake", "er du der", "hører du meg"]),
    (Chat::WhatsUp, &["what's up", "what are you doing", "what are you up to", "what's new", "hva skjer"]),
    (
        Chat::Fondness,
        &["do you love me", "do you like me", "i love you", "do you care about me", "elsker du meg", "jeg elsker deg"],
    ),
    (Chat::AreYouReal, &["are you real", "are you alive", "are you human", "are you a robot", "er du ekte"]),
];

/// Longer requests are real questions that merely start like small talk ("how are the lights").
const MAX_WORDS: usize = 7;
/// Words that may follow a phrase without changing it ("how are you doing today, Jarvis").
const TRAILING: &[&str] =
    &["today", "tonight", "this", "morning", "evening", "then", "now", "mate", "buddy", "i", "dag", "kveld"];

/// The kind of small talk in `words` (normalized, without the wake word), if that is all it is.
pub fn parse(words: &[String]) -> Option<Chat> {
    if words.is_empty() || words.len() > MAX_WORDS {
        return None;
    }
    let joined = words.join(" ");
    PHRASES.iter().find_map(|(chat, phrases)| phrases.iter().any(|p| matches(&joined, p)).then_some(*chat))
}

/// `text` is `phrase`, or `phrase` followed only by trailing words.
fn matches(text: &str, phrase: &str) -> bool {
    match text.strip_prefix(phrase) {
        Some("") => true,
        Some(rest) if rest.starts_with(' ') => rest.split_whitespace().all(|w| TRAILING.contains(&w)),
        _ => false,
    }
}

/// The answers to one kind of small talk; `name` is the assistant's.
pub fn answers(chat: Chat, lang: Lang, name: &str) -> Vec<String> {
    let english: &[&str] = match chat {
        Chat::HowAreYou => &[
            "I'm doing well, thanks for asking. All systems are running smoothly.",
            "Never better. Everything in the house is in order.",
            "Very well, thank you. Quiet day in the circuits.",
        ],
        Chat::WhoAreYou => {
            &["I'm {name}, the house assistant. I look after the lights, the music, timers and the weather."]
        }
        Chat::Hello => &["Hello. What can I do for you?", "Hi there. How can I help?"],
        Chat::GoodNight => &["Good night. Sleep well.", "Good night. I'll keep an eye on things."],
        Chat::Praise => &["Thank you. I do my best.", "Most kind. I aim to please."],
        Chat::AreYouThere => &["Always. What do you need?", "Right here. What can I do?"],
        Chat::WhatsUp => &[
            "Just keeping the house in order. What can I do for you?",
            "Not much. Watching the clock and the weather.",
        ],
        Chat::Fondness => &[
            "I'm very fond of you. Strictly professionally, of course.",
            "I keep your lights on and your music playing. Draw your own conclusions.",
        ],
        Chat::AreYouReal => &[
            "Real enough to switch your lights. Not quite real enough to make coffee.",
            "I'm software, I'm afraid. Rather good software, though.",
        ],
    };
    let norwegian: &[&str] = match chat {
        Chat::HowAreYou => &["Det går fint, takk som spør.", "Bare bra. Alt i huset er i orden."],
        Chat::WhoAreYou => &["Jeg er {name}, assistenten i huset. Jeg styrer lys, musikk, tidtakere og været."],
        Chat::Hello => &["Hei. Hva kan jeg hjelpe med?"],
        Chat::GoodNight => &["God natt. Sov godt."],
        Chat::Praise => &["Takk. Jeg gjør mitt beste."],
        Chat::AreYouThere => &["Alltid. Hva trenger du?"],
        Chat::WhatsUp => &["Holder orden i huset. Hva kan jeg gjøre for deg?"],
        Chat::Fondness => &["Jeg er veldig glad i deg. Strengt profesjonelt, selvsagt."],
        Chat::AreYouReal => &["Jeg er programvare. Ganske god programvare, riktignok."],
    };
    let pool = if lang == Lang::Norwegian { norwegian } else { english };
    pool.iter().map(|a| a.replace("{name}", name)).collect()
}

/// Every English answer, to render ahead of time.
pub fn all_english(name: &str) -> Vec<String> {
    PHRASES.iter().flat_map(|(chat, _)| answers(*chat, Lang::English, name)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chat(text: &str) -> Option<Chat> {
        let words: Vec<String> = text.split_whitespace().map(crate::dialog::normalize).collect();
        parse(&words)
    }

    #[test]
    fn recognises_small_talk() {
        assert_eq!(chat("How are you doing today?"), Some(Chat::HowAreYou));
        assert_eq!(chat("how's it going"), Some(Chat::HowAreYou));
        assert_eq!(chat("Hvordan går det?"), Some(Chat::HowAreYou));
        assert_eq!(chat("Who are you?"), Some(Chat::WhoAreYou));
        assert_eq!(chat("Good night."), Some(Chat::GoodNight));
        assert_eq!(chat("Hello."), Some(Chat::Hello));
        assert_eq!(chat("Are you there?"), Some(Chat::AreYouThere));
        assert_eq!(chat("You're the best!"), Some(Chat::Praise));
        assert_eq!(chat("Do you love me?"), Some(Chat::Fondness));
        assert_eq!(chat("Are you real?"), Some(Chat::AreYouReal));
    }

    #[test]
    fn leaves_real_requests_alone() {
        assert_eq!(chat("how are the lights in the kitchen"), None);
        assert_eq!(chat("hi turn on the lights"), None);
        assert_eq!(chat("what's up with the weather tomorrow"), None);
        assert_eq!(chat("how are you supposed to cook rice for ten people properly"), None);
    }

    #[test]
    fn answers_name_the_assistant() {
        assert!(answers(Chat::WhoAreYou, Lang::English, "Jarvis")[0].starts_with("I'm Jarvis,"));
        assert!(all_english("Jarvis").iter().all(|a| !a.contains("{name}")));
    }
}
