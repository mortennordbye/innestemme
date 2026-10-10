//! The language model behind the rules: requests the rule-based parser does not recognise go to an
//! OpenAI-compatible chat endpoint (Ollama, llama.cpp, vLLM, or a hosted service) with the skills
//! offered as tools. The model either answers in a sentence or picks a skill and its arguments; the
//! skill then runs exactly as if the rules had found it, so the spoken answer stays the skill's own.

use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};

use crate::intent::{Day, Intent, LightLevel, MusicCommand};
use crate::lang::Lang;
use crate::power::{PowerQuery, When};
use crate::shopping::ListCommand;
use crate::timer::{TimerCommand, Which};
use crate::transit::{Mode, TransitQuery};

#[derive(Clone)]
pub struct Llm {
    /// Base URL of the OpenAI-compatible API, e.g. `http://127.0.0.1:11434/v1`.
    url: String,
    model: String,
    key: Option<String>,
    agent: ureq::Agent,
}

/// One earlier exchange, for follow-ups ("and tomorrow?").
#[derive(Debug, Clone)]
pub struct Turn {
    pub user: String,
    pub assistant: String,
}

#[derive(Debug, PartialEq)]
pub enum Decision {
    /// Speak this.
    Say(String),
    /// Run a skill.
    Act(Intent),
    /// Not meant for the assistant (talk in the room during a follow-up window).
    Ignore,
}

impl Llm {
    pub fn new(url: &str, model: &str, key: Option<String>, timeout: Duration) -> Self {
        Self {
            url: url.trim_end_matches('/').to_owned(),
            model: model.to_owned(),
            key,
            // Error statuses come back as responses: their body says why ("does not support
            // reasoning"), which `decide` needs to retry.
            agent: ureq::Agent::config_builder()
                .timeout_global(Some(timeout))
                .http_status_as_error(false)
                .build()
                .into(),
        }
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    /// Loads the model on the server (an idle Ollama unloads it after a few minutes), so the first
    /// real question does not pay for it.
    pub fn warm_up(&self, system: &str) -> Result<()> {
        // The real system prompt and tools, so the server caches them too.
        self.decide(system, &[], "hello", Lang::English).map(|_| ())
    }

    /// Asks the model what to do with `request`.
    pub fn decide(&self, system: &str, history: &[Turn], request: &str, lang: Lang) -> Result<Decision> {
        // Everything that changes between requests comes after the system prompt and the tools,
        // so the server's prompt cache keeps them. The listener knows the language.
        let mut messages = vec![json!({ "role": "system", "content": system })];
        for turn in history {
            messages.push(json!({ "role": "user", "content": turn.user }));
            messages.push(json!({ "role": "assistant", "content": turn.assistant }));
        }
        let reply_in = match lang {
            Lang::English => "(reply in English)",
            Lang::Norwegian => "(svar på norsk bokmål)",
        };
        messages.push(json!({ "role": "user", "content": format!("{request} {reply_in}") }));
        let mut body = json!({
            "model": self.model,
            "messages": messages,
            "tools": tools(),
            "tool_choice": "auto",
            "temperature": 0.2,
            "max_tokens": 200,
            "stream": false,
            // Reasoning models (Qwen3) think at length first; a spoken answer cannot wait for that.
            "reasoning_effort": "none",
        });
        let response = match self.post(&body) {
            // Servers that reject the reasoning switch for their model get the request without it.
            Err(error) if error.to_string().contains("reasoning") => {
                body.as_object_mut().unwrap().remove("reasoning_effort");
                self.post(&body)?
            }
            other => other?,
        };
        parse_reply(&response["choices"][0]["message"])
    }

    fn post(&self, body: &Value) -> Result<Value> {
        let mut call = self.agent.post(&format!("{}/chat/completions", self.url));
        if let Some(key) = &self.key {
            call = call.header("Authorization", &format!("Bearer {key}"));
        }
        let mut response = call.send_json(body).map_err(|e| anyhow!("language model request failed: {e}"))?;
        let status = response.status();
        if !status.is_success() {
            let text = response.body_mut().read_to_string().unwrap_or_default();
            return Err(anyhow!("language model HTTP {}: {}", status.as_u16(), text.trim()));
        }
        response.body_mut().read_json().context("language model answer is not JSON")
    }
}

/// The model's message: a tool call becomes a skill, text becomes speech.
fn parse_reply(message: &Value) -> Result<Decision> {
    if let Some(call) = message["tool_calls"].as_array().and_then(|calls| calls.first()) {
        let name = call["function"]["name"].as_str().unwrap_or_default();
        // Arguments arrive as a JSON string (OpenAI) or an object (some servers).
        let args = match &call["function"]["arguments"] {
            Value::String(s) => serde_json::from_str(s).unwrap_or(Value::Null),
            other => other.clone(),
        };
        return tool_to_decision(name, &args);
    }
    let raw = message["content"].as_str().unwrap_or_default();
    // Small models sometimes write the call as text: a bare tool name, or the call's JSON.
    if let Some(start) = raw.find('{') {
        if let Ok(call) = serde_json::from_str::<Value>(raw[start..].trim_end_matches(|c| c != '}')) {
            if let Some(name) = call["name"].as_str() {
                let args = call.get("arguments").or_else(|| call.get("parameters")).cloned().unwrap_or(Value::Null);
                if let Ok(decision) = tool_to_decision(name, &args) {
                    return Ok(decision);
                }
            }
        }
    }
    let text = strip_thinking(raw);
    match text.trim_end_matches('.').to_lowercase().as_str() {
        "" => Err(anyhow!("the language model returned nothing")),
        "ignore" => Ok(Decision::Ignore),
        "joke" => Ok(Decision::Act(Intent::Joke)),
        _ => Ok(Decision::Say(text)),
    }
}

fn tool_to_decision(name: &str, args: &Value) -> Result<Decision> {
    // Small models leave quotes and commas around values ("living room”,").
    let text = |key: &str| {
        args[key]
            .as_str()
            .map(|s| s.trim_matches(|c: char| c.is_whitespace() || "\"'“”‘’,.".contains(c)))
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    Ok(match name {
        "weather" => Decision::Act(Intent::Weather {
            place: text("place"),
            day: if text("day").as_deref() == Some("tomorrow") { Day::Tomorrow } else { Day::Today },
        }),
        // One tool for switching, levels and scenes; "light_settings" and "scene" are the older
        // separate tools, still accepted from models that write calls as text.
        "lights" | "light_settings" | "scene" => {
            let target = text("target").or_else(|| text("room")).unwrap_or_default();
            let scene = text("scene").or_else(|| (name == "scene").then(|| text("name")).flatten());
            let brightness = args["brightness"]
                .as_f64()
                .or_else(|| text("brightness").and_then(|s| s.trim_end_matches('%').parse().ok()));
            let level = if let Some(b) = brightness {
                Some(LightLevel::Percent(b.clamp(0.0, 100.0).round() as u8))
            } else if let Some(color) = text("color") {
                Some(LightLevel::Color(color.to_lowercase()))
            } else {
                match text("change").as_deref() {
                    Some("brighter") => Some(LightLevel::Brighter),
                    Some("dimmer") => Some(LightLevel::Dimmer),
                    Some("warmer") => Some(LightLevel::Warm),
                    Some("cooler") => Some(LightLevel::Cool),
                    _ => None,
                }
            };
            if let Some(scene) = scene {
                let request = if target.is_empty() { scene } else { format!("{scene} in {target}") };
                Decision::Act(Intent::Scene(request))
            } else if let Some(level) = level {
                Decision::Act(Intent::LightLevel { level, target })
            } else {
                let on = args["on"].as_bool().unwrap_or(text("state").as_deref() != Some("off"));
                Decision::Act(Intent::Lights { on, target })
            }
        }
        "joke" => Decision::Act(Intent::Joke),
        "news" => Decision::Act(Intent::News),
        "shopping_list" => {
            // Items as a list, or one string ("milk, eggs") from models that ignore the schema.
            let items: Vec<String> = match &args["items"] {
                Value::Array(list) => list.iter().filter_map(Value::as_str).map(str::to_owned).collect(),
                Value::String(one) => one.split(',').map(str::to_owned).collect(),
                _ => Vec::new(),
            };
            let items: Vec<String> = items
                .iter()
                .map(|i| i.trim())
                .filter(|i| !i.is_empty())
                .map(|i| {
                    let mut chars = i.chars();
                    chars.next().map(|c| c.to_uppercase().chain(chars).collect()).unwrap_or_default()
                })
                .collect();
            let command = match text("action").as_deref().unwrap_or("read") {
                "add" => ListCommand::Add(items),
                "remove" if !items.is_empty() => ListCommand::Remove(items),
                "clear" => ListCommand::Clear,
                _ => ListCommand::Read,
            };
            Decision::Act(Intent::ShoppingList(command))
        }
        "electricity_price" => {
            let when = match text("when").as_deref() {
                Some("tonight") => When::Tonight,
                Some("tomorrow") => When::Tomorrow,
                _ => When::Today,
            };
            Decision::Act(Intent::Power(match text("question").as_deref() {
                Some("cheapest") => PowerQuery::Cheapest(when),
                Some("most_expensive") => PowerQuery::Dearest(when),
                _ => PowerQuery::Now,
            }))
        }
        "who_is_home" => Decision::Act(Intent::WhosHome(text("name").map(|n| n.to_lowercase()))),
        "departures" => {
            let mode = match text("mode").as_deref() {
                Some("bus") => Mode::Bus,
                Some("tram") => Mode::Tram,
                Some("metro") => Mode::Metro,
                Some("train") => Mode::Rail,
                Some("ferry") => Mode::Ferry,
                _ => Mode::Any,
            };
            Decision::Act(Intent::Transit(TransitQuery {
                mode,
                destination: text("destination").map(|d| d.to_lowercase()),
            }))
        }
        "timer" => {
            let number =
                |key: &str| args[key].as_f64().or_else(|| text(key).and_then(|s| s.parse().ok())).unwrap_or(0.0);
            let seconds = (number("hours") * 3600.0 + number("minutes") * 60.0 + number("seconds")).round() as u32;
            let seconds = (seconds > 0).then_some(seconds);
            let which = Which {
                name: text("name").filter(|n| n != "all"),
                all: args["all"].as_bool().unwrap_or(false) || text("name").as_deref() == Some("all"),
                ..Which::default()
            };
            let command = match text("action").as_deref().unwrap_or("start") {
                "cancel" | "stop" => TimerCommand::Cancel(which),
                "pause" => TimerCommand::Pause(which),
                "resume" => TimerCommand::Resume(which),
                "add" => TimerCommand::Add { seconds: seconds.unwrap_or(60), which },
                "remaining" | "status" => TimerCommand::Remaining(which),
                "remind" => TimerCommand::Remind { seconds, message: text("message") },
                _ => TimerCommand::Start { seconds, name: which.name },
            };
            Decision::Act(Intent::Timer(command))
        }
        "music" => {
            let command = match text("action").as_deref().unwrap_or("play") {
                "pause" | "stop" => MusicCommand::Pause,
                "resume" => MusicCommand::Resume,
                "next" => MusicCommand::Next,
                "previous" => MusicCommand::Previous,
                "louder" => MusicCommand::Louder,
                "quieter" => MusicCommand::Quieter,
                _ => match (text("kind").as_deref(), text("query")) {
                    (Some("liked"), _) | (_, None) => MusicCommand::Liked,
                    (Some("playlist"), Some(q)) => MusicCommand::Playlist(q),
                    (_, Some(q)) => MusicCommand::Song(q),
                },
            };
            Decision::Act(Intent::Music(command))
        }
        "ignore" => Decision::Ignore,
        other => return Err(anyhow!("the language model called an unknown tool {other:?}")),
    })
}

/// Removes `<think>...</think>` blocks some models emit, and markdown a speaker cannot say.
fn strip_thinking(text: &str) -> String {
    let mut out = text.to_owned();
    while let (Some(start), Some(end)) = (out.find("<think>"), out.find("</think>")) {
        if end < start {
            break;
        }
        out.replace_range(start..end + "</think>".len(), "");
    }
    out.replace(['*', '#', '`'], "").split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The skills, as OpenAI function tools. Every token here is in every request and makes each
/// generated token slower on a CPU, so descriptions are short; cutting them to a few words, or
/// merging tools, cost half the answers in `llm_eval` (calls written as plain text instead).
pub fn tools() -> Value {
    let tool = |name: &str, description: &str, parameters: Value| json!({ "type": "function", "function": { "name": name, "description": description, "parameters": parameters } });
    let none = json!({ "type": "object", "properties": {} });
    json!([
        tool(
            "weather",
            "Weather now and today, or tomorrow's forecast, for a place.",
            json!({ "type": "object", "properties": {
            "place": { "type": "string", "description": "City; omit for home" },
            "day": { "type": "string", "enum": ["today", "tomorrow"] }
        } })
        ),
        tool(
            "lights",
            "Switch lights on or off in a room or by light name.",
            json!({ "type": "object", "required": ["target", "on"], "properties": {
            "target": { "type": "string", "description": "Room or light name, e.g. kitchen" },
            "on": { "type": "boolean" }
        } })
        ),
        tool(
            "light_settings",
            "Light brightness, brighter or dimmer, warmer or cooler white, or a colour.",
            json!({ "type": "object", "properties": {
            "target": { "type": "string", "description": "Room or light; omit for here" },
            "brightness": { "type": "number", "description": "0-100" },
            "change": { "type": "string", "enum": ["brighter", "dimmer", "warmer", "cooler"] },
            "color": { "type": "string", "description": "English colour name" }
        } })
        ),
        tool(
            "scene",
            "Light scene or mood: relax, read, concentrate, energize, nightlight, dreamy dusk.",
            json!({ "type": "object", "required": ["name"], "properties": {
            "name": { "type": "string" },
            "room": { "type": "string", "description": "Omit for here" }
        } })
        ),
        tool(
            "music",
            "Play music (song, artist, playlist, liked songs) or control playback.",
            json!({ "type": "object", "required": ["action"], "properties": {
            "action": { "type": "string", "enum": ["play", "pause", "resume", "next", "previous", "louder", "quieter"] },
            "kind": { "type": "string", "enum": ["song", "artist", "playlist", "liked"] },
            "query": { "type": "string" }
        } })
        ),
        tool(
            "timer",
            "Timers and reminders: start, remind, cancel, pause, resume, add time, or time remaining.",
            json!({ "type": "object", "required": ["action"], "properties": {
            "action": { "type": "string", "enum": ["start", "remind", "cancel", "pause", "resume", "add", "remaining"] },
            "hours": { "type": "number" },
            "minutes": { "type": "number" },
            "seconds": { "type": "number" },
            "name": { "type": "string", "description": "What it is for, e.g. pasta; all for every timer" },
            "message": { "type": "string", "description": "For remind: what to remind about" }
        } })
        ),
        tool(
            "departures",
            "When the next bus, tram, metro, train or ferry leaves near home.",
            json!({ "type": "object", "properties": {
            "mode": { "type": "string", "enum": ["any", "bus", "tram", "metro", "train", "ferry"] },
            "destination": { "type": "string", "description": "Omit if not said" }
        } })
        ),
        tool(
            "shopping_list",
            "Shopping list: add, remove, read or clear. Also for things to buy or run out of.",
            json!({ "type": "object", "required": ["action"], "properties": {
            "action": { "type": "string", "enum": ["add", "remove", "read", "clear"] },
            "items": { "type": "array", "items": { "type": "string" } }
        } })
        ),
        tool(
            "electricity_price",
            "Electricity price now, or the cheapest or most expensive hour.",
            json!({ "type": "object", "properties": {
            "question": { "type": "string", "enum": ["now", "cheapest", "most_expensive"] },
            "when": { "type": "string", "enum": ["today", "tonight", "tomorrow"] }
        } })
        ),
        tool(
            "who_is_home",
            "Who is home, or whether one person is.",
            json!({ "type": "object", "properties": {
            "name": { "type": "string", "description": "Omit for everyone" }
        } })
        ),
        tool("joke", "Tell a joke.", none.clone()),
        tool("news", "Latest news headlines.", none.clone()),
        tool("ignore", "Words not meant for the assistant (people talking to each other, TV).", none),
    ])
}

/// The standing instructions: who the assistant is, where it lives, how it talks.
/// `today` is the date only: anything that changes more often would defeat the prompt cache
/// (the time of day is a rule of its own).
pub fn system_prompt(name: &str, home: Option<&str>, room: Option<&str>, today: &str) -> String {
    let home = home.map(|h| format!(" The home is in {h}.")).unwrap_or_default();
    let room = room.map(|r| format!(" You are in the {r}; \"here\" means the {r}.")).unwrap_or_default();
    format!(
        "You are {name}, a voice assistant that controls a home.{home}{room} Today is {today}.\n\
         Rule 1: for the weather, lights, brightness or colour, a light scene or mood, music, a timer or \
         reminder, the shopping list, departures, electricity prices, who is home, or something funny, call \
         the matching tool. Do not describe the action; the tool does it and speaks.\n\
         Rule 2: if the words were addressed to another person or are background talk, call ignore.\n\
         Rule 3: otherwise answer from your own knowledge in one short spoken sentence, no markdown. \
         Never say you did something without calling a tool.\n\
         Examples: \"cheer me up\" -> joke. \"we're out of coffee\" -> shopping_list add coffee. \
         \"make it cosy in here\" -> scene relax. \"when should I run the dishwasher\" -> electricity_price \
         cheapest. \"tell me when the eggs are done in 7 minutes\" -> timer. \"Anna, where are my keys?\" -> \
         ignore. \"it's freezing in here\" -> answer, not weather.\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_calls_become_skills() {
        let call = |name: &str, args: &str| json!({ "tool_calls": [{ "type": "function", "function": { "name": name, "arguments": args } }] });
        assert_eq!(
            parse_reply(&call("weather", r#"{"place":"Bergen","day":"tomorrow"}"#)).unwrap(),
            Decision::Act(Intent::Weather { place: Some("Bergen".into()), day: Day::Tomorrow })
        );
        assert_eq!(
            parse_reply(&call("lights", r#"{"target":"kitchen","on":false}"#)).unwrap(),
            Decision::Act(Intent::Lights { on: false, target: "kitchen".into() })
        );
        assert_eq!(
            parse_reply(&call("music", r#"{"action":"play","kind":"artist","query":"Ghost"}"#)).unwrap(),
            Decision::Act(Intent::Music(MusicCommand::Song("Ghost".into())))
        );
        assert_eq!(
            parse_reply(&call("music", r#"{"action":"next"}"#)).unwrap(),
            Decision::Act(Intent::Music(MusicCommand::Next))
        );
        assert_eq!(
            parse_reply(&call("timer", r#"{"action":"remind","minutes":20,"message":"take the laundry out"}"#))
                .unwrap(),
            Decision::Act(Intent::Timer(TimerCommand::Remind {
                seconds: Some(1200),
                message: Some("take the laundry out".into())
            }))
        );
        assert_eq!(
            parse_reply(&call("timer", r#"{"action":"start","minutes":"7","name":"eggs"}"#)).unwrap(),
            Decision::Act(Intent::Timer(TimerCommand::Start { seconds: Some(420), name: Some("eggs".into()) }))
        );
        assert_eq!(
            parse_reply(&call("shopping_list", r#"{"action":"add","items":["coffee","oat milk"]}"#)).unwrap(),
            Decision::Act(Intent::ShoppingList(ListCommand::Add(vec!["Coffee".into(), "Oat milk".into()])))
        );
        assert_eq!(
            parse_reply(&call("shopping_list", r#"{"action":"add","items":"milk, eggs"}"#)).unwrap(),
            Decision::Act(Intent::ShoppingList(ListCommand::Add(vec!["Milk".into(), "Eggs".into()])))
        );
        assert_eq!(
            parse_reply(&call("scene", r#"{"name":"relax","room":"bedroom"}"#)).unwrap(),
            Decision::Act(Intent::Scene("relax in bedroom".into()))
        );
        assert_eq!(
            parse_reply(&call("light_settings", r#"{"target":"kitchen”,","brightness":"40%"}"#)).unwrap(),
            Decision::Act(Intent::LightLevel { level: LightLevel::Percent(40), target: "kitchen".into() })
        );
        assert_eq!(
            parse_reply(&call("electricity_price", r#"{"question":"cheapest","when":"tonight"}"#)).unwrap(),
            Decision::Act(Intent::Power(PowerQuery::Cheapest(When::Tonight)))
        );
        assert_eq!(
            parse_reply(&call("who_is_home", r#"{"name":"Ingrid"}"#)).unwrap(),
            Decision::Act(Intent::WhosHome(Some("ingrid".into())))
        );
        assert_eq!(parse_reply(&call("ignore", "{}")).unwrap(), Decision::Ignore);
        // Some servers send the arguments as an object.
        let object = json!({ "tool_calls": [{ "function": { "name": "joke", "arguments": {} } }] });
        assert_eq!(parse_reply(&object).unwrap(), Decision::Act(Intent::Joke));
    }

    #[test]
    fn calls_written_as_text_still_count() {
        let text = |t: &str| parse_reply(&json!({ "content": t })).unwrap();
        assert_eq!(text("ignore"), Decision::Ignore);
        assert_eq!(
            text(r#"{"name": "music", "arguments": {"action": "quieter"}}"#),
            Decision::Act(Intent::Music(MusicCommand::Quieter))
        );
        assert_eq!(
            text(r#"lights {"name": "lights", "arguments": {"target": "living room", "on": false}}"#),
            Decision::Act(Intent::Lights { on: false, target: "living room".into() })
        );
    }

    #[test]
    fn text_answers_lose_thinking_and_markdown() {
        let message = json!({ "content": "<think>\n\n</think>\n\n**Paris** is the capital of France." });
        assert_eq!(parse_reply(&message).unwrap(), Decision::Say("Paris is the capital of France.".into()));
        assert!(parse_reply(&json!({ "content": "" })).is_err());
    }
}
