//! Turn logic on top of the transcript: wait for the wake word (a name, "Homie" by default), then
//! collect the request until the speaker pauses.

use std::collections::VecDeque;

/// 80 ms frames.
const FRAMES_PER_SECOND: u64 = 12;
/// End of request when no new word arrived for this long, if the pause head has not fired first.
const SILENCE_END: u64 = 2 * FRAMES_PER_SECOND;
/// Give up when nothing is said this long after the wake word.
const NOTHING_SAID: u64 = 6 * FRAMES_PER_SECOND;
const PAUSE_THRESHOLD: f32 = 0.5;
/// The pause head can fire while the last word is still held back by the STT delay (a word is
/// emitted only when the pause after it is seen), so it only ends a request after this much
/// quiet in the transcript as well.
const PAUSE_MIN_QUIET: u64 = 8;

#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    Wake,
    Request(String),
    /// Woken, but nothing followed.
    GaveUp,
}

enum State {
    Idle { recent: VecDeque<String> },
    Listening { words: Vec<String>, since: u64, last_word: u64 },
}

pub struct Dialog {
    state: State,
    frame: u64,
    wake: WakeWord,
}

impl Default for Dialog {
    fn default() -> Self {
        Self::new(WakeWord::default())
    }
}

impl Dialog {
    pub fn new(wake: WakeWord) -> Self {
        Self { state: State::Idle { recent: VecDeque::new() }, frame: 0, wake }
    }

    pub fn is_listening(&self) -> bool {
        matches!(self.state, State::Listening { .. })
    }

    pub fn reset(&mut self) {
        self.state = State::Idle { recent: VecDeque::new() };
    }

    /// One call per frame with the words that frame produced. At most one wake and one request
    /// per frame; the wake comes first when both happen.
    pub fn step(&mut self, words: &[String], pause: Option<f32>) -> Vec<Action> {
        self.frame += 1;
        let now = self.frame;
        let mut actions = Vec::new();
        // One STT "word" can hold several words.
        for word in words.iter().flat_map(|w| w.split_whitespace()).map(normalize).filter(|w| !w.is_empty()) {
            match &mut self.state {
                State::Idle { recent } => {
                    recent.push_back(word);
                    if recent.len() > 2 {
                        recent.pop_front();
                    }
                    if recent.back().is_some_and(|w| self.wake.matches(w)) {
                        actions.push(Action::Wake);
                        self.state = State::Listening { words: Vec::new(), since: now, last_word: now };
                    }
                }
                State::Listening { words, last_word, .. } => {
                    words.push(word);
                    *last_word = now;
                }
            }
        }
        if let State::Listening { words, since, last_word } = &mut self.state {
            let quiet = now - *last_word;
            let paused =
                (pause.is_some_and(|p| p > PAUSE_THRESHOLD) && quiet >= PAUSE_MIN_QUIET) || quiet >= SILENCE_END;
            if !words.is_empty() && paused {
                actions.push(Action::Request(words.join(" ")));
                self.reset();
            } else if words.is_empty() && now - *since >= NOTHING_SAID {
                actions.push(Action::GaveUp);
                self.reset();
            }
        }
        actions
    }
}

/// Lower case, letters, digits and apostrophes only.
pub fn normalize(word: &str) -> String {
    word.chars().filter(|c| c.is_alphanumeric() || *c == '\'').flat_map(char::to_lowercase).collect()
}

/// Ways transcribers write known names. An explicit list rather than an edit distance, which would
/// also match common words. For Freya, "fredag" (Friday) and "fela" (fiddle) are close
/// mishearings but everyday Norwegian, so they are left out; for Homie, so is "home".
const SPELLINGS: &[(&str, &[&str])] = &[
    ("homie", &["homie", "homies", "homie's", "homey", "homy", "homi", "hommie", "homee", "homeie"]),
    (
        "freya",
        &[
            "freya", "freyas", "freya's", "freja", "freia", "freyja", "fraya", "freyah", "frøya", "froya", "frea",
            "frela", "frala", "friar", "freyer", "freyr", "fria", "threa", "fraia",
        ],
    ),
];

/// The assistant's name as the transcript shows it: the name itself plus known spellings.
#[derive(Clone, Debug)]
pub struct WakeWord {
    name: String,
    spellings: Vec<String>,
}

impl Default for WakeWord {
    fn default() -> Self {
        Self::new("Homie")
    }
}

impl WakeWord {
    pub fn new(name: &str) -> Self {
        let key = normalize(name);
        let mut spellings = vec![key.clone()];
        if let Some((_, known)) = SPELLINGS.iter().find(|(k, _)| *k == key) {
            spellings.extend(known.iter().map(|s| s.to_string()));
        }
        Self { name: name.to_owned(), spellings }
    }

    /// Adds ways a transcriber writes the name, beyond the built-in ones.
    pub fn with_spellings(mut self, extra: &[String]) -> Self {
        for spelling in extra.iter().map(|s| normalize(s)).filter(|s| !s.is_empty()) {
            if !self.spellings.contains(&spelling) {
                self.spellings.push(spelling);
            }
        }
        self
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// `word` is normalized. Also true for greeting and name written as one word ("heyhomie").
    pub fn matches(&self, word: &str) -> bool {
        let is_name = |w: &str| self.spellings.iter().any(|s| s == w);
        is_name(word)
            || ["hey", "hei", "hi", "hai", "hallo", "hello"].iter().any(|g| word.strip_prefix(g).is_some_and(is_name))
    }

    /// For a whole transcribed utterance: `Some(request)` when it starts with the name, possibly
    /// after a greeting ("hey", "hei", ...). The request is empty when only the name was said.
    pub fn strip(&self, text: &str) -> Option<String> {
        let words: Vec<&str> = text.split_whitespace().collect();
        // The name within the first few words. Before it: greetings, or at most two other words,
        // which covers a misheard first try ("Hey, from. Hey, Homie!") without waking on the name
        // mid-sentence.
        let at = words.iter().take(5).position(|w| self.matches(&normalize(w)))?;
        let others = words[..at].iter().filter(|w| !is_greeting(&normalize(w))).count();
        if others > 2 {
            return None;
        }
        Some(words[at + 1..].join(" ").trim_start_matches([',', '.', '!', '?', ' ']).to_owned())
    }
}

/// Words that may come before the name: "hei Freya", "hi Freya", "god morgen Freya".
pub fn is_greeting(word: &str) -> bool {
    matches!(
        word,
        "hey"
            | "hei"
            | "hi"
            | "high"
            | "hai"
            | "hay"
            | "heia"
            | "heisann"
            | "hallo"
            | "hello"
            | "ok"
            | "okay"
            | "god"
            | "good"
            | "morgen"
            | "morning"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(text: &str) -> Vec<String> {
        text.split_whitespace().map(str::to_owned).collect()
    }

    fn freya() -> Dialog {
        Dialog::new(WakeWord::new("Freya"))
    }

    fn strip_wake(text: &str) -> Option<String> {
        WakeWord::new("Freya").strip(text)
    }

    #[test]
    fn wake_then_request_ends_on_pause_head() {
        let mut d = freya();
        assert!(d.step(&words("so anyway"), None).is_empty());
        assert_eq!(d.step(&words("Hey, Freya."), None), vec![Action::Wake]);
        assert!(d.step(&["what's the weather".into()], Some(0.1)).is_empty());
        assert!(d.step(&words("in"), Some(0.9)).is_empty());
        assert!(d.step(&words("Oslo?"), Some(0.9)).is_empty());
        let actions: Vec<_> = (0..PAUSE_MIN_QUIET).flat_map(|_| d.step(&[], Some(0.9))).collect();
        assert_eq!(actions, vec![Action::Request("what's the weather in oslo".into())]);
        assert!(!d.is_listening());
    }

    #[test]
    fn wake_and_request_in_one_breath_end_on_silence() {
        let mut d = freya();
        let mut actions = d.step(&words("Freya, what is the weather in bergen"), None);
        for _ in 0..SILENCE_END {
            actions.extend(d.step(&[], None));
        }
        assert_eq!(actions, vec![Action::Wake, Action::Request("what is the weather in bergen".into())]);
    }

    #[test]
    fn wakes_on_freya_alone_or_after_a_greeting() {
        for phrase in ["Freya,", "freya", "hey Freja", "hei Freia", "Frøya.", "okay Freyja", "hi Freya", "hei, Freya"]
        {
            let mut d = freya();
            assert_eq!(d.step(&words(phrase), None), vec![Action::Wake], "{phrase}");
        }
        for phrase in ["free", "Frey", "hey there", "fresh", "Eden"] {
            let mut d = freya();
            assert!(d.step(&words(phrase), None).is_empty(), "{phrase}");
        }
    }

    #[test]
    fn strips_the_wake_word_from_a_transcript() {
        assert_eq!(strip_wake("Freya, hvordan blir været i Oslo?").as_deref(), Some("hvordan blir været i Oslo?"));
        assert_eq!(strip_wake("Hei Freia. Hva er klokka?").as_deref(), Some("Hva er klokka?"));
        assert_eq!(strip_wake("Freya.").as_deref(), Some(""));
        assert_eq!(strip_wake("We sailed past Frøya yesterday"), None);
        assert_eq!(strip_wake("Hey, from. Hey, Freya! Tell me a joke.").as_deref(), Some("Tell me a joke."));
        assert_eq!(strip_wake("I told my sister Freya about it"), None);
        assert_eq!(strip_wake("Hi Freya, what's the weather?").as_deref(), Some("what's the weather?"));
        assert_eq!(strip_wake("Hei, Freya. Hvordan er været?").as_deref(), Some("Hvordan er været?"));
        assert_eq!(strip_wake("Hei hei Freja, hvordan er været?").as_deref(), Some("hvordan er været?"));
        assert_eq!(strip_wake("God morgen Freya, hvordan er været?").as_deref(), Some("hvordan er været?"));
        assert_eq!(strip_wake("Heifreya, hvordan er været?").as_deref(), Some("hvordan er været?"));
        assert_eq!(strip_wake("Hi."), None);
    }

    #[test]
    fn homie_is_the_default_name() {
        let homie = WakeWord::default();
        assert_eq!(homie.name(), "Homie");
        assert_eq!(homie.strip("Homie, turn off the lights.").as_deref(), Some("turn off the lights."));
        assert_eq!(homie.strip("Hey homey, tell me a joke").as_deref(), Some("tell me a joke"));
        assert_eq!(homie.strip("Heyhomie what's the weather").as_deref(), Some("what's the weather"));
        assert_eq!(homie.strip("Home, turn off the lights."), None);
        assert_eq!(homie.strip("Freya, tell me a joke"), None);
        let mut d = Dialog::default();
        assert_eq!(d.step(&words("hey Homie"), None), vec![Action::Wake]);
    }

    #[test]
    fn any_name_works_without_a_spelling_table() {
        let zelda = WakeWord::new("Zelda");
        assert_eq!(zelda.strip("Zelda, tell me a joke.").as_deref(), Some("tell me a joke."));
        assert_eq!(zelda.strip("Homie, tell me a joke."), None);
    }

    #[test]
    fn gives_up_when_nothing_follows() {
        let mut d = freya();
        d.step(&words("hey freya"), None);
        let actions: Vec<_> = (0..NOTHING_SAID).flat_map(|_| d.step(&[], Some(0.9))).collect();
        assert_eq!(actions, vec![Action::GaveUp]);
    }
}
