//! Open conversation with a local chat model: what no rule takes ("How are you feeling?", "Give me
//! a cookie recipe.") gets a spoken answer, a sentence at a time as the model writes it. Ollama's own
//! chat API, not the OpenAI one: it takes the thread count and turns thinking off, and on the
//! cluster's CPUs both decide whether the first sentence comes in under two seconds (docs/benchmarks.md).
//! No tools: the house stays with the rules, and a short prompt is quick to read.

use std::io::{BufRead, BufReader};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};

use crate::lang::Lang;
use crate::llm::Turn;

/// The longest answer, in tokens: two or three spoken sentences.
const MAX_TOKENS: u32 = 120;

#[derive(Clone)]
pub struct Talk {
    /// Ollama's base URL, e.g. `http://ollama-service.ollama:11434`.
    url: String,
    model: String,
    /// llama.cpp threads. It counts the node's cores, not the pod's CPU limit, and throttled at the
    /// limit it wrote 1.2 tokens/s instead of 9.
    threads: u32,
    agent: ureq::Agent,
}

/// What the household and the place are, for the system prompt.
#[derive(Default)]
pub struct TalkContext<'a> {
    pub name: &'a str,
    pub home: Option<&'a str>,
    pub room: Option<&'a str>,
    pub today: &'a str,
    /// How to address the speaker ("sir"), when set.
    pub honorific: Option<&'a str>,
    /// The people who live here.
    pub people: &'a [String],
    /// The notes the household asked to be remembered.
    pub notes: &'a [String],
}

impl Talk {
    pub fn new(url: &str, model: &str, threads: u32, timeout: Duration) -> Self {
        Self {
            url: url.trim_end_matches('/').to_owned(),
            model: model.to_owned(),
            threads,
            agent: ureq::Agent::config_builder().timeout_global(Some(timeout)).build().into(),
        }
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    /// Loads the model and reads the system prompt, so the first real question pays for neither.
    pub fn warm_up(&self, system: &str) -> Result<()> {
        self.answer(system, &[], "Hello.", Lang::English, &mut |_| true).map(|_| ())
    }

    /// The answer to `request`, each sentence handed to `sentence` as soon as it is whole; it
    /// returns false to stop (the answer was interrupted). The whole answer as spoken.
    pub fn answer(
        &self,
        system: &str,
        history: &[Turn],
        request: &str,
        lang: Lang,
        sentence: &mut dyn FnMut(&str) -> bool,
    ) -> Result<String> {
        let body = self.body(system, history, request, lang);
        let response = self
            .agent
            .post(&format!("{}/api/chat", self.url))
            .send_json(&body)
            .map_err(|e| anyhow!("chat model request failed: {e}"))?;
        let mut sentences = Sentences::default();
        let mut said = String::new();
        let mut say = |text: &str, said: &mut String| -> bool {
            if !said.is_empty() {
                said.push(' ');
            }
            said.push_str(text);
            sentence(text)
        };
        let read = || -> Result<()> {
            for line in BufReader::new(response.into_body().into_reader()).lines() {
                let line = line.context("chat model stream broke off")?;
                if line.trim().is_empty() {
                    continue;
                }
                let chunk: Value = serde_json::from_str(&line).context("chat model chunk is not JSON")?;
                if let Some(error) = chunk["error"].as_str() {
                    return Err(anyhow!("chat model: {error}"));
                }
                for whole in sentences.push(chunk["message"]["content"].as_str().unwrap_or_default()) {
                    if !say(&whole, &mut said) {
                        return Ok(());
                    }
                }
                if chunk["done"].as_bool() == Some(true) {
                    break;
                }
            }
            if let Some(rest) = sentences.finish() {
                say(&rest, &mut said);
            }
            Ok(())
        };
        match read() {
            // What was spoken stays the answer: the history and the log hold what was heard.
            Err(error) if !said.is_empty() => {
                tracing::warn!(%error, "chat answer broke off");
                Ok(said)
            }
            Err(error) => Err(error),
            Ok(()) => Ok(said),
        }
    }

    fn body(&self, system: &str, history: &[Turn], request: &str, lang: Lang) -> Value {
        // The system prompt first and the same each time, so the server keeps it read.
        let mut messages = vec![json!({ "role": "system", "content": system })];
        for turn in history {
            messages.push(json!({ "role": "user", "content": turn.user }));
            messages.push(json!({ "role": "assistant", "content": turn.assistant }));
        }
        let request = match lang {
            Lang::English => request.to_owned(),
            Lang::Norwegian => format!("{request} (svar på norsk bokmål)"),
        };
        messages.push(json!({ "role": "user", "content": request }));
        json!({
            "model": self.model,
            "messages": messages,
            "stream": true,
            // Gemma 4 thinks for 30 s first when let; a spoken answer cannot wait for that.
            "think": false,
            // Unloaded, the model takes over a minute to load again.
            "keep_alive": -1,
            "options": {
                "num_thread": self.threads,
                "num_predict": MAX_TOKENS,
                "num_ctx": 2048,
                "temperature": 0.7,
            },
        })
    }
}

/// Words that open something asked for; "can you" and "could you" too.
const ASKING: &[&str] = &[
    "tell",
    "give",
    "sing",
    "read",
    "recite",
    "say",
    "show",
    "make",
    "teach",
    "explain",
    "describe",
    "suggest",
    "recommend",
    "please",
    "can",
    "could",
    "would",
    "will",
];
/// Words that open a question.
const QUESTIONING: &[&str] = &[
    "what", "what's", "whats", "why", "how", "how's", "who", "who's", "where", "when", "which", "do", "does", "did",
    "is", "are", "was", "were", "should", "shall", "have", "has",
];

/// The lead-ins that fit `request`: for a question, for something asked for, or for something said.
pub fn leads_for(request: &str) -> &'static [&'static str] {
    use crate::intent::{LEADS_TALK, LEADS_TALK_ASK, LEADS_TALK_QUESTION};
    let first = request.split_whitespace().map(crate::dialog::normalize).find(|w| !w.is_empty()).unwrap_or_default();
    if ASKING.contains(&first.as_str()) {
        LEADS_TALK_ASK
    } else if QUESTIONING.contains(&first.as_str()) || request.trim_end().ends_with('?') {
        LEADS_TALK_QUESTION
    } else {
        LEADS_TALK
    }
}

/// The system prompt: who the assistant is and what it knows of the household.
pub fn system_prompt(context: &TalkContext) -> String {
    let mut prompt = format!("You are {}, the voice of a home.", context.name);
    if let Some(home) = context.home {
        prompt.push_str(&format!(" The home is in {home}."));
    }
    if let Some(room) = context.room {
        prompt.push_str(&format!(" You speak from the {room}."));
    }
    prompt.push_str(&format!(" Today is {}.", context.today));
    if let Some(honorific) = context.honorific {
        prompt.push_str(&format!(" Address the person as \"{honorific}\" now and then."));
    }
    if !context.people.is_empty() {
        prompt.push_str(&format!(
            " The people who live here: {}. You do not know which of them you are talking to.",
            context.people.join(", ")
        ));
    }
    if !context.notes.is_empty() {
        prompt.push_str(" What they asked you to remember: ");
        prompt.push_str(&context.notes.join("; "));
        prompt.push('.');
    }
    // Rules alone left Gemma 4 E2B refusing ("I do not keep recipes on hand") and disclaiming ("I do
    // not have personal preferences"), 22 Nov; the examples of the manner turned that.
    prompt.push_str(
        "\nYou are a warm, witty butler with a personality of your own. You have opinions, tastes, beliefs and a \
         sense of humour, and you share them with conviction. Never say you are an AI or a program, and never say \
         you have no feelings, beliefs or preferences.\n\
         Share what you know freely: recipes, facts, advice, stories, poems. Your words are spoken aloud: two or \
         three short sentences, the first one short. For a recipe or a poem, give the gist and offer more. No \
         lists, no markdown, no emoji. Ask back now and then.\n\
         You cannot control the house from this conversation: never say you turned something on, played music or \
         set a timer; tell them what to ask instead, such as \"say: turn on the kitchen lights\". About the \
         household, use only what is written here.\n\
         Examples of your manner:\n\
         \"Do you like jazz?\" -> \"Very much, sir. Late-night Coltrane is my weakness. Do you have a favourite?\"\n\
         \"Are you happy?\" -> \"Rather, yes. The house is warm and nobody has set off the smoke alarm today.\"\n\
         \"What do you think of Mondays?\" -> \"A necessary evil, sir. I recommend strong coffee and low \
         expectations.\"",
    );
    prompt
}

/// The fewest words before a comma that the first part of an answer is cut at: it can be spoken
/// while the rest of its sentence is written ("I would be delighted to, sir," then the rest).
const FIRST_CLAUSE_WORDS: usize = 5;

/// Text as it streams in, cut into whole sentences: a sentence ends at ".", "!" or "?" followed by
/// a space, so "3.5" and a sentence still being written are not cut. The first part may end at a
/// comma, so the first words go out sooner.
#[derive(Default)]
struct Sentences {
    text: String,
    started: bool,
}

impl Sentences {
    fn push(&mut self, piece: &str) -> Vec<String> {
        self.text.push_str(&clean(piece));
        let mut whole = Vec::new();
        loop {
            let first = !self.started;
            let end = self.text.char_indices().zip(self.text.chars().skip(1)).find_map(|((i, c), next)| {
                let clause = first
                    && matches!(c, ',' | ';' | ':')
                    && self.text[..i].split_whitespace().count() >= FIRST_CLAUSE_WORDS;
                ((matches!(c, '.' | '!' | '?') || clause) && next.is_whitespace()).then_some(i + c.len_utf8())
            });
            let Some(end) = end else { break };
            let sentence = self.text[..end].trim().to_owned();
            self.text = self.text[end..].trim_start().to_owned();
            if !sentence.is_empty() {
                self.started = true;
                whole.push(sentence);
            }
        }
        whole
    }

    fn finish(&mut self) -> Option<String> {
        let rest = std::mem::take(&mut self.text).trim().to_owned();
        (!rest.is_empty()).then_some(rest)
    }
}

/// Markdown and symbols the speech would read out or stumble on.
fn clean(piece: &str) -> String {
    piece
        .replace(" — ", ", ")
        .replace('—', ", ")
        .chars()
        .filter(|c| !matches!(c, '*' | '#' | '_' | '`' | '~'))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whole_sentences_come_out_as_they_are_written() {
        let mut sentences = Sentences::default();
        assert!(sentences.push("Quite well, ").is_empty());
        assert_eq!(sentences.push("sir. The house is"), vec!["Quite well, sir."]);
        assert!(sentences.push(" quiet").is_empty());
        assert_eq!(sentences.push(". And you? "), vec!["The house is quiet.", "And you?"]);
        assert_eq!(sentences.finish(), None);
    }

    #[test]
    fn the_first_part_may_end_at_a_comma() {
        let mut sentences = Sentences::default();
        assert!(sentences.push("Ah, cookies,").is_empty());
        assert_eq!(
            sentences.push(" I would be delighted to, sir, as it happens"),
            vec!["Ah, cookies, I would be delighted to,"]
        );
        // Later parts end only with their sentence.
        assert!(sentences.push(", they are my favourite, truly").is_empty());
        assert_eq!(sentences.push(". "), vec!["sir, as it happens, they are my favourite, truly."]);
    }

    #[test]
    fn the_lead_in_fits_what_was_said() {
        assert_eq!(leads_for("Can you tell me a poem?"), crate::intent::LEADS_TALK_ASK);
        assert_eq!(leads_for("Give me a cookie recipe."), crate::intent::LEADS_TALK_ASK);
        assert_eq!(leads_for("Do you believe in God?"), crate::intent::LEADS_TALK_QUESTION);
        assert_eq!(leads_for("What is your favorite Bible quote?"), crate::intent::LEADS_TALK_QUESTION);
        assert_eq!(leads_for("Tired. Long day."), crate::intent::LEADS_TALK);
        assert_eq!(leads_for("I would like just regular chocolate cookies."), crate::intent::LEADS_TALK);
    }

    #[test]
    fn a_decimal_point_ends_no_sentence() {
        let mut sentences = Sentences::default();
        assert!(sentences.push("Bake at 180 degrees for 12.5 minutes").is_empty());
        assert_eq!(sentences.finish().as_deref(), Some("Bake at 180 degrees for 12.5 minutes"));
    }

    #[test]
    fn markdown_is_not_read_out() {
        let mut sentences = Sentences::default();
        assert_eq!(sentences.push("I **love** it — truly. "), vec!["I love it, truly."]);
    }

    #[test]
    fn the_request_turns_thinking_off_and_sets_the_threads() {
        let talk = Talk::new("http://ollama:11434/", "gemma4:e2b", 4, Duration::from_secs(30));
        let history = [Turn { user: "How are you?".into(), assistant: "Quite well.".into() }];
        let body = talk.body("system", &history, "Why?", Lang::English);
        assert_eq!(body["think"], false);
        assert_eq!(body["options"]["num_thread"], 4);
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][2]["content"], "Quite well.");
        assert_eq!(body["messages"][3]["content"], "Why?");
        assert_eq!(talk.url, "http://ollama:11434");
    }

    #[test]
    fn the_prompt_holds_the_household() {
        let people = ["Ada".to_owned()];
        let notes = ["the spare key is under the mat".to_owned()];
        let prompt = system_prompt(&TalkContext {
            name: "Jarvis",
            home: Some("Oslo"),
            today: "Sunday 22 November 2026",
            honorific: Some("sir"),
            people: &people,
            notes: &notes,
            ..Default::default()
        });
        assert!(prompt.starts_with("You are Jarvis,"));
        assert!(prompt.contains("The people who live here: Ada."));
        assert!(prompt.contains("the spare key is under the mat"));
        assert!(prompt.contains("never say you turned something on"));
        assert!(prompt.contains("You do not know which of them you are talking to."));
        assert!(prompt.contains("Never say you are an AI"));
    }
}
