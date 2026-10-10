//! The latest headlines from RSS feeds, for "what's the news?" and the morning briefing: by default
//! BBC News for the world and News in English for Norway (an English voice cannot read NRK's
//! Norwegian), NRK in Norwegian. Headlines are live data: synthesized every time, never kept by the
//! speech cache.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use tracing::warn;

use crate::lang::Lang;

/// Feeds as `Name=URL`; the name is how the source is introduced ("From Norway: ...").
pub const ENGLISH_FEEDS: &[&str] =
    &["BBC News=https://feeds.bbci.co.uk/news/rss.xml", "Norway=https://www.newsinenglish.no/feed/"];
pub const NORWEGIAN_FEEDS: &[&str] = &["NRK=https://www.nrk.no/toppsaker.rss"];
/// Headlines read out in all; more is a news broadcast, not an answer.
const HEADLINES: usize = 4;
/// Feeds change every few minutes at most; a briefing and a question soon after reuse one fetch.
const CACHE: Duration = Duration::from_secs(600);

pub struct News {
    agent: ureq::Agent,
    english: Vec<Source>,
    norwegian: Vec<Source>,
    cache: HashMap<String, (Instant, Feed)>,
}

#[derive(Debug, Clone, PartialEq)]
struct Source {
    /// From the setting; the feed's own title when it gives none.
    name: Option<String>,
    url: String,
}

#[derive(Debug, Clone, PartialEq)]
struct Feed {
    /// The source as it is spoken: "BBC News".
    name: String,
    headlines: Vec<String>,
}

/// What to say, and which of its sentences are headlines (not to be cached).
pub struct Headlines {
    pub text: String,
    pub live: Vec<String>,
}

impl News {
    /// Feeds as `Name=URL` or a bare URL.
    pub fn new(user_agent: &str, english: &[String], norwegian: &[String]) -> Self {
        Self {
            agent: crate::geo::agent(user_agent),
            english: english.iter().map(|f| source(f)).collect(),
            norwegian: norwegian.iter().map(|f| source(f)).collect(),
            cache: HashMap::new(),
        }
    }

    /// "Here are the latest headlines. From BBC News: ... From Norway: ..."
    pub fn answer(&mut self, lang: Lang) -> Result<Headlines> {
        let lead = if lang == Lang::Norwegian { "Her er de siste nyhetene." } else { "Here are the latest headlines." };
        self.read(lead, lang)
    }

    /// The briefing's part: "In the news. From BBC News: ..."
    pub fn briefing(&mut self, lang: Lang) -> Result<Headlines> {
        self.read(if lang == Lang::Norwegian { "I nyhetene." } else { "In the news." }, lang)
    }

    /// The headlines shared between the feeds that answer; a feed that fails is left out.
    fn read(&mut self, lead: &str, lang: Lang) -> Result<Headlines> {
        let sources = if lang == Lang::Norwegian { self.norwegian.clone() } else { self.english.clone() };
        let feeds: Vec<Feed> = sources
            .iter()
            .filter_map(|source| {
                self.feed(source).inspect_err(|error| warn!(error = format!("{error:#}"), "news feed failed")).ok()
            })
            .collect();
        if feeds.is_empty() {
            bail!("no news feed answered");
        }
        let each = (HEADLINES / feeds.len()).max(1);
        let from = if lang == Lang::Norwegian { "Fra" } else { "From" };
        let mut said = Vec::new();
        for feed in &feeds {
            for (i, headline) in feed.headlines.iter().take(each).enumerate() {
                let headline = sentence(headline);
                // Only with several sources is it worth saying where each comes from.
                said.push(if i == 0 && feeds.len() > 1 {
                    format!("{from} {}: {headline}", feed.name)
                } else {
                    headline
                });
            }
        }
        // Speech goes a sentence at a time, so "U.S. talks resume." is two pieces: both are live.
        let live = said
            .iter()
            .flat_map(|h| h.split_inclusive(['.', '?', '!']).map(str::trim).filter(|s| !s.is_empty()))
            .map(str::to_owned)
            .collect();
        let text = std::iter::once(lead.to_owned()).chain(said).collect::<Vec<_>>().join(" ");
        Ok(Headlines { text, live })
    }

    fn feed(&mut self, source: &Source) -> Result<Feed> {
        if let Some((at, feed)) = self.cache.get(&source.url) {
            if at.elapsed() < CACHE {
                return Ok(feed.clone());
            }
        }
        let xml = self
            .agent
            .get(&source.url)
            .call()
            .and_then(|mut response| response.body_mut().read_to_string())
            .with_context(|| format!("fetching {}", source.url))?;
        let mut feed = parse(&xml);
        if feed.headlines.is_empty() {
            bail!("{}: no headlines", source.url);
        }
        if let Some(name) = &source.name {
            feed.name = name.clone();
        }
        self.cache.insert(source.url.clone(), (Instant::now(), feed.clone()));
        Ok(feed)
    }
}

/// `Name=URL` or a bare URL.
fn source(setting: &str) -> Source {
    match setting.split_once('=') {
        Some((name, url)) if !name.contains("://") => {
            Source { name: Some(name.trim().to_owned()), url: url.trim().to_owned() }
        }
        _ => Source { name: None, url: setting.trim().to_owned() },
    }
}

/// A headline as spoken: with closing punctuation.
fn sentence(headline: &str) -> String {
    let text = headline.trim().to_owned();
    if text.ends_with(['.', '?', '!']) {
        text
    } else {
        format!("{text}.")
    }
}

/// The channel's name and its items' titles, from RSS 2.0.
fn parse(xml: &str) -> Feed {
    let channel = xml.split("<item").next().unwrap_or_default();
    let name = tag(channel, "title").map(|t| spoken_name(&t)).unwrap_or_default();
    let headlines =
        xml.split("<item").skip(1).filter_map(|item| tag(item, "title")).filter(|t| !t.is_empty()).collect();
    Feed { name, headlines }
}

/// "BBC News - Home" is said "BBC News", "NRK - Toppsaker" is "NRK".
fn spoken_name(title: &str) -> String {
    title.split(" - ").next().unwrap_or(title).trim().to_owned()
}

/// The text of the first `<name>` element, CDATA and entities resolved.
fn tag(xml: &str, name: &str) -> Option<String> {
    let start = xml.find(&format!("<{name}"))?;
    let after = &xml[start..];
    let open_end = after.find('>')? + 1;
    let close = after.find(&format!("</{name}>"))?;
    let inner = after.get(open_end..close)?.trim();
    let inner = inner.strip_prefix("<![CDATA[").and_then(|s| s.strip_suffix("]]>")).unwrap_or(inner);
    Some(entities(inner.trim()))
}

fn entities(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        let Some(end) = tail.find(';').filter(|&e| e <= 10) else {
            out.push('&');
            rest = &tail[1..];
            continue;
        };
        let entity = &tail[1..end];
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ => entity
                .strip_prefix("#x")
                .and_then(|h| u32::from_str_radix(h, 16).ok())
                .or_else(|| entity.strip_prefix('#').and_then(|d| d.parse().ok()))
                .and_then(char::from_u32),
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &tail[end + 1..];
            }
            None => {
                out.push('&');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const RSS: &str = r#"<?xml version="1.0"?><rss><channel><title><![CDATA[BBC News]]></title>
        <item><title><![CDATA[Storm brings flooding to coastal towns]]></title></item>
        <item><title>Talks resume in Geneva &amp; Vienna</title></item>
        <item><title>Who won? The final in 3 minutes</title></item>
        <item><title>A fourth story</title></item>
        </channel></rss>"#;

    #[test]
    fn reads_the_channel_and_titles() {
        let feed = parse(RSS);
        assert_eq!(feed.name, "BBC News");
        assert_eq!(feed.headlines[1], "Talks resume in Geneva & Vienna");
        assert_eq!(feed.headlines.len(), 4);
        assert_eq!(parse("<rss><channel><title>NRK - Toppsaker</title></channel></rss>").name, "NRK");
    }

    #[test]
    fn headlines_are_sentences() {
        assert_eq!(sentence("Storm brings flooding"), "Storm brings flooding.");
        assert_eq!(sentence("Who won? The final"), "Who won? The final.");
        assert_eq!(sentence("U.S. talks resume"), "U.S. talks resume.");
    }

    #[test]
    fn feeds_are_named_or_bare() {
        assert_eq!(source("Norway=https://x.no/feed/").name.as_deref(), Some("Norway"));
        assert_eq!(source("https://x.no/feed?a=b").name, None);
        assert_eq!(source("https://x.no/feed?a=b").url, "https://x.no/feed?a=b");
    }

    #[test]
    fn decodes_entities() {
        assert_eq!(entities("Rock &amp; roll &#8217;s &#x41; &unknown; a & b"), "Rock & roll ’s A &unknown; a & b");
    }
}
