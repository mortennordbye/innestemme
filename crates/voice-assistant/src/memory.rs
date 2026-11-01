//! Personal notes: "remember that the spare key is in the kitchen drawer", then "where is the spare
//! key?", "what did I say about the spare key?", "what do you remember?", "forget the spare key".
//! Kept in a file on the models volume, like the timers, so a deploy keeps them. Passwords and
//! codes are not kept: anyone in the room can ask for a note.
//!
//! The rules run before most skills, since a note holds any words ("remember that the oven timer
//! is broken"), so each form is narrow: a note is a sentence after "remember that", and forgetting
//! never takes a timer, an alarm or the shopping list.

use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::dialog::normalize;
use crate::lang::Lang;

/// Notes kept at most. Answers read them aloud, so a long list is no use; the cap also bounds the
/// file.
pub const MAX_NOTES: usize = 50;
/// Notes read out by "what do you remember?"; the rest are counted.
const READ_OUT: usize = 5;
/// How long "Remember what?" and the questions before forgetting wait for their answer, like the
/// engine's other questions.
const ASK_WINDOW: Duration = Duration::from_secs(30);

/// The fixed English answers a sentence at a time, rendered ahead by the speech cache. The notes
/// themselves are never cached.
pub const ENGLISH_ANSWERS: &[&str] = &[
    "Okay, I'll remember that.",
    "Okay, I've updated that.",
    "I don't keep passwords or codes.",
    "Anyone in the room could ask me for them.",
    "My notes are full.",
    "Forget something first.",
    "Remember what?",
    "I don't have a note about that.",
    "I don't remember anything yet.",
    "I remember one thing.",
    "Okay, I've forgotten that.",
    "Forget everything I remember?",
    "Forget all of them?",
    "Okay, I've forgotten everything.",
    "Okay, I'll keep it all.",
    "Okay, I'll keep them.",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemoryCommand {
    /// "Remember that the spare key is in the kitchen drawer": the sentence to keep. Empty for
    /// "remember this", which asks what.
    Remember(String),
    /// "What did I say about the spare key?", "where did I put my keys?": what it is about.
    Recall(String),
    /// "What do you remember?"
    List,
    /// "Forget the spare key".
    Forget(String),
    /// "Forget everything": asked first.
    ForgetAll,
}

/// Asking for a note's subject: what follows them is the subject.
const ABOUT: &[&str] = &["about", "om"];
/// Words with no meaning of their own in a subject: "the spare key" is the spare key.
const FILLER: &[&str] = &[
    "the", "a", "an", "my", "our", "your", "his", "her", "their", "its", "some", "that", "this", "these", "those",
    "it", "me", "i", "you", "we", "to", "of", "for", "please", "again", "now", "min", "mi", "mitt", "mine", "vår",
    "vårt", "våre", "din", "di", "ditt", "dine", "en", "ei", "et", "den", "det", "de", "jeg", "du", "vi", "meg", "deg",
    "oss", "til", "av", "takk", "igjen", "nå", "is", "are", "er", "was", "var", "put", "left", "la", "lagt", "satte",
    "where", "what", "hvor", "hva", "did", "do", "say", "said", "tell", "told", "sa", "sagt", "fortalte", "about",
    "om", "leave", "hide", "gjemte", "have", "har",
];
/// Politeness before the request: "can you remember that ...", "kan du huske at ...".
const LEAD: &[&str] =
    &["can", "could", "would", "will", "you", "please", "just", "okay", "ok", "hey", "kan", "du", "vil", "så"];
/// Words of other skills' things: "forget the timer" is about the timer, not a note.
const OTHER_SKILLS: &[&str] = &[
    "timer",
    "timers",
    "timeren",
    "timerne",
    "alarm",
    "alarms",
    "alarmen",
    "reminder",
    "reminders",
    "påminnelse",
    "påminnelsen",
    "påminnelsene",
    "list",
    "lists",
    "liste",
    "listen",
    "lista",
    "handleliste",
    "handlelisten",
    "handlelista",
    "shopping",
    "vekkerklokke",
    "vekkerklokka",
    "vekkerklokken",
    "nedtelling",
    "nedtellingen",
];
/// "Remember my keys are ...": a sentence of its own after "remember" needs one of these.
const COPULA: &[&str] = &["is", "are", "was", "were", "er", "var"];
/// What starts the thing a note is about after "remember": "remember MY bike is in the shed".
const DETERMINERS: &[&str] = &["my", "the", "our", "min", "mi", "mitt", "mine", "vår", "vårt", "våre"];

/// "Remember that the spare key is in the kitchen drawer", "what did I say about the spare key",
/// "what do you remember", "forget the spare key", "forget everything", "where did I put my keys",
/// and the Norwegian: "husk at reservenøkkelen ligger i skuffen", "hva sa jeg om reservenøkkelen",
/// "hva husker du", "glem reservenøkkelen", "glem alt", "hvor la jeg nøklene".
pub fn parse(text: &str) -> Option<MemoryCommand> {
    let tokens: Vec<&str> = text.split_whitespace().collect();
    let words: Vec<String> = tokens.iter().map(|t| normalize(t)).collect();
    let start = words.iter().position(|w| !w.is_empty() && !LEAD.contains(&w.as_str()))?;
    let w: Vec<&str> = words[start..].iter().map(String::as_str).collect();
    let rest = |from: usize| tokens[start + from..].join(" ");
    let note = |from: usize| {
        let said = rest(from);
        is_statement(&said).then_some(MemoryCommand::Remember(said))
    };
    match w.as_slice() {
        ["remember" | "husk" | "huske", "that" | "at", ..] => note(2),
        ["make", "a" | "an", "note", "that", ..] => note(4),
        ["remember" | "husk" | "huske", "this" | "dette" | "something" | "noe"] => {
            Some(MemoryCommand::Remember(String::new()))
        }
        // "Remember my bike is in the shed": a sentence about a thing, said with "is". Not "remember
        // the milk", "remember the time we ...".
        ["remember" | "husk" | "huske", first, ..]
            if DETERMINERS.contains(first) && w.iter().skip(2).any(|x| COPULA.contains(x)) =>
        {
            note(1)
        }
        ["forget" | "glem", next, ..] => {
            // "Forget it", "forget about it, turn off the lights": never mind, which the rules answer.
            let then = w.get(2).copied().unwrap_or_default();
            let never_mind = ["it", "that", "this", "det", "dette"].contains(next)
                || (*next == "about" && ["it", "that", "this"].contains(&then));
            if never_mind || w.iter().any(|x| OTHER_SKILLS.contains(x)) {
                return None;
            }
            match &w[1..] {
                ["everything" | "alt"]
                | ["everything" | "alt", "you" | "du", "remember" | "know" | "husker" | "vet"]
                | ["all" | "alle", "the" | "my", "notes"]
                | ["alle", "notatene"] => Some(MemoryCommand::ForgetAll),
                _ => {
                    let subject = strip_about(&rest(1));
                    (!subject_words(&subject).is_empty()).then_some(MemoryCommand::Forget(subject))
                }
            }
        }
        ["what" | "what's" | "whats", "do" | "did", "you", "remember", rest @ ..]
        | ["hva", "husker", "du", rest @ ..]
        | ["what", "have", "you", "remembered" | "noted", rest @ ..] => {
            let skip = w.len() - rest.len();
            match rest.iter().position(|w| ABOUT.contains(w)) {
                Some(i) => Some(MemoryCommand::Recall(rest_from(&tokens, start + skip + i + 1))),
                None if rest.iter().all(|w| ["now", "nå", "so", "far"].contains(w)) => Some(MemoryCommand::List),
                None => None,
            }
        }
        // "What did I say about the spare key", "what did I tell you about the car", "hva sa jeg om
        // bilen". Not "what did you say about the weather", which asks to hear an answer again.
        ["what" | "hva", ..] => {
            let about = w.iter().position(|x| ABOUT.contains(x))?;
            let before = &w[..about];
            let said = before.iter().any(|x| ["say", "tell", "told", "said", "sa", "sagt", "fortalt"].contains(x))
                && before.iter().any(|x| ["i", "jeg"].contains(x));
            (said && about + 1 < w.len()).then(|| MemoryCommand::Recall(rest(about + 1)))
        }
        // From the verb on: "where did I park" is about parking.
        ["where", "did", "i", "put" | "leave" | "park" | "hide", ..] => Some(MemoryCommand::Recall(rest(3))),
        ["hvor", "la" | "satte" | "gjemte" | "parkerte", "jeg", ..] => Some(MemoryCommand::Recall(rest(1))),
        ["hvor", "har", "jeg", "lagt" | "satt" | "gjemt" | "parkert", ..] => Some(MemoryCommand::Recall(rest(3))),
        _ => None,
    }
}

fn rest_from(tokens: &[&str], from: usize) -> String {
    tokens.get(from..).map(|t| t.join(" ")).unwrap_or_default()
}

/// A note is a statement: "the spare key is in the drawer", "I parked on level 3". Not "that song".
fn is_statement(text: &str) -> bool {
    let words: Vec<String> = text.split_whitespace().map(normalize).filter(|w| !w.is_empty()).collect();
    let verb = words.iter().any(|w| {
        let w = w.trim_end_matches("'s");
        VERBS.contains(&w) || PUT.contains(&w)
    }) || words.iter().any(|w| w.ends_with("'s") && w.len() > 2);
    let person = words.first().is_some_and(|w| ["i", "we", "jeg", "vi", "i'm", "we're"].contains(&w.as_str()));
    words.len() >= 2 && (verb || person)
}

/// "About the spare key" is the spare key.
fn strip_about(subject: &str) -> String {
    let words: Vec<&str> = subject.split_whitespace().collect();
    match words.first().map(|w| normalize(w)) {
        Some(w) if ABOUT.contains(&w.as_str()) => words[1..].join(" "),
        _ => subject.to_owned(),
    }
}

/// What a "where is" question asks about, for a note: "where is the spare key" is the spare key,
/// "where are my glasses" the glasses, "hvor er reservenøkkelen" the spare key. `None` for other
/// questions.
pub fn where_subject(text: &str) -> Option<String> {
    let tokens: Vec<&str> = text.split_whitespace().collect();
    let words: Vec<String> = tokens.iter().map(|t| normalize(t)).collect();
    let start = words.iter().position(|w| !w.is_empty() && !LEAD.contains(&w.as_str()))?;
    let w: Vec<&str> = words[start..].iter().map(String::as_str).collect();
    let from = match w.as_slice() {
        ["where's" | "wheres", ..] => 1,
        ["where", "is" | "are" | "was" | "were", ..] | ["hvor", "er" | "ligger" | "står" | "henger" | "var", ..] => 2,
        _ => return None,
    };
    let subject = tokens[start + from..].join(" ");
    (!subject_words(&subject).is_empty()).then_some(subject)
}

/// A word as it is compared: "keys" is "key", "reservenøkkelen" is "reservenøkkel", "parked" is
/// "park", so a note matches however the question bends the word.
fn stem(word: &str) -> String {
    let word = normalize(word);
    let word = word.trim_end_matches("'s").trim_end_matches('\'');
    for ending in ["ene", "ane", "ing", "ed", "en", "et", "er", "s", "a"] {
        if let Some(root) = word.strip_suffix(ending) {
            if root.chars().count() >= 3 {
                return root.to_owned();
            }
        }
    }
    word.to_owned()
}

/// The words of `text` that say what it is about, stemmed.
fn subject_words(text: &str) -> Vec<String> {
    text.split_whitespace()
        .map(normalize)
        .filter(|w| !w.is_empty() && !FILLER.contains(&w.as_str()))
        .map(|w| stem(&w))
        .collect()
}

/// The same word, or one close enough for a misheard or differently bent one.
fn same_word(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    let (short, long) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    let (short_len, long_len) = (short.chars().count(), long.chars().count());
    // "Glas" and "glasse" (glass, glasses); "reserve" and "reservenøkkel", a Norwegian compound
    // named by its first part.
    (short_len >= 4 && long.starts_with(short) && (long_len - short_len <= 2 || short_len >= 5))
        || (short_len >= 5 && crate::names::levenshtein(a, b) <= 1)
}

/// Every word asked about is among `words`.
fn covers(asked: &[String], words: &[String]) -> bool {
    !asked.is_empty() && asked.iter().all(|q| words.iter().any(|w| same_word(q, w)))
}

/// Verbs that end the subject of a note: "the spare key IS in the drawer".
const VERBS: &[&str] = &[
    "is", "are", "was", "were", "sit", "sits", "live", "lives", "hang", "hangs", "stay", "stays", "go", "goes", "come",
    "comes", "open", "opens", "close", "closes", "belong", "belongs", "start", "starts", "end", "ends", "cost",
    "costs", "need", "needs", "has", "have", "er", "var", "ligger", "står", "henger", "sitter", "bor", "har", "går",
    "kommer", "åpner", "stenger", "starter", "slutter", "koster", "trenger",
];
/// "I put the spare key in the drawer", "jeg la nøkkelen i skuffen": the thing is after the verb.
const PUT: &[&str] = &[
    "put", "left", "parked", "hid", "stored", "keep", "keeps", "placed", "la", "satte", "parkerte", "gjemte", "hengte",
    "lagt", "satt",
];
/// Words that end the thing put somewhere: "the key IN the drawer".
const PLACES: &[&str] =
    &["in", "on", "under", "behind", "at", "by", "next", "inside", "beside", "i", "på", "bak", "ved", "inni", "oppå"];
/// Words that make a note about when rather than where: "the dentist is on Friday".
const WHEN: &[&str] = &[
    "monday",
    "tuesday",
    "wednesday",
    "thursday",
    "friday",
    "saturday",
    "sunday",
    "mondays",
    "tuesdays",
    "wednesdays",
    "thursdays",
    "fridays",
    "saturdays",
    "sundays",
    "today",
    "tonight",
    "tomorrow",
    "morning",
    "evening",
    "afternoon",
    "week",
    "weekend",
    "o'clock",
    "january",
    "february",
    "march",
    "april",
    "may",
    "june",
    "july",
    "august",
    "september",
    "october",
    "november",
    "december",
    "mandag",
    "tirsdag",
    "onsdag",
    "torsdag",
    "fredag",
    "lørdag",
    "søndag",
    "mandager",
    "tirsdager",
    "onsdager",
    "torsdager",
    "fredager",
    "lørdager",
    "søndager",
    "i dag",
    "morgen",
    "kveld",
    "uke",
    "helg",
    "klokka",
    "kl",
    "januar",
    "februar",
    "mars",
    "mai",
    "juni",
    "juli",
    "oktober",
    "desember",
];

/// What the rest of a note says about its subject: where it is, when it is, or something else.
/// Two notes about one thing replace each other only when they say the same kind of thing, so
/// "the dentist is on Friday" and "the dentist is Dr Hansen" are both kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
enum Kind {
    Place,
    Time,
    #[default]
    Other,
}

fn kind_of(predicate: &[String]) -> Kind {
    let at_number = predicate.windows(2).any(|p| p[0] == "at" && p[1].chars().any(|c| c.is_ascii_digit()));
    if at_number || predicate.iter().any(|w| WHEN.contains(&w.as_str())) {
        Kind::Time
    } else if predicate.first().is_some_and(|w| PLACES.contains(&w.as_str())) {
        Kind::Place
    } else {
        Kind::Other
    }
}

/// What a note is about, and what kind of thing it says: "the spare key is in the kitchen drawer"
/// is about the spare key and where it is, "Ingrid's bike is in the shed" about Ingrid's bike, "I
/// put my bike in the shed" about the bike, "I parked on level 3" about parking. A note with none
/// of those shapes is about all its words.
fn subject_of(note: &str) -> (Vec<String>, Kind) {
    let words: Vec<String> = note.split_whitespace().map(normalize).filter(|w| !w.is_empty()).collect();
    let pronoun = words.first().is_some_and(|w| ["i", "we", "jeg", "vi"].contains(&w.as_str()));
    // "I put my bike in the shed": the thing after the verb, up to where it went.
    if let Some(i) = words.iter().position(|w| PUT.contains(&w.as_str())).filter(|_| pronoun) {
        let end = words[i + 1..].iter().position(|w| PLACES.contains(&w.as_str())).map_or(words.len(), |e| i + 1 + e);
        let thing = subject_words(&words[i + 1..end].join(" "));
        let kind = kind_of(&words[end..]);
        // "I parked on level 3": about parking.
        return if thing.is_empty() { (vec![stem(&words[i])], Kind::Place) } else { (thing, kind) };
    }
    if let Some(i) = words.iter().position(|w| VERBS.contains(&w.as_str())) {
        let before = subject_words(&words[..i].join(" "));
        if !before.is_empty() {
            return (before, kind_of(&words[i + 1..]));
        }
        // "We need milk on Friday", "I have a meeting at 3": about what follows the verb.
        let end = words[i + 1..]
            .iter()
            .position(|w| PLACES.contains(&w.as_str()) || WHEN.contains(&w.as_str()))
            .map_or(words.len(), |e| i + 1 + e);
        let thing = subject_words(&words[i + 1..end].join(" "));
        if !thing.is_empty() {
            return (thing, kind_of(&words[end..]));
        }
    }
    // "The key's in the drawer": "'s" for "is", since no other verb came.
    if let Some(i) = words.iter().position(|w| w.ends_with("'s") && w.len() > 2) {
        let mut before: Vec<String> = words[..i].to_vec();
        before.push(words[i].trim_end_matches("'s").to_owned());
        let subject = subject_words(&before.join(" "));
        if !subject.is_empty() {
            return (subject, kind_of(&words[i + 1..]));
        }
    }
    (subject_words(note), Kind::Other)
}

/// The note said back to the person: "my bike is in the shed" becomes "Your bike is in the shed.".
fn said_back(note: &str, lang: Lang) -> String {
    let swap: &[(&str, &str)] = if lang == Lang::Norwegian {
        &[
            ("jeg", "du"),
            ("meg", "deg"),
            ("min", "din"),
            ("mi", "di"),
            ("mitt", "ditt"),
            ("mine", "dine"),
            ("vi", "dere"),
            ("oss", "dere"),
            ("vår", "deres"),
            ("vårt", "deres"),
            ("våre", "deres"),
        ]
    } else {
        &[
            ("i", "you"),
            ("i'm", "you're"),
            ("i've", "you've"),
            ("i'll", "you'll"),
            ("me", "you"),
            ("my", "your"),
            ("mine", "yours"),
            ("myself", "yourself"),
            ("we", "you"),
            ("we're", "you're"),
            ("us", "you"),
            ("our", "your"),
            ("am", "are"),
        ]
    };
    let words: Vec<String> = note
        .split_whitespace()
        .map(|token| {
            let bare = token.trim_end_matches([',', '.', '!', '?']);
            let tail = &token[bare.len()..];
            match swap.iter().find(|(from, _)| bare.to_lowercase() == *from) {
                Some((_, to)) => format!("{to}{tail}"),
                None => token.to_owned(),
            }
        })
        .collect();
    let text = words.join(" ");
    let text = text.trim().trim_end_matches(['.', ',', '!', '?', ';']);
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => format!("{}{}.", first.to_uppercase(), chars.as_str()),
        None => String::new(),
    }
}

/// What a password or code is called, as a whole word. A note is read out to whoever asks.
const SECRET: &[&str] = &[
    "password",
    "passwords",
    "passcode",
    "passcodes",
    "passphrase",
    "pass",
    "pin",
    "pins",
    "pincode",
    "code",
    "codes",
    "codeword",
    "combination",
    "combo",
    "login",
    "logins",
    "passord",
    "passordet",
    "kode",
    "koden",
    "koder",
    "kodene",
    "pinkode",
    "pinkoden",
    "kodeord",
    "kodeordet",
    "kombinasjon",
    "kombinasjonen",
    "kortnummer",
    "kortnummeret",
];
/// Norwegian compounds that end in a code or a password ("garasjekoden").
const SECRET_ENDINGS: &[&str] = &["kode", "koden", "koder", "kodene", "passord", "passordet"];

/// A password or code: kept out of the notes.
pub fn is_secret(text: &str) -> bool {
    let words: Vec<String> = text
        .split_whitespace()
        .map(normalize)
        .map(|w| w.trim_end_matches("'s").trim_end_matches('\'').to_owned())
        .filter(|w| !w.is_empty())
        .collect();
    let has = |list: &[&str]| words.iter().any(|w| list.contains(&w.as_str()));
    let named = words.iter().any(|w| {
        SECRET.contains(&w.as_str()) || SECRET_ENDINGS.iter().any(|end| w.ends_with(end) && w.len() > end.len() + 2)
    });
    // "The wifi key", "card number".
    let wifi_key =
        has(&["key", "nøkkel", "nøkkelen"]) && has(&["wifi", "wi", "network", "router", "nettverk", "ruter"]);
    let card = words
        .windows(2)
        .any(|p| ["card", "kort"].contains(&p[0].as_str()) && ["number", "nummer"].contains(&p[1].as_str()));
    // "The gate opens with 4512", "the bike lock is 4512".
    let lock = has(&["lock", "gate", "door", "opens", "lås", "låsen", "porten", "døra", "åpner"]);
    let digits = words.iter().any(|w| w.chars().filter(char::is_ascii_digit).count() >= 4);
    named || wifi_key || card || (lock && digits)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Note {
    /// As it is said back: "The spare key is in the kitchen drawer."
    text: String,
    /// What it is about, stemmed: "spare", "key".
    subject: Vec<String>,
    /// What it says about it, so a note replaces only one of the same kind.
    #[serde(default)]
    kind: Kind,
    norwegian: bool,
    saved: jiff::Timestamp,
}

/// The notes in `text`, one by one: an entry that does not read loses only itself. A file that is
/// not a list at all is set aside under a name of its own rather than overwritten by the next save.
fn read_kept(text: &str, path: &std::path::Path) -> Vec<Note> {
    let entries: Vec<serde_json::Value> = match serde_json::from_str(text) {
        Ok(entries) => entries,
        Err(error) => {
            let aside = path.with_extension(format!("bad-{}", jiff::Timestamp::now().as_second()));
            warn!(%error, path = %path.display(), aside = %aside.display(), "could not read the kept notes; set aside");
            let _ = std::fs::rename(path, aside);
            return Vec::new();
        }
    };
    entries
        .into_iter()
        .filter_map(|entry| {
            serde_json::from_value(entry).inspect_err(|error| warn!(%error, "a kept note did not read; dropped")).ok()
        })
        .take(MAX_NOTES)
        .collect()
}

/// A question waiting for its answer.
#[derive(Debug, Clone)]
enum Asked {
    /// "Remember what?": the next words are the note.
    What,
    /// "Forget everything I remember?"
    ForgetAll,
    /// "I have 3 notes about that. Forget all of them?": the words they were asked by.
    Forget(Vec<String>),
}

#[derive(Default)]
pub struct Memory {
    notes: Vec<Note>,
    store: Option<PathBuf>,
    asked: Option<(Asked, Instant)>,
}

impl Memory {
    /// Kept in `path`, and read back from it. A file that is there but cannot be read (permissions,
    /// a disk error) is left as it is, and nothing is saved over it.
    pub fn kept_in(path: PathBuf) -> Self {
        match std::fs::read_to_string(&path) {
            Ok(text) => Self { notes: read_kept(&text, &path), store: Some(path), ..Self::default() },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Self { store: Some(path), ..Self::default() },
            Err(error) => {
                warn!(%error, path = %path.display(), "could not read the kept notes; they are not saved this run");
                Self::default()
            }
        }
    }

    pub fn len(&self) -> usize {
        self.notes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.notes.is_empty()
    }

    fn save(&self) {
        let Some(path) = &self.store else { return };
        // Written aside, flushed to the disk and renamed, so a crash never leaves half a file.
        let partial = path.with_extension("part");
        let result = serde_json::to_vec_pretty(&self.notes).map_err(std::io::Error::other).and_then(|bytes| {
            let mut file = std::fs::File::create(&partial)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            std::fs::rename(&partial, path)
        });
        if let Err(error) = result {
            warn!(%error, path = %path.display(), "could not keep the notes");
        }
    }

    fn asked(&self) -> Option<&Asked> {
        self.asked.as_ref().filter(|(_, at)| at.elapsed() < ASK_WINDOW).map(|(asked, _)| asked)
    }

    /// "Remember what?" is waiting for its answer.
    pub fn asked_what(&self) -> bool {
        matches!(self.asked(), Some(Asked::What))
    }

    /// A question before forgetting is waiting for a yes or a no.
    pub fn asked_to_forget(&self) -> bool {
        matches!(self.asked(), Some(Asked::ForgetAll | Asked::Forget(_)))
    }

    /// Drops the question: the next request was something else.
    pub fn clear_question(&mut self) {
        self.asked = None;
    }

    /// The answer to a question before forgetting.
    pub fn answer_forget(&mut self, yes: bool, lang: Lang) -> String {
        let no = lang == Lang::Norwegian;
        let Some((asked, _)) = self.asked.take() else { return String::new() };
        match (asked, yes) {
            (Asked::ForgetAll, false) => if no { "Ok, jeg beholder alt." } else { "Okay, I'll keep it all." }.into(),
            (Asked::Forget(_), false) => if no { "Ok, jeg beholder dem." } else { "Okay, I'll keep them." }.into(),
            (Asked::ForgetAll, true) => {
                self.notes.clear();
                self.save();
                if no { "Ok, jeg har glemt alt." } else { "Okay, I've forgotten everything." }.into()
            }
            (Asked::Forget(subject), true) => {
                let before = self.notes.len();
                self.notes.retain(|n| !covers(&subject, &n.subject));
                self.save();
                forgotten(before - self.notes.len(), lang)
            }
            (Asked::What, _) => String::new(),
        }
    }

    /// The note a "where is" question is about, said back; `None` when none is about it.
    pub fn where_is(&self, text: &str) -> Option<String> {
        let asked = subject_words(&where_subject(text)?);
        self.notes.iter().rev().find(|n| covers(&asked, &n.subject)).map(|n| n.text.clone())
    }

    /// Runs a command; the spoken answer.
    pub fn run(&mut self, command: MemoryCommand, lang: Lang) -> String {
        let no = lang == Lang::Norwegian;
        match command {
            MemoryCommand::Remember(note) => self.remember(&note, lang),
            MemoryCommand::Recall(about) => {
                let asked = subject_words(&about);
                // By what a note is about first, then by any of its words ("where did I park").
                let mut found: Vec<&Note> = self.notes.iter().rev().filter(|n| covers(&asked, &n.subject)).collect();
                if found.is_empty() {
                    found = self.notes.iter().rev().filter(|n| covers(&asked, &subject_words(&n.text))).collect();
                }
                if found.is_empty() {
                    return if no { "Jeg har ikke noe notat om det." } else { "I don't have a note about that." }
                        .into();
                }
                found.iter().take(3).map(|n| n.text.as_str()).collect::<Vec<_>>().join(" ")
            }
            MemoryCommand::List => {
                if self.notes.is_empty() {
                    return if no { "Jeg husker ingenting ennå." } else { "I don't remember anything yet." }.into();
                }
                let count = match (self.notes.len(), no) {
                    (1, true) => "Jeg husker én ting.".to_owned(),
                    (1, false) => "I remember one thing.".to_owned(),
                    (n, true) => format!("Jeg husker {n} ting."),
                    (n, false) => format!("I remember {n} things."),
                };
                let mut parts = vec![count];
                parts.extend(self.notes.iter().rev().take(READ_OUT).map(|n| n.text.clone()));
                let more = self.notes.len().saturating_sub(READ_OUT);
                if more > 0 {
                    parts.push(if no { format!("Og {more} til.") } else { format!("And {more} more.") });
                }
                parts.join(" ")
            }
            // By what a note is about only: "forget the key" must not drop a note that merely
            // mentions one.
            MemoryCommand::Forget(about) => {
                let asked = subject_words(&about);
                match self.notes.iter().filter(|n| covers(&asked, &n.subject)).count() {
                    0 if no => "Jeg har ikke noe notat om det.".into(),
                    0 => "I don't have a note about that.".into(),
                    1 => {
                        self.notes.retain(|n| !covers(&asked, &n.subject));
                        self.save();
                        forgotten(1, lang)
                    }
                    n => {
                        self.asked = Some((Asked::Forget(asked), Instant::now()));
                        if no {
                            format!("Jeg har {n} notater om det. Skal jeg glemme alle?")
                        } else {
                            format!("I have {n} notes about that. Forget all of them?")
                        }
                    }
                }
            }
            MemoryCommand::ForgetAll => {
                if self.notes.is_empty() {
                    return if no { "Jeg husker ingenting ennå." } else { "I don't remember anything yet." }.into();
                }
                self.asked = Some((Asked::ForgetAll, Instant::now()));
                if no { "Skal jeg glemme alt jeg husker?" } else { "Forget everything I remember?" }.into()
            }
        }
    }

    /// Keeps `note`, or replaces the one that says the same kind of thing about the same thing.
    pub fn remember(&mut self, note: &str, lang: Lang) -> String {
        let no = lang == Lang::Norwegian;
        self.asked = None;
        if subject_words(note).is_empty() {
            self.asked = Some((Asked::What, Instant::now()));
            return if no { "Hva skal jeg huske?" } else { "Remember what?" }.into();
        }
        if is_secret(note) {
            return if no {
                "Jeg tar ikke vare på passord eller koder. Alle i rommet kan spørre meg om dem."
            } else {
                "I don't keep passwords or codes. Anyone in the room could ask me for them."
            }
            .into();
        }
        let (subject, kind) = subject_of(note);
        let text = said_back(note, lang);
        let same = self.notes.iter().position(|n| n.subject == subject && n.kind == kind);
        let fresh = Note { text, subject, kind, norwegian: no, saved: jiff::Timestamp::now() };
        let answer = match same {
            Some(i) => {
                self.notes.remove(i);
                self.notes.push(fresh);
                if no {
                    "Ok, jeg har oppdatert det."
                } else {
                    "Okay, I've updated that."
                }
            }
            None if self.notes.len() >= MAX_NOTES => {
                return if no {
                    "Notatene mine er fulle. Glem noe først."
                } else {
                    "My notes are full. Forget something first."
                }
                .into()
            }
            None => {
                self.notes.push(fresh);
                if no {
                    "Ok, det skal jeg huske."
                } else {
                    "Okay, I'll remember that."
                }
            }
        };
        self.save();
        answer.into()
    }

    /// The notes as they are said, which change and are never for the speech cache.
    pub fn sentences(&self) -> impl Iterator<Item = &str> {
        self.notes.iter().map(|n| n.text.as_str())
    }
}

fn forgotten(count: usize, lang: Lang) -> String {
    match (count, lang == Lang::Norwegian) {
        (1, true) => "Ok, jeg har glemt det.".into(),
        (1, false) => "Okay, I've forgotten that.".into(),
        (n, true) => format!("Ok, jeg har glemt {n} notater."),
        (n, false) => format!("Okay, I've forgotten {n} notes."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use MemoryCommand::*;

    #[test]
    fn requests_parse() {
        let remember = |s: &str| Some(Remember(s.into()));
        assert_eq!(
            parse("Remember that the spare key is in the kitchen drawer"),
            remember("the spare key is in the kitchen drawer")
        );
        assert_eq!(parse("Can you remember that I parked on level 3?"), remember("I parked on level 3?"));
        assert_eq!(parse("remember the spare key is in the drawer"), remember("the spare key is in the drawer"));
        assert_eq!(parse("remember my keys are on the hook"), remember("my keys are on the hook"));
        assert_eq!(parse("Husk at reservenøkkelen ligger i skuffen"), remember("reservenøkkelen ligger i skuffen"));
        assert_eq!(parse("Make a note that the bins go out on Tuesday"), remember("the bins go out on Tuesday"));
        assert_eq!(parse("remember this"), remember(""));
        assert_eq!(parse("What did I say about the spare key?"), Some(Recall("the spare key?".into())));
        assert_eq!(parse("What did I tell you about the car?"), Some(Recall("the car?".into())));
        assert_eq!(parse("Hva sa jeg om reservenøkkelen?"), Some(Recall("reservenøkkelen?".into())));
        assert_eq!(parse("Hva har jeg sagt om bilen?"), Some(Recall("bilen?".into())));
        assert_eq!(parse("What do you remember about the spare key?"), Some(Recall("the spare key?".into())));
        assert_eq!(parse("Where did I put my keys?"), Some(Recall("put my keys?".into())));
        assert_eq!(parse("Where did I park?"), Some(Recall("park?".into())));
        assert_eq!(parse("Hvor la jeg nøklene?"), Some(Recall("la jeg nøklene?".into())));
        assert_eq!(parse("What do you remember?"), Some(List));
        assert_eq!(parse("Hva husker du?"), Some(List));
        assert_eq!(parse("Forget the spare key"), Some(Forget("the spare key".into())));
        assert_eq!(parse("Forget about the spare key"), Some(Forget("the spare key".into())));
        assert_eq!(parse("Glem reservenøkkelen"), Some(Forget("reservenøkkelen".into())));
        assert_eq!(parse("Forget everything"), Some(ForgetAll));
        assert_eq!(parse("Glem alt"), Some(ForgetAll));
        assert_eq!(parse("Forget everything you remember"), Some(ForgetAll));
    }

    #[test]
    fn other_requests_are_left_alone() {
        for other in [
            "forget it",
            "forget about it",
            "forget it, turn off the lights",
            "glem det, skru av lyset",
            "forget the timer",
            "forget the pasta timer",
            "forget my alarm",
            "forget the reminder",
            "forget milk from the shopping list",
            "glem handlelisten",
            "glem timeren",
            "delete the spare key",
            "slett reservenøkkelen",
            "remember to buy milk",
            "remember me?",
            "remember the milk",
            "remember the time we went to Bergen",
            "remember when we went to Bergen",
            "remember that song",
            "husk å kjøpe melk",
            "husk på å ringe mamma",
            "do you know what time it is",
            "do you remember where the spare key is",
            "vet du hvor Ingrid er",
            "husker du hva klokka er",
            "what do you know about Edvard Munch",
            "hva vet du om Edvard Munch",
            "what did you say?",
            "what did you say about the weather",
            "play Remember the Time",
            "where is Ingrid",
            "do you remember",
        ] {
            assert_eq!(parse(other), None, "{other}");
        }
    }

    #[test]
    fn where_questions_name_their_subject() {
        assert_eq!(where_subject("Where is the spare key?").as_deref(), Some("the spare key?"));
        assert_eq!(where_subject("where are my glasses").as_deref(), Some("my glasses"));
        assert_eq!(where_subject("where's the remote").as_deref(), Some("the remote"));
        assert_eq!(where_subject("Hvor er reservenøkkelen?").as_deref(), Some("reservenøkkelen?"));
        assert_eq!(where_subject("hvor ligger skrutrekkeren").as_deref(), Some("skrutrekkeren"));
        assert_eq!(where_subject("where is it"), None);
        assert_eq!(where_subject("what is the weather"), None);
    }

    #[test]
    fn notes_are_kept_and_found() {
        let mut memory = Memory::default();
        let en = Lang::English;
        assert_eq!(
            memory.run(Remember("the spare key is in the kitchen drawer".into()), en),
            "Okay, I'll remember that."
        );
        assert_eq!(memory.run(Remember("my bike is in the shed".into()), en), "Okay, I'll remember that.");
        assert_eq!(memory.run(Remember("I parked on level 3".into()), en), "Okay, I'll remember that.");
        assert_eq!(
            memory.where_is("Where is the spare key?").as_deref(),
            Some("The spare key is in the kitchen drawer.")
        );
        assert_eq!(memory.where_is("where is the key").as_deref(), Some("The spare key is in the kitchen drawer."));
        assert_eq!(memory.where_is("where's my bike").as_deref(), Some("Your bike is in the shed."));
        assert_eq!(memory.where_is("where is Ingrid"), None);
        assert_eq!(memory.run(Recall("my bikes".into()), en), "Your bike is in the shed.");
        assert_eq!(memory.run(Recall("".into()), en), "I don't have a note about that.");
        assert_eq!(memory.run(parse("where did I park").unwrap(), en), "You parked on level 3.");
        assert_eq!(memory.run(Recall("my glasses".into()), en), "I don't have a note about that.");
        assert_eq!(
            memory.run(List, en),
            "I remember 3 things. You parked on level 3. Your bike is in the shed. The spare key is in the kitchen drawer."
        );
        // Where it is again replaces where it was; parking replaces parking.
        assert_eq!(memory.run(Remember("the spare key is under the mat".into()), en), "Okay, I've updated that.");
        assert_eq!(memory.where_is("where is the spare key").as_deref(), Some("The spare key is under the mat."));
        assert_eq!(memory.run(Remember("I parked on level 5".into()), en), "Okay, I've updated that.");
        assert_eq!(memory.run(parse("where did I park").unwrap(), en), "You parked on level 5.");
        assert_eq!(memory.len(), 3);
        assert_eq!(memory.run(Forget("the spare key".into()), en), "Okay, I've forgotten that.");
        assert_eq!(memory.run(Forget("the spare key".into()), en), "I don't have a note about that.");
        assert_eq!(memory.len(), 2);
    }

    #[test]
    fn subjects_keep_whose_and_what_kind() {
        let mut memory = Memory::default();
        let en = Lang::English;
        memory.run(Remember("Ingrid's bike is in the shed".into()), en);
        memory.run(Remember("my bike is in the hall".into()), en);
        assert_eq!(memory.len(), 2);
        assert_eq!(memory.where_is("where is Ingrid's bike").as_deref(), Some("Ingrid's bike is in the shed."));
        memory.run(Remember("mum's birthday is on 3 May".into()), en);
        memory.run(Remember("mum's address is Storgata 1".into()), en);
        assert_eq!(memory.len(), 4);
        // When and who are different facts about the dentist.
        memory.run(Remember("the dentist is on Friday".into()), en);
        memory.run(Remember("the dentist is Dr Hansen".into()), en);
        assert_eq!(memory.len(), 6);
        assert_eq!(memory.run(Remember("the dentist is on Monday at 9".into()), en), "Okay, I've updated that.");
        assert_eq!(memory.len(), 6);
        // A note that only mentions something is not forgotten by it.
        memory.run(Remember("the spare key is next to the dentist card".into()), en);
        assert_eq!(memory.run(Forget("the card".into()), en), "I don't have a note about that.");
    }

    #[test]
    fn several_notes_are_forgotten_after_asking() {
        let mut memory = Memory::default();
        let en = Lang::English;
        memory.run(Remember("the spare key is in the drawer".into()), en);
        memory.run(Remember("the car key is on the hook".into()), en);
        assert_eq!(memory.run(Forget("the key".into()), en), "I have 2 notes about that. Forget all of them?");
        assert!(memory.asked_to_forget());
        assert_eq!(memory.answer_forget(false, en), "Okay, I'll keep them.");
        assert_eq!(memory.len(), 2);
        memory.run(Forget("keys".into()), en);
        assert_eq!(memory.answer_forget(true, en), "Okay, I've forgotten 2 notes.");
        assert!(memory.is_empty());
    }

    #[test]
    fn norwegian_notes() {
        let mut memory = Memory::default();
        let no = Lang::Norwegian;
        let said = parse("husk at reservenøkkelen ligger i skuffen").unwrap();
        assert_eq!(memory.run(said, no), "Ok, det skal jeg huske.");
        assert_eq!(memory.where_is("hvor er reservenøkkelen").as_deref(), Some("Reservenøkkelen ligger i skuffen."));
        assert_eq!(memory.where_is("hvor er reservenøkkel").as_deref(), Some("Reservenøkkelen ligger i skuffen."));
        assert_eq!(memory.run(parse("husk at jeg la sykkelen min i boden").unwrap(), no), "Ok, det skal jeg huske.");
        assert_eq!(memory.run(parse("hvor la jeg sykkelen").unwrap(), no), "Du la sykkelen din i boden.");
        assert_eq!(memory.run(parse("glem reservenøkkelen").unwrap(), no), "Ok, jeg har glemt det.");
        assert_eq!(memory.run(List, no), "Jeg husker én ting. Du la sykkelen din i boden.");
    }

    #[test]
    fn secrets_are_not_kept() {
        let mut memory = Memory::default();
        for said in [
            "my bike lock code is 4512",
            "the wifi password is hunter2",
            "the wifi key is abcd1234",
            "my PIN is 1234",
            "the gate opens with 4512",
            "the bike lock is 4512",
            "my card number is 1234 5678",
            "the alarm code's 9876",
            "the code-word is banana",
            "the passphrase is correct horse",
            "the combo is 12 34 56",
            "my login is ola",
            "garasjekoden er 1234",
            "passordet til ruteren er abc",
        ] {
            let answer = memory.run(Remember(said.into()), Lang::English);
            assert!(answer.starts_with("I don't keep passwords or codes."), "{said}: {answer}");
        }
        assert!(memory.is_empty());
        assert!(!is_secret("the spare key is in the drawer"));
        assert!(!is_secret("the barcode is on the box"));
        assert!(!is_secret("the passport is in the drawer"));
        assert!(!is_secret("the dentist is at 1430"));
    }

    #[test]
    fn questions_wait_for_their_answer() {
        let mut memory = Memory::default();
        assert_eq!(memory.run(Remember(String::new()), Lang::English), "Remember what?");
        assert!(memory.asked_what());
        assert_eq!(memory.remember("the bins go out on Tuesday", Lang::English), "Okay, I'll remember that.");
        assert!(!memory.asked_what());
        assert_eq!(memory.run(ForgetAll, Lang::English), "Forget everything I remember?");
        assert!(memory.asked_to_forget());
        assert_eq!(memory.answer_forget(false, Lang::English), "Okay, I'll keep it all.");
        assert_eq!(memory.len(), 1);
        memory.run(ForgetAll, Lang::English);
        assert_eq!(memory.answer_forget(true, Lang::English), "Okay, I've forgotten everything.");
        assert!(memory.is_empty());
        assert_eq!(memory.run(ForgetAll, Lang::English), "I don't remember anything yet.");
        assert!(!memory.asked_to_forget());
    }

    #[test]
    fn fixed_answers_are_rendered_ahead() {
        let mut memory = Memory::default();
        let en = Lang::English;
        let mut answers = vec![
            memory.run(Remember("my PIN is 1234".into()), en),
            memory.run(Remember(String::new()), en),
            memory.run(List, en),
            memory.run(Recall("the cat".into()), en),
            memory.run(ForgetAll, en),
            memory.run(Remember("the bins go out on Tuesday".into()), en),
            memory.run(Remember("the bins go out on Wednesday".into()), en),
            memory.run(Forget("the bins".into()), en),
        ];
        memory.run(Remember("the spare key is in the drawer".into()), en);
        memory.run(Remember("the car key is on the hook".into()), en);
        let asked = memory.run(Forget("key".into()), en);
        answers.push(asked.trim_start_matches("I have 2 notes about that. ").to_owned());
        answers.push(memory.answer_forget(false, en));
        memory.run(ForgetAll, en);
        answers.push(memory.answer_forget(false, en));
        memory.run(ForgetAll, en);
        answers.push(memory.answer_forget(true, en));
        for answer in answers {
            for sentence in answer.split_inclusive(['.', '?']).map(str::trim) {
                assert!(ENGLISH_ANSWERS.contains(&sentence), "{sentence}");
            }
        }
    }

    #[test]
    fn the_notes_are_capped() {
        let mut memory = Memory::default();
        for i in 0..MAX_NOTES {
            memory.run(Remember(format!("box {i} is in the attic")), Lang::English);
        }
        assert_eq!(memory.len(), MAX_NOTES);
        assert_eq!(
            memory.run(Remember("the spare key is in the drawer".into()), Lang::English),
            "My notes are full. Forget something first."
        );
        // Replacing one is still fine.
        assert_eq!(memory.run(Remember("box 7 is in the cellar".into()), Lang::English), "Okay, I've updated that.");
        assert!(memory.run(List, Lang::English).ends_with(&format!("And {} more.", MAX_NOTES - READ_OUT)));
    }

    #[test]
    fn notes_survive_a_restart_and_a_bad_entry() {
        let dir = std::env::temp_dir().join(format!("innestemme-memory-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("memory.json");
        let mut memory = Memory::kept_in(path.clone());
        memory.run(Remember("the spare key is in the drawer".into()), Lang::English);
        memory.run(Remember("my bike is in the shed".into()), Lang::English);
        let again = Memory::kept_in(path.clone());
        assert_eq!(again.where_is("where is the spare key").as_deref(), Some("The spare key is in the drawer."));
        // One damaged entry loses only itself.
        let text = std::fs::read_to_string(&path).unwrap();
        let mut entries: Vec<serde_json::Value> = serde_json::from_str(&text).unwrap();
        entries[0] = serde_json::json!({ "text": 5 });
        std::fs::write(&path, serde_json::to_string(&entries).unwrap()).unwrap();
        let again = Memory::kept_in(path.clone());
        assert_eq!(again.len(), 1);
        // A file that is not a list is set aside under a name of its own, twice over.
        for _ in 0..2 {
            std::fs::write(&path, "not json").unwrap();
            assert!(Memory::kept_in(path.clone()).is_empty());
            std::thread::sleep(Duration::from_millis(1100));
        }
        let aside = std::fs::read_dir(&dir).unwrap().filter_map(Result::ok);
        assert_eq!(aside.filter(|e| e.file_name().to_string_lossy().contains(".bad-")).count(), 2);
        // A path that cannot be read as a file (here a directory) is left alone and not saved over.
        let blocked = dir.join("blocked.json");
        std::fs::create_dir_all(&blocked).unwrap();
        let mut memory = Memory::kept_in(blocked.clone());
        memory.run(Remember("the spare key is in the drawer".into()), Lang::English);
        assert!(blocked.is_dir());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
