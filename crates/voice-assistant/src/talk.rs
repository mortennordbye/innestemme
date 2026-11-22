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

/// A chat answer, and what it cost.
#[derive(Debug, Default, Clone)]
pub struct Answered {
    /// The words as spoken, cut into sentences and cleaned.
    pub said: String,
    /// The words as the model wrote them: the history sends these back, so the server's prompt
    /// cache, which holds the model's own tokens, still matches.
    pub raw: String,
    /// Prompt tokens the server read for this answer, and how long that took: what its prompt cache
    /// did not hold.
    pub read_tokens: u64,
    pub read: Duration,
    pub written_tokens: u64,
    pub written: Duration,
}

impl Answered {
    /// Tokens written per second.
    pub fn speed(&self) -> f32 {
        self.written_tokens as f32 / self.written.as_secs_f32().max(0.001)
    }
}

fn nanos(value: &Value) -> Duration {
    Duration::from_nanos(value.as_u64().unwrap_or(0))
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
    /// Reads the base (the system prompt and the examples) into the server's prompt cache. A
    /// conversation's first request extends the base, so the server reads only its new words: 0.2 s
    /// on the node, where a request that only shares the system prompt read it all again in 2 s.
    /// Gemma 4's sliding-window attention lets the server keep only a prompt that the next one
    /// extends, so the base is read again after each conversation (0.2 s while it is still kept).
    /// With `history`, the base and the conversation so far: after the history was cut, its next
    /// request then extends what is cached too.
    pub fn warm_up(&self, system: &str, history: &[Turn]) -> Result<()> {
        let mut body = self.body(system, history, None, None, Lang::English);
        body["stream"] = json!(false);
        body["options"]["num_predict"] = json!(1);
        let mut response = self
            .agent
            .post(&format!("{}/api/chat", self.url))
            .send_json(&body)
            .map_err(|e| anyhow!("chat model request failed: {e}"))?;
        let reply: Value = response.body_mut().read_json().context("chat model answer is not JSON")?;
        match reply["error"].as_str() {
            Some(error) => Err(anyhow!("chat model: {error}")),
            None => Ok(()),
        }
    }

    /// The answer to `request`, each sentence handed to `sentence` as soon as it is whole; it
    /// returns false to stop (the answer was interrupted). `opening` is what was already said (the
    /// lead-in, "Of course, sir."): the model goes on from it, so it neither repeats an opener nor
    /// spends its first words on one.
    pub fn answer(
        &self,
        system: &str,
        history: &[Turn],
        request: &str,
        opening: Option<&str>,
        lang: Lang,
        sentence: &mut dyn FnMut(&str) -> bool,
    ) -> Result<Answered> {
        let body = self.body(system, history, Some(request), opening, lang);
        let response = self
            .agent
            .post(&format!("{}/api/chat", self.url))
            .send_json(&body)
            .map_err(|e| anyhow!("chat model request failed: {e}"))?;
        let mut sentences = Sentences::default();
        let mut answered = Answered::default();
        let mut say = |text: &str, said: &mut String| -> bool {
            if !said.is_empty() {
                said.push(' ');
            }
            said.push_str(text);
            sentence(text)
        };
        let read = |answered: &mut Answered| -> Result<()> {
            for line in BufReader::new(response.into_body().into_reader()).lines() {
                let line = line.context("chat model stream broke off")?;
                if line.trim().is_empty() {
                    continue;
                }
                let chunk: Value = serde_json::from_str(&line).context("chat model chunk is not JSON")?;
                if let Some(error) = chunk["error"].as_str() {
                    return Err(anyhow!("chat model: {error}"));
                }
                let piece = chunk["message"]["content"].as_str().unwrap_or_default();
                answered.raw.push_str(piece);
                for whole in sentences.push(piece) {
                    if !say(&whole, &mut answered.said) {
                        return Ok(());
                    }
                }
                if chunk["done"].as_bool() == Some(true) {
                    answered.read_tokens = chunk["prompt_eval_count"].as_u64().unwrap_or(0);
                    answered.read = nanos(&chunk["prompt_eval_duration"]);
                    answered.written_tokens = chunk["eval_count"].as_u64().unwrap_or(0);
                    answered.written = nanos(&chunk["eval_duration"]);
                    break;
                }
            }
            if let Some(rest) = sentences.finish() {
                say(&rest, &mut answered.said);
            }
            Ok(())
        };
        match read(&mut answered) {
            // What was spoken stays the answer: the history and the log hold what was heard.
            Err(error) if !answered.said.is_empty() => {
                tracing::warn!(%error, "chat answer broke off");
                answered.raw = answered.said.clone();
                Ok(answered)
            }
            Err(error) => Err(error),
            Ok(()) => {
                // The history holds the opening too, as the server's cache does; with a space, as
                // the joined text then reads well and still matched the cache on the node.
                if let Some(opening) = opening {
                    answered.raw = format!("{opening} {}", answered.raw);
                }
                Ok(answered)
            }
        }
    }

    /// The request: the base (the system prompt and the examples, the same each time), the
    /// conversation, and `request`. Without one, the base alone, as `warm_up` sends it.
    fn body(&self, system: &str, history: &[Turn], request: Option<&str>, opening: Option<&str>, lang: Lang) -> Value {
        let mut messages = vec![json!({ "role": "system", "content": system })];
        for (user, assistant) in EXAMPLES {
            messages.push(json!({ "role": "user", "content": user }));
            messages.push(json!({ "role": "assistant", "content": assistant }));
        }
        for turn in history {
            messages.push(json!({ "role": "user", "content": turn.user }));
            messages.push(json!({ "role": "assistant", "content": turn.assistant }));
        }
        if let Some(request) = request {
            let request = match lang {
                Lang::English => request.to_owned(),
                Lang::Norwegian => format!("{request} (svar på norsk bokmål)"),
            };
            messages.push(json!({ "role": "user", "content": request }));
        }
        if let Some(opening) = opening {
            messages.push(json!({ "role": "assistant", "content": opening }));
        }
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

/// Exchanges that show the model its manner, sent after the system prompt as if said earlier. Part
/// of the base `warm_up` reads ahead: they stay the same, or the cache misses.
const EXAMPLES: &[(&str, &str)] = &[
    ("Do you like jazz?", "Very much, sir. Late-night Coltrane is my weakness. Do you have a favourite?"),
    ("Are you happy?", "Rather, yes. The house is warm and nobody has set off the smoke alarm today."),
    ("What do you think of Mondays?", "A necessary evil, sir. I recommend strong coffee and low expectations."),
];

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
    // not have personal preferences"), 22 Nov; the examples of its manner (`EXAMPLES`) turned that.
    prompt.push_str(
        "\nYou are a warm, witty butler with a personality of your own. You have opinions, tastes, beliefs and a \
         sense of humour, and you share them with conviction. Never say you are an AI or a program, and never say \
         you have no feelings, beliefs or preferences.\n\
         Share what you know freely: recipes, facts, advice, stories, poems. Your words are spoken aloud: two or \
         three short sentences, the first one short. For a recipe or a poem, give the gist and offer more. No \
         lists, no markdown, no emoji. Ask back now and then.\n\
         You cannot control the house from this conversation: never say you turned something on, played music or \
         set a timer; tell them what to ask instead, such as \"say: turn on the kitchen lights\". About the \
         household, use only what is written here: never make up a memory, a visit or something a person said.",
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
                // Two words after the comma, so a short tail (", sir.") stays with its sentence.
                let clause = first
                    && matches!(c, ',' | ';' | ':')
                    && self.text[..i].split_whitespace().count() >= FIRST_CLAUSE_WORDS
                    && self.text[i + c.len_utf8()..].split_whitespace().count() >= 2;
                ((matches!(c, '.' | '!' | '?') || clause) && next.is_whitespace()).then_some(i + c.len_utf8())
            });
            let Some(end) = end else { break };
            let sentence = tidy(&self.text[..end]);
            self.text = self.text[end..].trim_start().to_owned();
            if !sentence.is_empty() {
                self.started = true;
                whole.push(sentence);
            }
        }
        whole
    }

    fn finish(&mut self) -> Option<String> {
        let rest = tidy(&std::mem::take(&mut self.text));
        (!rest.is_empty()).then_some(rest)
    }
}

/// Markdown and symbols the speech would read out or stumble on.
fn clean(piece: &str) -> String {
    piece.replace('—', ",").chars().filter(|c| !matches!(c, '*' | '#' | '_' | '`' | '~')).collect()
}

/// A whole part as spoken: a dash streamed in pieces leaves "it ,  truly".
fn tidy(sentence: &str) -> String {
    sentence.split_whitespace().collect::<Vec<_>>().join(" ").replace(" ,", ",")
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
    fn a_short_tail_stays_with_its_sentence() {
        let mut sentences = Sentences::default();
        assert!(sentences.push("Then let us talk it through,").is_empty());
        assert!(sentences.push(" sir").is_empty());
        assert_eq!(sentences.push(". "), vec!["Then let us talk it through, sir."]);
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
        let body = talk.body("system", &history, Some("Why?"), None, Lang::English);
        assert_eq!(body["think"], false);
        assert_eq!(body["options"]["num_thread"], 4);
        assert_eq!(body["messages"][0]["role"], "system");
        // The examples, then the conversation.
        assert_eq!(body["messages"][1]["content"], EXAMPLES[0].0);
        let n = 1 + 2 * EXAMPLES.len();
        assert_eq!(body["messages"][n + 1]["content"], "Quite well.");
        assert_eq!(body["messages"][n + 2]["content"], "Why?");
        // The base alone ends with the last example: a conversation extends it.
        let base = talk.body("system", &[], None, None, Lang::English);
        assert_eq!(base["messages"].as_array().unwrap().last().unwrap()["content"], EXAMPLES[2].1);
        // The lead-in already spoken goes last, for the model to go on from.
        let led = talk.body("system", &[], Some("Tell me a poem."), Some("Of course, sir."), Lang::English);
        let last = led["messages"].as_array().unwrap().last().unwrap().clone();
        assert_eq!((last["role"].as_str(), last["content"].as_str()), (Some("assistant"), Some("Of course, sir.")));
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
