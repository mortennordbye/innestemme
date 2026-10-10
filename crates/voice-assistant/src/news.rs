//! The latest headlines from an RSS feed (BBC News in English, NRK in Norwegian by default), for
//! "what's the news?" and the morning briefing. Headlines are live data: they are synthesized every
//! time and never kept by the speech cache.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

use crate::lang::Lang;

pub const ENGLISH_FEED: &str = "https://feeds.bbci.co.uk/news/rss.xml";
pub const NORWEGIAN_FEED: &str = "https://www.nrk.no/toppsaker.rss";
/// Headlines read out; more is a news broadcast, not an answer.
const HEADLINES: usize = 3;
/// Feeds change every few minutes at most; a briefing and a question soon after reuse one fetch.
const CACHE: Duration = Duration::from_secs(600);

pub struct News {
    agent: ureq::Agent,
    english: String,
    norwegian: String,
    cache: HashMap<String, (Instant, Feed)>,
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
    pub fn new(user_agent: &str, english: &str, norwegian: &str) -> Self {
        Self {
            agent: crate::geo::agent(user_agent),
            english: english.to_owned(),
            norwegian: norwegian.to_owned(),
            cache: HashMap::new(),
        }
    }

    /// "Here are the latest headlines from BBC News. ..."
    pub fn answer(&mut self, lang: Lang) -> Result<Headlines> {
        let feed = self.feed(lang)?;
        let lead = if lang == Lang::Norwegian {
            format!("Her er de siste nyhetene fra {}.", feed.name)
        } else {
            format!("Here are the latest headlines from {}.", feed.name)
        };
        Ok(headlines(lead, &feed.headlines))
    }

    /// The briefing's part: "In the news: ..."
    pub fn briefing(&mut self, lang: Lang) -> Result<Headlines> {
        let feed = self.feed(lang)?;
        let lead = if lang == Lang::Norwegian { "I nyhetene:" } else { "In the news:" };
        Ok(headlines(lead.to_owned(), &feed.headlines))
    }

    fn feed(&mut self, lang: Lang) -> Result<Feed> {
        let url = if lang == Lang::Norwegian { &self.norwegian } else { &self.english }.clone();
        if let Some((at, feed)) = self.cache.get(&url) {
            if at.elapsed() < CACHE {
                return Ok(feed.clone());
            }
        }
        let xml = self
            .agent
            .get(&url)
            .call()
            .and_then(|mut response| response.body_mut().read_to_string())
            .with_context(|| format!("fetching {url}"))?;
        let feed = parse(&xml);
        if feed.headlines.is_empty() {
            bail!("{url}: no headlines");
        }
        self.cache.insert(url, (Instant::now(), feed.clone()));
        Ok(feed)
    }
}

fn headlines(lead: String, headlines: &[String]) -> Headlines {
    let live: Vec<String> = headlines.iter().take(HEADLINES).map(|h| sentence(h)).collect();
    let text = std::iter::once(lead).chain(live.iter().cloned()).collect::<Vec<_>>().join(" ");
    Headlines { text, live }
}

/// A headline as one spoken sentence: closing punctuation, and no full stop inside to split it.
fn sentence(headline: &str) -> String {
    let text = headline.trim().replace(". ", ", ");
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
    fn three_headlines_as_sentences() {
        let h = headlines("In the news:".into(), &parse(RSS).headlines);
        assert_eq!(
            h.live,
            [
                "Storm brings flooding to coastal towns.",
                "Talks resume in Geneva & Vienna.",
                "Who won? The final in 3 minutes."
            ]
        );
        assert!(h.text.starts_with("In the news: Storm brings"));
    }

    #[test]
    fn decodes_entities() {
        assert_eq!(entities("Rock &amp; roll &#8217;s &#x41; &unknown; a & b"), "Rock & roll ’s A &unknown; a & b");
    }
}
