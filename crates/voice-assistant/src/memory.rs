//! Personal notes: "remember that the spare key is in the kitchen drawer", then "where is the spare
//! key?", "what did I say about the spare key?", "what do you remember?", "forget the spare key".
//! Kept in a file on the models volume, like the timers, so a deploy keeps them. Passwords and
//! codes are not kept: anyone in the room can ask for a note.

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
/// How long "Remember what?" and "Forget everything I remember?" wait for their answer, like the
/// engine's other questions.
const ASK_WINDOW: Duration = Duration::from_secs(30);

/// The fixed English answers a sentence at a time, rendered ahead by the speech cache; the notes
/// themselves are live.
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
    "Okay, I've forgotten everything.",
    "Okay, I'll keep it all.",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemoryCommand {
    /// "Remember that the spare key is in the kitchen drawer": the words after "remember (that)".
    /// Empty when nothing followed ("remember something").
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

/// Asking for a note's subject without a question word: what follows them is the subject.
const ABOUT: &[&str] = &["about", "om"];
/// Words with no meaning of their own in a subject: "the spare key" is the spare key.
const FILLER: &[&str] = &[
    "the", "a", "an", "my", "our", "your", "his", "her", "their", "its", "some", "that", "this", "these", "those",
    "it", "me", "i", "you", "we", "to", "of", "for", "please", "again", "now", "min", "mi", "mitt", "mine", "vår",
    "vårt", "våre", "din", "di", "ditt", "dine", "en", "ei", "et", "den", "det", "de", "jeg", "du", "vi", "meg", "deg",
    "oss", "til", "av", "for", "takk", "igjen", "nå", "is", "are", "er", "was", "var", "put", "left", "la", "lagt",
    "satte", "where", "what", "hvor", "hva", "did", "do", "say", "said", "tell", "told", "sa", "fortalte", "about",
    "om", "leave", "hide", "gjemte",
];
/// Politeness before the request: "can you remember that ...", "kan du huske at ...".
const LEAD: &[&str] =
    &["can", "could", "would", "will", "you", "please", "just", "okay", "ok", "hey", "kan", "du", "vil", "så"];
/// What a password or code is called. A note is read out to whoever asks, so these are not kept.
const SECRET: &[&str] = &[
    "password",
    "passwords",
    "passcode",
    "passcodes",
    "pin",
    "pins",
    "pincode",
    "code",
    "codes",
    "combination",
    "login",
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
];

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
    match w.as_slice() {
        // "Remember to buy milk" is a reminder without a time, not a note; "remember me" is not one either.
        ["remember" | "husk" | "huske", "to" | "me" | "å" | "meg", ..] => None,
        ["remember" | "husk" | "huske", "that" | "at", ..] => Some(MemoryCommand::Remember(rest(2))),
        ["remember" | "husk" | "huske", "this" | "dette" | "something" | "noe"] | ["remember" | "husk" | "huske"] => {
            Some(MemoryCommand::Remember(String::new()))
        }
        // "Remember the spare key is in the drawer": a sentence of its own after the word.
        ["remember" | "husk" | "huske", ..] if w.len() >= 4 => Some(MemoryCommand::Remember(rest(1))),
        ["make", "a" | "an", "note" | "notes", "that" | "of", ..] => Some(MemoryCommand::Remember(rest(4))),
        ["note" | "noter", "that" | "at", ..] => Some(MemoryCommand::Remember(rest(2))),
        ["forget" | "glem", "everything" | "alt" | "all" | "alle"]
        | ["forget" | "glem", "everything" | "alt", "you" | "du", "remember" | "know" | "husker" | "vet"]
        | ["forget" | "glem", "all" | "alle", "the" | "my", "notes" | "notater" | "notatene"]
        | ["delete" | "slett", "all" | "alle", "the" | "my", "notes" | "notater" | "notatene"]
        | ["slett" | "delete", "alle" | "all", "notatene" | "notes"] => Some(MemoryCommand::ForgetAll),
        // "Forget it", "forget about it", "glem det": never mind, which the rules answer.
        ["forget" | "glem" | "delete" | "slett", ..] => {
            let subject = rest(1);
            let about = subject_words(&subject);
            (!about.is_empty()).then(|| MemoryCommand::Forget(strip_about(&subject)))
        }
        ["what" | "what's" | "whats", "do" | "did", "you", "remember" | "know", rest @ ..]
        | ["hva", "husker" | "vet", "du", rest @ ..]
        | ["what", "have", "you", "remembered" | "noted", rest @ ..] => {
            let about = rest.iter().position(|w| ABOUT.contains(w));
            match about {
                Some(i) => Some(MemoryCommand::Recall(tokens[start + w.len() - rest.len() + i + 1..].join(" "))),
                None if rest.is_empty()
                    || rest.iter().all(|w| ["now", "nå", "so", "far", "about", "me", "meg", "om"].contains(w)) =>
                {
                    Some(MemoryCommand::List)
                }
                None => None,
            }
        }
        // "What did I say about the spare key", "what did I tell you about the car", "hva sa jeg om bilen".
        ["what" | "hva", "did" | "have" | "sa" | "fortalte" | "har", ..] => {
            let about = w.iter().position(|x| ABOUT.contains(x))?;
            // "What did you say about the weather" asks to hear an answer again, not for a note.
            let said = w[..about].iter().any(|x| ["say", "tell", "told", "said", "sa", "sagt", "fortalte"].contains(x))
                && w[..about].iter().any(|x| ["i", "jeg"].contains(x));
            (said && about + 1 < w.len()).then(|| MemoryCommand::Recall(rest(about + 1)))
        }
        // "Do you remember where the spare key is", "husker du hvor nøkkelen er".
        ["do", "you", "remember" | "know", "where" | "what", ..] | ["husker" | "vet", "du", "hvor" | "hva", ..] => {
            let about = rest(4).trim_end_matches(['?', '.']).to_owned();
            (!subject_words(&about).is_empty()).then_some(MemoryCommand::Recall(about))
        }
        // "Where did I put my keys", "where did I leave the remote", "hvor la jeg nøklene".
        // From the verb on: "where did I park" is about parking.
        ["where", "did", "i", "put" | "leave" | "park" | "hide", ..] => Some(MemoryCommand::Recall(rest(3))),
        ["hvor", "la" | "satte" | "gjemte" | "parkerte", "jeg", ..] => Some(MemoryCommand::Recall(rest(1))),
        ["hvor", "har", "jeg", "lagt" | "satt" | "gjemt" | "parkert", ..] => Some(MemoryCommand::Recall(rest(4))),
        _ => None,
    }
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
/// questions. People and places are asked the same way, so a note answers only when one is about it.
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

/// What a note is about: "the spare key is in the kitchen drawer" is about the spare key, "I put
/// my bike in the shed" about the bike. A note with neither shape is about all its words.
fn subject_of(note: &str) -> Vec<String> {
    let words: Vec<String> = note.split_whitespace().map(normalize).filter(|w| !w.is_empty()).collect();
    // "The key's in the drawer".
    if let Some(i) = words.iter().position(|w| w.ends_with("'s") && w.len() > 2) {
        let mut before: Vec<String> = words[..i].to_vec();
        before.push(words[i].trim_end_matches("'s").to_owned());
        let subject = subject_words(&before.join(" "));
        if !subject.is_empty() {
            return subject;
        }
    }
    if let Some(i) = words.iter().position(|w| PUT.contains(&w.as_str())) {
        let end = words[i + 1..].iter().position(|w| PLACES.contains(&w.as_str())).map_or(words.len(), |e| i + 1 + e);
        let subject = subject_words(&words[i + 1..end].join(" "));
        if !subject.is_empty() {
            return subject;
        }
    }
    if let Some(i) = words.iter().position(|w| VERBS.contains(&w.as_str())) {
        let subject = subject_words(&words[..i].join(" "));
        if !subject.is_empty() {
            return subject;
        }
    }
    subject_words(note)
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

/// A password or code: kept out of the notes. Norwegian compounds count too ("garasjekoden").
pub fn is_secret(text: &str) -> bool {
    text.split_whitespace().map(normalize).any(|w| {
        SECRET.contains(&w.as_str())
            || ["kode", "koden", "koder", "passord", "passordet", "code", "codes", "password", "passwords"]
                .iter()
                .any(|end| w.ends_with(end) && w.len() > end.len() + 2)
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Note {
    /// As it is said back: "The spare key is in the kitchen drawer."
    text: String,
    /// What it is about, stemmed: "spare", "key".
    subject: Vec<String>,
    norwegian: bool,
    saved: jiff::Timestamp,
}

/// The notes in `text`, one by one: an entry that does not read loses only itself. A file that is
/// not a list at all is set aside rather than overwritten by the next save.
fn read_kept(text: &str, path: &std::path::Path) -> Vec<Note> {
    let entries: Vec<serde_json::Value> = match serde_json::from_str(text) {
        Ok(entries) => entries,
        Err(error) => {
            warn!(%error, path = %path.display(), "could not read the kept notes; set aside");
            let _ = std::fs::rename(path, path.with_extension("bad"));
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

#[derive(Default)]
pub struct Memory {
    notes: Vec<Note>,
    store: Option<PathBuf>,
    /// "Remember what?" was asked: the next words are the note.
    asked_what: Option<Instant>,
    /// "Forget everything I remember?" was asked.
    asked_forget_all: Option<Instant>,
}

impl Memory {
    /// Kept in `path`, and read back from it.
    pub fn kept_in(path: PathBuf) -> Self {
        let notes = match std::fs::read_to_string(&path) {
            Ok(text) => read_kept(&text, &path),
            Err(_) => Vec::new(),
        };
        Self { notes, store: Some(path), ..Self::default() }
    }

    pub fn len(&self) -> usize {
        self.notes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.notes.is_empty()
    }

    fn save(&self) {
        let Some(path) = &self.store else { return };
        // Written aside and renamed, so a crash never leaves half a file.
        let partial = path.with_extension("part");
        let result = serde_json::to_vec_pretty(&self.notes)
            .map_err(std::io::Error::other)
            .and_then(|bytes| std::fs::write(&partial, bytes))
            .and_then(|()| std::fs::rename(&partial, path));
        if let Err(error) = result {
            warn!(%error, path = %path.display(), "could not keep the notes");
        }
    }

    /// "Remember what?" is waiting for its answer.
    pub fn asked_what(&self) -> bool {
        self.asked_what.is_some_and(|at| at.elapsed() < ASK_WINDOW)
    }

    /// "Forget everything I remember?" is waiting for a yes or a no.
    pub fn asked_forget_all(&self) -> bool {
        self.asked_forget_all.is_some_and(|at| at.elapsed() < ASK_WINDOW)
    }

    /// Drops the questions: the next request was something else.
    pub fn clear_questions(&mut self) {
        self.asked_what = None;
        self.asked_forget_all = None;
    }

    /// The answer to "Forget everything I remember?".
    pub fn answer_forget_all(&mut self, yes: bool, lang: Lang) -> String {
        self.asked_forget_all = None;
        let no = lang == Lang::Norwegian;
        if !yes {
            return if no { "Ok, jeg beholder alt." } else { "Okay, I'll keep it all." }.into();
        }
        self.notes.clear();
        self.save();
        if no { "Ok, jeg har glemt alt." } else { "Okay, I've forgotten everything." }.into()
    }

    /// The note a "where is" question is about, said back; `None` when none is about it, so the
    /// question goes on to people and places.
    pub fn where_is(&self, text: &str) -> Option<String> {
        let asked = subject_words(&where_subject(text)?);
        self.notes.iter().rev().find(|n| covers(&asked, &n.subject)).map(|n| n.text.clone())
    }

    /// Runs a command; the spoken answer.
    pub fn run(&mut self, command: MemoryCommand, lang: Lang) -> String {
        let no = lang == Lang::Norwegian;
        self.clear_questions();
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
            MemoryCommand::Forget(about) => {
                let asked = subject_words(&about);
                let before = self.notes.len();
                let by_subject = self.notes.iter().any(|n| covers(&asked, &n.subject));
                self.notes.retain(|n| {
                    !if by_subject { covers(&asked, &n.subject) } else { covers(&asked, &subject_words(&n.text)) }
                });
                match before - self.notes.len() {
                    0 if no => "Jeg har ikke noe notat om det.".into(),
                    0 => "I don't have a note about that.".into(),
                    gone => {
                        self.save();
                        match (gone, no) {
                            (1, true) => "Ok, jeg har glemt det.".into(),
                            (1, false) => "Okay, I've forgotten that.".into(),
                            (n, true) => format!("Ok, jeg har glemt {n} notater."),
                            (n, false) => format!("Okay, I've forgotten {n} notes."),
                        }
                    }
                }
            }
            MemoryCommand::ForgetAll => {
                if self.notes.is_empty() {
                    return if no { "Jeg husker ingenting ennå." } else { "I don't remember anything yet." }.into();
                }
                self.asked_forget_all = Some(Instant::now());
                if no { "Skal jeg glemme alt jeg husker?" } else { "Forget everything I remember?" }.into()
            }
        }
    }

    /// Keeps `note`, or replaces the one about the same thing.
    pub fn remember(&mut self, note: &str, lang: Lang) -> String {
        let no = lang == Lang::Norwegian;
        self.clear_questions();
        if subject_words(note).is_empty() {
            self.asked_what = Some(Instant::now());
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
        let subject = subject_of(note);
        let text = said_back(note, lang);
        let same = self.notes.iter().position(|n| n.subject == subject);
        let fresh = Note { text, subject, norwegian: no, saved: jiff::Timestamp::now() };
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

    /// The sentences answers read from the notes, which change and are not for the speech cache.
    pub fn sentences(&self) -> impl Iterator<Item = &str> {
        self.notes.iter().map(|n| n.text.as_str())
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
        assert_eq!(parse("Husk at reservenøkkelen ligger i skuffen"), remember("reservenøkkelen ligger i skuffen"));
        assert_eq!(parse("Make a note that the bins go out on Tuesday"), remember("the bins go out on Tuesday"));
        assert_eq!(parse("remember this"), remember(""));
        assert_eq!(parse("What did I say about the spare key?"), Some(Recall("the spare key?".into())));
        assert_eq!(parse("What did I tell you about the car?"), Some(Recall("the car?".into())));
        assert_eq!(parse("Hva sa jeg om reservenøkkelen?"), Some(Recall("reservenøkkelen?".into())));
        assert_eq!(parse("What do you remember about the spare key?"), Some(Recall("the spare key?".into())));
        assert_eq!(parse("Do you remember where the spare key is?"), Some(Recall("the spare key is".into())));
        assert_eq!(parse("Where did I put my keys?"), Some(Recall("put my keys?".into())));
        assert_eq!(parse("Where did I park?"), Some(Recall("park?".into())));
        assert_eq!(parse("Hvor la jeg nøklene?"), Some(Recall("la jeg nøklene?".into())));
        assert_eq!(parse("Hva har jeg sagt om bilen?"), Some(Recall("bilen?".into())));
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
            "glem det",
            "never mind, forget that",
            "remember to buy milk",
            "remember me?",
            "remind me in 10 minutes to check the oven",
            "what did you say?",
            "what did you say about the weather",
            "play Remember the Time",
            "what's the weather tomorrow",
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
        // A person or a place: no note, so the question goes on to them.
        assert_eq!(memory.where_is("where is Ingrid"), None);
        assert_eq!(memory.where_is("where is Bergen"), None);
        assert_eq!(memory.run(Recall("my bikes".into()), en), "Your bike is in the shed.");
        assert_eq!(memory.run(Recall("".into()), en), "I don't have a note about that.");
        // "Where did I park" finds the note by its words.
        assert_eq!(memory.run(parse("where did I park").unwrap(), en), "You parked on level 3.");
        assert_eq!(memory.run(Recall("parking".into()), en), "You parked on level 3.");
        assert_eq!(memory.run(Recall("my glasses".into()), en), "I don't have a note about that.");
        assert_eq!(memory.run(Recall("the cat".into()), en), "I don't have a note about that.");
        assert_eq!(
            memory.run(List, en),
            "I remember 3 things. You parked on level 3. Your bike is in the shed. The spare key is in the kitchen drawer."
        );
        // The same thing again replaces it.
        assert_eq!(memory.run(Remember("the spare key is under the mat".into()), en), "Okay, I've updated that.");
        assert_eq!(memory.where_is("where is the spare key").as_deref(), Some("The spare key is under the mat."));
        assert_eq!(memory.len(), 3);
        assert_eq!(memory.run(Forget("the spare key".into()), en), "Okay, I've forgotten that.");
        assert_eq!(memory.run(Forget("the spare key".into()), en), "I don't have a note about that.");
        assert_eq!(memory.len(), 2);
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
            "my PIN is 1234",
            "garasjekoden er 1234",
            "passordet til ruteren er abc",
        ] {
            let answer = memory.run(Remember(said.into()), Lang::English);
            assert!(answer.starts_with("I don't keep passwords or codes."), "{said}: {answer}");
        }
        assert!(memory.is_empty());
        assert!(!is_secret("the spare key is in the drawer"));
    }

    #[test]
    fn questions_wait_for_their_answer() {
        let mut memory = Memory::default();
        assert_eq!(memory.run(Remember(String::new()), Lang::English), "Remember what?");
        assert!(memory.asked_what());
        assert_eq!(memory.remember("the bins go out on Tuesday", Lang::English), "Okay, I'll remember that.");
        assert!(!memory.asked_what());
        assert_eq!(memory.run(ForgetAll, Lang::English), "Forget everything I remember?");
        assert!(memory.asked_forget_all());
        assert_eq!(memory.answer_forget_all(false, Lang::English), "Okay, I'll keep it all.");
        assert_eq!(memory.len(), 1);
        memory.run(ForgetAll, Lang::English);
        assert_eq!(memory.answer_forget_all(true, Lang::English), "Okay, I've forgotten everything.");
        assert!(memory.is_empty());
        assert_eq!(memory.run(ForgetAll, Lang::English), "I don't remember anything yet.");
        assert!(!memory.asked_forget_all());
    }

    #[test]
    fn fixed_answers_are_rendered_ahead() {
        let mut memory = Memory::default();
        let en = Lang::English;
        let answers = [
            memory.run(Remember("my PIN is 1234".into()), en),
            memory.run(Remember(String::new()), en),
            memory.run(List, en),
            memory.run(Recall("the cat".into()), en),
            memory.run(ForgetAll, en),
            memory.run(Remember("the bins go out on Tuesday".into()), en),
            memory.run(Remember("the bins go out on Wednesday".into()), en),
            memory.run(Forget("the bins".into()), en),
            memory.answer_forget_all(false, en),
            memory.answer_forget_all(true, en),
        ];
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
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("memory.json");
        let _ = std::fs::remove_file(&path);
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
        // A file that is not a list is set aside, not overwritten.
        std::fs::write(&path, "not json").unwrap();
        let again = Memory::kept_in(path.clone());
        assert!(again.is_empty());
        assert!(path.with_extension("bad").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
