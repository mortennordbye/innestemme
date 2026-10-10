//! "Who is ...", "what is ...", "tell me about ...": the first sentences of the Wikipedia article on
//! the topic, without a language model. Questions that are not about a topic ("how tall is the
//! Eiffel Tower") find the article but not the number; those are for a language model.

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::dialog::normalize;
use crate::lang::Lang;

/// Spoken answers stop after this many words, at a sentence end where possible.
const MAX_WORDS: usize = 35;

/// Openings that ask about a topic, longest first; what follows is the topic.
const OPENINGS: &[&str] = &[
    "what do you know about",
    "can you tell me about",
    "tell me about",
    "tell me who",
    "tell me what",
    "look up",
    "search for",
    "search",
    "google",
    "who is",
    "who was",
    "who are",
    "who were",
    "who's",
    "what is",
    "what are",
    "what was",
    "what's",
    "fortell meg om",
    "fortell om",
    "slå opp",
    "søk etter",
    "hvem er",
    "hvem var",
    "hva er",
    "hva var",
];
/// A topic with these is about the house or the people in it, not for Wikipedia.
const PERSONAL: &[&str] = &[
    "you", "your", "yours", "i", "me", "my", "we", "our", "us", "it", "this", "that", "du", "deg", "din", "jeg", "meg",
    "min", "vi", "vår",
];
const ARTICLES: &[&str] = &["a", "an", "the", "en", "et", "ei"];
/// "The capital of France", "the population of Norway": a fact about a topic, which an article's
/// opening rarely states. Left to a language model.
const ATTRIBUTES: &[&str] = &[
    "capital",
    "population",
    "height",
    "size",
    "age",
    "area",
    "length",
    "weight",
    "currency",
    "president",
    "leader",
    "mayor",
    "ceo",
    "owner",
    "founder",
    "author",
    "director",
    "price",
    "cost",
    "date",
    "distance",
    "temperature",
    "name",
    "hovedstaden",
    "hovedstad",
    "befolkningen",
    "høyden",
    "prisen",
];

/// The topic of a lookup question, if `text` is one.
pub fn topic(text: &str) -> Option<String> {
    let words: Vec<String> = text.split_whitespace().map(normalize).filter(|w| !w.is_empty()).collect();
    let joined = words.join(" ");
    let opening = OPENINGS.iter().find(|o| joined.starts_with(&format!("{o} ")))?;
    let skip = opening.split(' ').count();
    // The topic keeps its original spelling ("Jonas Gahr Støre"), without the closing "?".
    let original: Vec<&str> = text.split_whitespace().skip(skip).collect();
    let mut topic: Vec<&str> = original.iter().map(|w| w.trim_matches(|c: char| !c.is_alphanumeric())).collect();
    topic.retain(|w| !w.is_empty());
    while topic.first().is_some_and(|w| ARTICLES.contains(&w.to_lowercase().as_str())) {
        topic.remove(0);
    }
    if topic.is_empty() || topic.iter().any(|w| PERSONAL.contains(&w.to_lowercase().as_str())) {
        return None;
    }
    let lower: Vec<String> = topic.iter().map(|w| w.to_lowercase()).collect();
    if lower.len() > 2
        && ATTRIBUTES.contains(&lower[0].as_str())
        && ["of", "in", "til", "i", "av"].contains(&lower[1].as_str())
    {
        return None;
    }
    Some(topic.join(" "))
}

pub struct Lookup {
    agent: ureq::Agent,
}

#[derive(Deserialize)]
struct Summary {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    extract: String,
}

#[derive(Deserialize)]
struct Search {
    query: SearchQuery,
}

#[derive(Deserialize)]
struct SearchQuery {
    search: Vec<SearchHit>,
}

#[derive(Deserialize)]
struct SearchHit {
    title: String,
}

impl Lookup {
    pub fn new(user_agent: &str) -> Self {
        Self { agent: crate::geo::agent(user_agent) }
    }

    /// The answer about `topic`, or `None` when Wikipedia has no article on it.
    pub fn answer(&self, topic: &str, lang: Lang) -> Result<Option<String>> {
        let host = if lang == Lang::Norwegian { "no.wikipedia.org" } else { "en.wikipedia.org" };
        // The exact article first ("Black hole"), else the best search hit.
        let mut extract = self.summary(host, topic)?;
        if extract.is_none() {
            for title in self.search(host, topic)? {
                extract = self.summary(host, &title)?;
                if extract.is_some() {
                    break;
                }
            }
        }
        let Some(extract) = extract else { return Ok(None) };
        let from = if lang == Lang::Norwegian { "Ifølge Wikipedia" } else { "According to Wikipedia" };
        Ok(Some(format!("{from}, {}", lowercase_first(&spoken(&extract)))))
    }

    /// The article's summary, unless there is none or it is a disambiguation page.
    fn summary(&self, host: &str, title: &str) -> Result<Option<String>> {
        let url = format!("https://{host}/api/rest_v1/page/summary/{}", encode(&title.replace(' ', "_")));
        let response = self.agent.get(&url).config().http_status_as_error(false).build().call();
        let mut response = response.with_context(|| format!("fetching {url}"))?;
        if response.status() == 404 {
            return Ok(None);
        }
        let summary: Summary = response.body_mut().read_json().with_context(|| format!("reading {url}"))?;
        Ok((summary.kind == "standard" && !summary.extract.is_empty()).then_some(summary.extract))
    }

    fn search(&self, host: &str, topic: &str) -> Result<Vec<String>> {
        let url = format!(
            "https://{host}/w/api.php?action=query&list=search&srlimit=3&format=json&utf8=1&srsearch={}",
            encode(topic)
        );
        let search: Search = self
            .agent
            .get(&url)
            .call()
            .and_then(|mut r| r.body_mut().read_json())
            .with_context(|| format!("searching {url}"))?;
        Ok(search.query.search.into_iter().map(|hit| hit.title).collect())
    }
}

/// The first sentences, up to `MAX_WORDS`, without bracketed asides (pronunciations, dates).
fn spoken(extract: &str) -> String {
    let mut plain = String::with_capacity(extract.len());
    let mut depth = 0usize;
    for c in extract.chars() {
        match c {
            '(' => depth += 1,
            ')' if depth > 0 => depth -= 1,
            _ if depth == 0 => plain.push(c),
            _ => {}
        }
    }
    let plain = plain.split_whitespace().collect::<Vec<_>>().join(" ").replace(" ,", ",").replace(" .", ".");
    let mut out = String::new();
    for sentence in plain.split_inclusive(". ") {
        let words = out.split_whitespace().count() + sentence.split_whitespace().count();
        if !out.is_empty() && words > MAX_WORDS {
            break;
        }
        out.push_str(sentence);
    }
    let out = out.trim();
    let words: Vec<&str> = out.split_whitespace().collect();
    // One very long first sentence: cut it, and end it.
    if words.len() > MAX_WORDS {
        return format!("{}.", words[..MAX_WORDS].join(" ").trim_end_matches([',', ';', ':']));
    }
    if out.ends_with(['.', '!', '?']) {
        out.to_owned()
    } else {
        format!("{out}.")
    }
}

/// "Paris is ..." stays; "The Eiffel Tower is ..." reads on after "According to Wikipedia, the".
fn lowercase_first(text: &str) -> String {
    let first = text.split_whitespace().next().unwrap_or_default();
    if ["The", "A", "An"].contains(&first) {
        let mut chars = text.chars();
        chars.next().map_or_else(String::new, |c| c.to_lowercase().chain(chars).collect())
    } else {
        text.to_owned()
    }
}

/// Percent-encoding for a URL path segment or query value.
fn encode(text: &str) -> String {
    text.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_topic() {
        assert_eq!(topic("Who is Jonas Gahr Støre?").as_deref(), Some("Jonas Gahr Støre"));
        assert_eq!(topic("what is a black hole").as_deref(), Some("black hole"));
        assert_eq!(topic("What's Skibidi Toilet?").as_deref(), Some("Skibidi Toilet"));
        assert_eq!(topic("tell me about the Eiffel Tower").as_deref(), Some("Eiffel Tower"));
        assert_eq!(topic("Hvem er Kong Harald?").as_deref(), Some("Kong Harald"));
    }

    #[test]
    fn leaves_personal_and_other_questions_alone() {
        assert_eq!(topic("What's your favorite color?"), None);
        assert_eq!(topic("what is my name"), None);
        assert_eq!(topic("what is it"), None);
        assert_eq!(topic("What's the capital of France?"), None);
        assert_eq!(topic("what is the speed of light").as_deref(), Some("speed of light"));
        assert_eq!(topic("turn on the lights"), None);
        assert_eq!(topic("what's"), None);
    }

    #[test]
    fn speaks_the_first_sentences() {
        let extract = "Paris (French pronunciation: [paʁi]) is the capital and largest city of France. It has an \
                       estimated population of two million. Located on the Seine, it is the largest metropolitan \
                       area in the European Union and a centre of finance, diplomacy, commerce, culture, fashion and \
                       gastronomy for centuries.";
        assert_eq!(
            spoken(extract),
            "Paris is the capital and largest city of France. It has an estimated population of two million."
        );
        assert_eq!(lowercase_first("The Eiffel Tower is a tower."), "the Eiffel Tower is a tower.");
        assert_eq!(encode("Jonas Gahr_Støre"), "Jonas%20Gahr_St%C3%B8re");
    }
}
