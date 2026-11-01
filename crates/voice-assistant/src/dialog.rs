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
    (
        "jarvis",
        &[
            "jarvis", "jarvis's", "jarvis'", "jarris", "jervis", "jarvus", "jarviss", "jarves", "javis", "jaris",
            "jarvas", "jarviz", "jarvice", "jarbis", "charvis", "jervis's", "darvis", "jargis", "ajaris",
        ],
    ),
    ("homie", &["homie", "homies", "homie's", "homey", "homy", "homi", "hommie", "homee", "homeie"]),
    (
        "freya",
        &[
            "freya", "freyas", "freya's", "freja", "freia", "freyja", "fraya", "freyah", "frøya", "froya", "frea",
            "frela", "frala", "friar", "freyer", "freyr", "fria", "threa", "fraia",
        ],
    ),
];

/// Everyday words a transcriber writes for the name in a wake word ("Hey German."). They count only
/// right after a greeting and only in the wake word's own audio, which the wake word model already
/// scored as a near miss.
const AFTER_GREETING: &[(&str, &[&str])] = &[("jarvis", &["german", "germans", "drivers", "doris", "james"])];

/// The assistant's name as the transcript shows it: the name itself plus known spellings.
#[derive(Clone, Debug)]
pub struct WakeWord {
    name: String,
    spellings: Vec<String>,
    after_greeting: &'static [&'static str],
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
        let after_greeting = AFTER_GREETING.iter().find(|(k, _)| *k == key).map_or(&[][..], |(_, words)| *words);
        Self { name: name.to_owned(), spellings, after_greeting }
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

    /// Whether the transcript of a wake word's audio holds the name: as anywhere else, or as one of
    /// the everyday words it is heard as right after a greeting.
    pub fn in_wake_word(&self, text: &str) -> bool {
        let words: Vec<String> = text.split_whitespace().map(normalize).collect();
        words.iter().any(|w| self.matches(w))
            || words.windows(2).any(|pair| is_greeting(&pair[0]) && self.after_greeting.contains(&pair[1].as_str()))
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
        let rest = words[at + 1..].join(" ").trim_start_matches([',', '.', '!', '?', ' ']).to_owned();
        // "Good morning, Jarvis" and "good night Jarvis" are requests said before the name.
        let before: Vec<String> = words[..at].iter().map(|w| normalize(w)).collect();
        let greets = matches!(
            before.iter().map(String::as_str).collect::<Vec<_>>().as_slice(),
            ["good", "morning" | "evening" | "afternoon" | "night"] | ["god", "morgen" | "kveld" | "natt"]
        );
        if rest.is_empty() && greets {
            return Some(words[..at].join(" ").trim_end_matches([',', '.', '!', '?']).to_owned());
        }
        Some(rest)
    }

    /// The request without the name said at its end: "what time is it, Jarvis" is "what time is it".
    pub fn drop_trailing(&self, text: &str) -> String {
        let words: Vec<&str> = text.split_whitespace().collect();
        // "Hva er været på Frøya" with the name Freya: after a preposition the word is a place or
        // a thing, unless a comma sets the name apart.
        let after_preposition = words.len() >= 2 && {
            let before = words[words.len() - 2];
            !before.ends_with(',')
                && [
                    "på", "i", "in", "at", "to", "til", "from", "fra", "for", "of", "av", "near", "ved", "about", "om",
                    "with", "med",
                ]
                .contains(&normalize(before).as_str())
        };
        match words.split_last() {
            Some((last, rest)) if !rest.is_empty() && !after_preposition && self.matches(&normalize(last)) => {
                let tail = last.trim_start_matches(|c: char| c.is_alphanumeric() || c == '\'');
                let rest = rest.join(" ");
                format!("{}{tail}", rest.trim_end_matches([',', ' ']))
            }
            _ => text.to_owned(),
        }
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
        for heard in [
            "Hey Jarvis.",
            "Hey Jarris.",
            "Hey Jervis.",
            "Kate Jarvis.",
            "Hey, Jarvis, play.",
            "heyjarvis",
            "Hey Jaris.",
        ] {
            assert!(heard.split_whitespace().any(|w| wake.matches(&normalize(w))), "{heard}");
        }
        assert!(!"That's my parents.".split_whitespace().any(|w| wake.matches(&normalize(w))));
    }

    /// Wake words Parakeet heard as everyday words (Loki, 27-31 Oct).
    #[test]
    fn hears_jarvis_in_a_wake_word() {
        let wake = WakeWord::new("Jarvis");
        for heard in [
            "Hey German.",
            "Hey Germans.",
            "Hey drivers!",
            "Hey Darvis.",
            "Hey, Jarvis.",
            "Hey Jargis.",
            "Ajaris",
            "Hey Doris.",
            "Hey James.",
        ] {
            assert!(wake.in_wake_word(heard), "{heard}");
        }
        for heard in ["German.", "The drivers are late.", "Doris called.", "Yeah.", "", "Hey there."] {
            assert!(!wake.in_wake_word(heard), "{heard}");
        }
        // Elsewhere they are the words they are.
        assert_eq!(wake.strip("Hey German, how are you?"), None);
        assert!(!WakeWord::new("Homie").in_wake_word("Hey German."));
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
        assert_eq!(strip_wake("Good morning, Freya.").as_deref(), Some("Good morning"));
        assert_eq!(strip_wake("Good night Freya").as_deref(), Some("Good night"));
        assert_eq!(strip_wake("Hey Freya.").as_deref(), Some(""));
        let freya = WakeWord::new("Freya");
        assert_eq!(freya.drop_trailing("What time is it now, please, Freya?"), "What time is it now, please?");
        assert_eq!(freya.drop_trailing("Turn off the lights Freya"), "Turn off the lights");
        assert_eq!(freya.drop_trailing("Freya"), "Freya");
        assert_eq!(freya.drop_trailing("Tell me a joke"), "Tell me a joke");
        // After a preposition it is a place, unless a comma sets the name apart.
        assert_eq!(freya.drop_trailing("Hva er været på Frøya"), "Hva er været på Frøya");
        assert_eq!(freya.drop_trailing("What's the weather in Oslo, Freya"), "What's the weather in Oslo");
        assert_eq!(freya.drop_trailing("Hva er været på, Freya"), "Hva er været på");
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
