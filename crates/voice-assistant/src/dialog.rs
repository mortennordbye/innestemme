//! The wake word: the assistant's name (a name, "Homie" by default) as transcribers write it, and
//! the request after it.

/// Lower case, letters, digits and apostrophes only.
pub fn normalize(word: &str) -> String {
    word.chars().filter(|c| c.is_alphanumeric() || *c == '\'').flat_map(char::to_lowercase).collect()
}

/// Ways transcribers write known names. An explicit list rather than an edit distance, which would
/// also match common words. For Freya, "fredag" (Friday) and "fela" (fiddle) are close
/// mishearings but everyday Norwegian, so they are left out; for Homie, so is "home".
const SPELLINGS: &[(&str, &[&str])] = &[
    ("jarvis", &["jarvis", "jarvis's", "jarvis'", "jarris", "jervis", "jarvus", "jarviss", "jarves", "javis"]),
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

    /// Parakeet on the living room Voice PE's wake words (`examples/wake_text.rs`).
    #[test]
    fn hears_jarvis_as_parakeet_writes_it() {
        let wake = WakeWord::new("Jarvis");
        for heard in ["Hey Jarvis.", "Hey Jarris.", "Hey Jervis.", "Kate Jarvis.", "Hey, Jarvis, play.", "heyjarvis"] {
            assert!(heard.split_whitespace().any(|w| wake.matches(&normalize(w))), "{heard}");
        }
        assert!(!"That's my parents.".split_whitespace().any(|w| wake.matches(&normalize(w))));
    }

    /// Whether a phrase holds the name, word by word as the engine checks a transcript.
    fn wakes(wake: &WakeWord, phrase: &str) -> bool {
        phrase.split_whitespace().any(|w| wake.matches(&normalize(w)))
    }

    fn strip_wake(text: &str) -> Option<String> {
        WakeWord::new("Freya").strip(text)
    }

    #[test]
    fn wakes_on_freya_alone_or_after_a_greeting() {
        let freya = WakeWord::new("Freya");
        for phrase in ["Freya,", "freya", "hey Freja", "hei Freia", "Frøya.", "okay Freyja", "hi Freya", "hei, Freya"]
        {
            assert!(wakes(&freya, phrase), "{phrase}");
        }
        for phrase in ["free", "Frey", "hey there", "fresh", "Eden"] {
            assert!(!wakes(&freya, phrase), "{phrase}");
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
        assert!(wakes(&homie, "hey Homie"));
    }

    #[test]
    fn any_name_works_without_a_spelling_table() {
        let zelda = WakeWord::new("Zelda");
        assert_eq!(zelda.strip("Zelda, tell me a joke.").as_deref(), Some("tell me a joke."));
        assert_eq!(zelda.strip("Homie, tell me a joke."), None);
    }
}
