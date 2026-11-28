//! Rule-based intent parsing, English and Norwegian. A placeholder for the LLM step: enough to
//! route a weather question and pull out the place and day.
//!
//! The order of the rules in `rules` is part of the logic: each skill's own rules live in its
//! module, and the router asks them in turn.

mod follow_on;
mod lights;
mod meta;
mod music;
mod people;
mod weather;

pub use follow_on::{follow_on, open_to_follow_on};
use lights::{is_lights_question, level_target, light_level, mood, scheduled_switch, without_opening, LIGHT_WORDS};
pub use lights::{says_off, LightLevel};
pub use meta::{Meta, LEADS, LEADS_TALK, LEADS_TALK_ASK, LEADS_TALK_QUESTION, TALK_FILLERS};
use music::{music, NOT_FOR_YOU, PLAY_WORDS, POLITE};
pub use music::{MusicCommand, Repeat};
use people::{where_is, whos_home};
pub use weather::{day_named, Day, Focus};
use weather::{day_of, focus_of, place_of, wants_clothes, WEATHER_STEMS, WEATHER_WORDS};

use crate::dialog::normalize;
use crate::house::{self, HouseCommand, Routine};
use crate::power::{self, PowerQuery};
use crate::shopping::{self, ListCommand};
use crate::smalltalk::Chat;
use crate::timer::{self, TimerCommand};
use crate::transit::{self, TransitQuery};
use crate::tv::{self, TvCommand};

#[derive(Debug, PartialEq, Eq)]
pub enum Intent {
    /// `place` is `None` when the request names no place.
    Weather {
        place: Option<String>,
        day: Day,
        focus: Focus,
    },
    Joke,
    /// "What time is it?", "hva er klokka?"
    Time,
    /// "What's the date?", "what day is it?", "hvilken dag er det?"
    Date,
    Music(MusicCommand),
    /// Timers and reminders.
    Timer(TimerCommand),
    /// "When's the next bus": departures from the stops near home.
    Transit(TransitQuery),
    /// Switch lights; `target` is the request text, resolved against Home Assistant later.
    Lights {
        on: bool,
        target: String,
    },
    /// "Which lights are on?", "is anything left on?", "is the kitchen light on?"; `target` is the
    /// request text, resolved against Home Assistant later.
    LightsStatus {
        target: String,
    },
    /// Brightness or colour: "dim the living room to 30%", "make the bedroom lights red"; `target`
    /// is the request without the level words, resolved later like [`Intent::Lights`].
    LightLevel {
        level: LightLevel,
        target: String,
    },
    /// Electricity prices.
    Power(PowerQuery),
    /// "Who's home?" (`None`), "is Ingrid home?" (`Some("ingrid")`).
    WhosHome(Option<String>),
    /// "Where is Ingrid" (`zone: None`), "is Ingrid at work" (`zone: Some("work")`), "when will
    /// Ingrid be home" (`arriving`); the name is matched against Home Assistant's people later.
    WhereIs {
        name: String,
        zone: Option<String>,
        arriving: bool,
    },
    /// "Convert 10 miles to kilometres", "how many grams in a pound".
    Convert(crate::units::Conversion),
    /// Personal notes: "remember that the spare key is in the kitchen drawer", "what did I say
    /// about the spare key", "forget the spare key". "Where is the spare key" is answered from a
    /// note when there is one, before people and places.
    Memory(crate::memory::MemoryCommand),
    /// "Good morning": the time, the weather at home and the shopping list in a few sentences.
    Briefing,
    /// "What's the news?": the latest headlines.
    News,
    /// "Who is ...", "what is ...", "tell me about ...": a topic to look up on Wikipedia.
    Lookup(String),
    /// "What's 5 plus 5": the sum as said, worked out when answering.
    Calculate(String),
    /// "How far is it from Oslo to Shanghai?"
    Distance(crate::distance::Query),
    /// The shopping list (a Home Assistant to-do list).
    ShoppingList(ListCommand),
    /// A Home Assistant scene; the request text, resolved later ("set the bedroom to relax").
    Scene(String),
    /// A Home Assistant script; the request text, resolved later ("run movie time").
    Script(String),
    /// Reverse the last action: "reverse that", "undo", "switch it back".
    Undo,
    /// The TV: "turn off the TV", "put Netflix on the TV".
    Tv(TvCommand),
    /// The rest of the house: status, sun, phones, guest Wi-Fi, camera, alarm.
    House(HouseCommand),
    /// "I'm leaving", "I'm home", "I'm going to bed".
    Routine(Routine),
    /// "Party mode", "fast party mode", "stop the party": the Hue lights follow the music.
    Party(crate::party::PartyCommand),
    Thanks,
    /// "How are you?", "who are you?", "good night".
    SmallTalk(Chat),
    /// "Never mind", "cancel": end the conversation without an answer.
    Cancel,
    Unknown,
}

/// "Good morning" on its own, or asking for the briefing.
/// "What's the news?", "latest headlines", "hva er nyhetene?"; not "good news" in passing.
fn is_news(words: &[String]) -> bool {
    let news = ["news", "headlines", "nyheter", "nyhetene", "nyhetsoppdatering"];
    words.iter().any(|w| news.contains(&w.as_str())) && !words.windows(2).any(|p| p[0] == "good" && p[1] == "news")
}

fn is_briefing(words: &[String]) -> bool {
    let phrase: Vec<&str> = words
        .iter()
        .map(String::as_str)
        .filter(|w| !["and", "hey", "hi", "ok", "og", "hei", "to", "you", "deg"].contains(w))
        .collect();
    matches!(
        phrase.as_slice(),
        ["good", "morning" | "afternoon" | "evening"]
            | ["god", "morgen" | "kveld" | "ettermiddag"]
            | ["morning"]
            | ["morn"]
    ) || words.iter().any(|w| w == "briefing")
        || words.windows(2).any(|p| (p[0] == "brief" && p[1] == "me") || (p[0] == "dagens" && p[1] == "oversikt"))
}

/// "What time is it (now)?", "what's the time", "hva/hvor mye er klokka (nå)?"; the local time
/// only: anything more ("in Tokyo", "does the shop close") is left to the language model.
fn is_time_question(words: &[String]) -> bool {
    // Words around the question that do not change it: "what's the time today", "tell me the time please".
    // An opening the room adds: "hey, what time is it", "so what's the time".
    let filler = [
        "now",
        "please",
        "right",
        "today",
        "currently",
        "exactly",
        "again",
        "here",
        "hey",
        "so",
        "oh",
        "well",
        "nå",
        "da",
        "egentlig",
    ];
    let phrase: Vec<&str> = words.iter().map(String::as_str).filter(|w| !filler.contains(w)).collect();
    matches!(
        phrase.as_slice(),
        ["what", "time", "is", "it" | "this"]
            | ["what's" | "whats", "the", "time"]
            | ["what", "is", "the", "time"]
            // "What's the clock?" (21 Nov) went to Wikipedia.
            | ["what's" | "whats", "the", "clock"]
            | ["what", "is", "the", "clock"]
            | ["what", "does", "the", "clock", "say"]
            | ["tell", "me", "the", "time"]
            | ["do", "you", "know", "what", "time", "it", "is"]
            | ["the", "time"]
            | ["time"]
            | ["hva", "er", "klokka" | "klokken"]
            | ["hvor", "mye", "er", "klokka" | "klokken"]
    )
}

/// "What's the date (today)", "what day is it", "what's today's date", "hvilken dag er det", "hva
/// er datoen i dag".
fn is_date_question(words: &[String]) -> bool {
    let filler =
        ["now", "please", "today", "today's", "todays", "again", "hey", "so", "oh", "well", "idag", "nå", "da"];
    let mut phrase: Vec<&str> = words.iter().map(String::as_str).filter(|w| !filler.contains(w)).collect();
    if phrase.ends_with(&["i", "dag"]) {
        phrase.truncate(phrase.len() - 2);
    }
    matches!(
        phrase.as_slice(),
        ["what's" | "whats", "the", "date"]
            | ["what's" | "whats", "date"]
            | ["what", "is", "the", "date"]
            | ["what", "date", "is", "it"]
            | ["what", "day", "is", "it"]
            | ["which", "day", "is", "it"]
            | ["what's" | "whats", "the", "day"]
            | ["what", "day", "of", "the", "week", "is", "it"]
            | ["what", "month", "is", "it"]
            | ["the", "date"]
            | ["date"]
            | ["tell", "me", "the", "date"]
            | ["hva", "er", "datoen"]
            | ["hvilken", "dag" | "dato", "er", "det"]
            | ["hva", "er", "det", "for", "dag"]
            | ["hvilken", "måned", "er", "det"]
    )
}

/// "joke", "vits" and their forms; checked before weather, so "a joke about the weather" is a joke.
/// "joik" is how NB-Whisper sometimes spells "joke".
const JOKE_STEMS: &[&str] = &["joke", "joik", "vits", "morsom", "funny"];

/// "Which lights are on", "is anything left on", "står det noe lys på", "er lyset på i stua".
/// The word a question opens with once lead-ins are skipped: "so did I leave the lights on" ->
/// "did", "can you tell me if the lights are on" -> "if", "tell me which lights are on" -> "which".
/// A request ("can you turn the lights on") keeps its first word, so callers still see "can".
pub(crate) fn question_word(words: &[String]) -> &str {
    const LEAD: &[&str] = &["so", "oh", "okay", "ok", "hey", "well", "um", "uh", "and", "but", "just", "please", "så"];
    const ASK: &[&str] = &["tell", "check", "see", "know", "fortell", "sjekk", "vet"];
    const ME: &[&str] = &["me", "us", "meg", "oss"];
    const ACTION: &[&str] = &["turn", "switch", "skru", "slå", "start", "stop", "stopp", "dim", "set", "sett"];
    let mut i = words.iter().take_while(|w| LEAD.contains(&w.as_str())).count();
    let at = |i: usize| words.get(i).map(String::as_str).unwrap_or_default();
    // "If you could turn off the lights", "if the TV is on, turn it off", "any chance you could start
    // party mode", "is it possible to dim the lights": requests behind a question's words.
    let request = at(i) == "if"
        || (at(i) == "see" && at(i + 1) == "if")
        || (at(i) == "any" && at(i + 1) == "chance")
        || (["is", "would"].contains(&at(i)) && at(i + 1) == "it" && words[i..].iter().any(|w| w == "possible"));
    if request {
        if let Some(verb) = words[i..].iter().find(|w| ACTION.contains(&w.as_str())) {
            return verb;
        }
    }
    // "Can you tell me", "could you check", "do you know", "kan du sjekke".
    if ["can", "could", "would", "will", "do", "kan", "vil"].contains(&at(i))
        && ["you", "du"].contains(&at(i + 1))
        && ASK.iter().any(|a| at(i + 2).starts_with(a))
    {
        i += 2;
    }
    if ASK.iter().any(|a| at(i).starts_with(a)) {
        // "Tell me if", "vet du om".
        let after = i + 1 + usize::from(ME.contains(&at(i + 1)) || ["you", "du"].contains(&at(i + 1)));
        if ["if", "whether", "which", "what", "om", "hvilke", "hva"].contains(&at(after)) {
            i = after;
        }
    }
    at(i)
}

/// Words of a request that are read: no spoken request is longer, and the rules' time grows with
/// the length of what is typed into the web page.
const MAX_WORDS: usize = 60;
const MAX_WORD_CHARS: usize = 40;

pub fn parse(text: &str) -> Intent {
    parse_with(text, true)
}

/// As the other skills take the words, leaving notes out: for "what did I tell you about the
/// timer" or "forget the milk" when no note is about them.
pub fn parse_without_notes(text: &str) -> Intent {
    parse_with(text, false)
}

fn parse_with(text: &str, notes: bool) -> Intent {
    let mut words = text.split_whitespace();
    let capped: String;
    let text = if text.len() > MAX_WORDS * MAX_WORD_CHARS || words.nth(MAX_WORDS).is_some() {
        let words: Vec<String> =
            text.split_whitespace().take(MAX_WORDS).map(|w| w.chars().take(MAX_WORD_CHARS).collect()).collect();
        capped = words.join(" ");
        capped.as_str()
    } else {
        text
    };
    // A lookup only when no skill takes the words: "what is the weather" stays the weather.
    match rules(text, notes) {
        Intent::Unknown => crate::lookup::topic(text).map_or(Intent::Unknown, Intent::Lookup),
        intent => intent,
    }
}

/// Words the transcriber gets wrong in requests, from the engine's logs, and what was said. Only
/// mishearings that are no request of their own: "stand out" is never asked for.
const MISHEARD: &[(&[&str], &[&str])] = &[
    (&["stand", "out"], &["stand", "down"]),
    (&["shove"], &["show"]),
    (&["tarmer"], &["timer"]),
    (&["tarmers"], &["timers"]),
    (&["best", "times"], &["bus", "times"]),
    (&["bowling"], &["volume"]),
    (&["showed"], &["show"]),
    (&["close", "up"], &["close", "that"]),
    (&["cancel", "time"], &["cancel", "timer"]),
    (&["set", "a", "towel"], &["set", "a", "timer"]),
    (&["code", "rent"], &["code", "red"]),
    (&["code", "ran"], &["code", "red"]),
    (&["who", "am", "i", "on"], &["what's", "on"]),
    (&["increase", "the", "sum"], &["increase", "the", "sound"]),
    (&["increased", "the", "sale"], &["increase", "the", "sound"]),
    (&["temperaturing", "size"], &["temperature", "inside"]),
    (&["temperature", "in", "size"], &["temperature", "inside"]),
];

/// "Lights" as heard at the end of a switch command: "Turn off the kitchen lines." (27 Nov). Only
/// there, as "lines" is a word of its own elsewhere.
const LIGHTS_HEARD: &[&str] = &["lines", "lites", "lytes"];

/// The request with known mishearings put right, as normalised words; unchanged when there are none.
fn corrected(text: &str) -> std::borrow::Cow<'_, str> {
    let mut words: Vec<String> = crate::dialog::words(text);
    let switch = words.first().is_some_and(|w| w == "turn" || w == "switch");
    let lights = words.last_mut().filter(|last| switch && LIGHTS_HEARD.contains(&last.as_str()));
    let fixed = lights.map(|last| *last = "lights".into()).is_some();
    let mut out: Vec<&str> = Vec::with_capacity(words.len());
    let (mut i, mut changed) = (0, fixed);
    while i < words.len() {
        let rest = &words[i..];
        let fix =
            MISHEARD.iter().find(|(heard, _)| rest.len() >= heard.len() && heard.iter().zip(rest).all(|(h, w)| h == w));
        match fix {
            Some((heard, meant)) => {
                out.extend_from_slice(meant);
                i += heard.len();
                changed = true;
            }
            None => {
                out.push(&words[i]);
                i += 1;
            }
        }
    }
    if changed {
        out.join(" ").into()
    } else {
        text.into()
    }
}

fn rules(text: &str, notes: bool) -> Intent {
    let text = &*corrected(text);
    let words: Vec<String> = crate::dialog::words(text);
    // Whole short phrases only, so it takes nothing from the skills ("how are the lights" is not it).
    if let Some(chat) = crate::smalltalk::parse(&words) {
        return Intent::SmallTalk(chat);
    }
    // Before the sums, timers and shopping list: a note holds any words ("remember that the timer
    // is in the drawer", "my locker is number 12").
    if let Some(command) = crate::memory::parse(text).filter(|_| notes) {
        return Intent::Memory(command);
    }
    if let Some((said, _)) = crate::calc::parse(text) {
        return Intent::Calculate(said);
    }
    // Before the weather: "20 degrees Fahrenheit in Celsius" names no place.
    if let Some(conversion) = crate::units::parse(text) {
        return Intent::Convert(conversion);
    }
    if let Some(routine) = house::routine(&words) {
        return Intent::Routine(routine);
    }
    // Before music: "stop the party" is not "stop the music".
    if let Some(command) = crate::party::parse(&words) {
        return Intent::Party(command);
    }
    // Before music: "pause the TV" is not the music, "play Netflix" not a song.
    if let Some(command) = tv::parse(&words) {
        return Intent::Tv(command);
    }
    // Timers before music ("pause the timer"), except for a song called "Timer".
    let play = words.iter().find(|w| !POLITE.contains(&w.as_str())).is_some_and(|w| PLAY_WORDS.contains(&w.as_str()));
    if !play {
        // First: items are any words ("add pause buttons to the list").
        if let Some(command) = shopping::parse(text) {
            return Intent::ShoppingList(command);
        }
        // Before timers: "billigste fire timer med strøm" is four hours of prices, not a timer.
        if let Some(query @ PowerQuery::Window { .. }) = power::parse(text) {
            return Intent::Power(query);
        }
        if let Some(command) = timer::parse(text) {
            return Intent::Timer(command);
        }
        // Before music: "next bus" is not "next song".
        if let Some(query) = transit::parse(text) {
            return Intent::Transit(query);
        }
        // Before the electricity price ("hvor mye strøm har telefonen") and "stop" ("stop recording").
        if let Some(command) = house::parse(&words) {
            return Intent::House(command);
        }
        if let Some(query) = power::parse(text) {
            return Intent::Power(query);
        }
        if is_briefing(&words) {
            return Intent::Briefing;
        }
        if is_news(&words) {
            return Intent::News;
        }
        if let Some(query) = crate::distance::parse(text) {
            return Intent::Distance(query);
        }
    }
    // Then music: song titles contain every other kind of word ("Here Comes the Rain Again").
    if let Some(command) = music(&words) {
        return Intent::Music(command);
    }
    if is_time_question(&words) {
        return Intent::Time;
    }
    if is_date_question(&words) {
        return Intent::Date;
    }
    if words.iter().any(|w| JOKE_STEMS.iter().any(|s| w.starts_with(s))) {
        return Intent::Joke;
    }
    let has = |w: &str| words.iter().any(|x| x == w);
    if is_lights_question(&words) {
        return Intent::LightsStatus { target: text.to_owned() };
    }
    if let Some(name) = whos_home(&words) {
        return Intent::WhosHome(name);
    }
    if let Some(intent) = where_is(&words) {
        return intent;
    }
    if scheduled_switch(&words, text) {
        return Intent::Unknown;
    }
    if let Some(level) = light_level(&words, text) {
        return Intent::LightLevel { level, target: level_target(&without_opening(text)) };
    }
    let first = words.iter().map(String::as_str).find(|w| !POLITE.contains(w)).unwrap_or_default();
    if ["script", "skript", "skriptet"].iter().any(|w| has(w)) || ["run", "kjør", "execute"].contains(&first) {
        return Intent::Script(text.to_owned());
    }
    let scene_word = ["scene", "scenen", "mode", "modus", "stemning", "stemningen"].iter().any(|w| has(w));
    let setting =
        (["set", "sett"].contains(&first) && (has("to") || has("til"))) || ["activate", "aktiver"].contains(&first);
    let not_scene = ["volume", "volumet", "temperature", "temperaturen", "alarm", "timer"].iter().any(|w| has(w));
    if (scene_word || setting) && !not_scene {
        return Intent::Scene(text.to_owned());
    }
    if let Some(mood) = mood(&words) {
        return Intent::Scene(match crate::ha::room_named(&words) {
            Some(room) => format!("{mood} in {room}"),
            None => mood.to_owned(),
        });
    }
    let switching = ["turn", "switch", "skru", "slå"].iter().any(|w| has(w));
    let pronoun = ["them", "it", "those", "these", "they", "that", "dem", "den", "det", "de"].iter().any(|w| has(w));
    // "off"/"av" first: in Norwegian "på" is also the preposition ("av på kjøkkenet").
    let on = if says_off(&words) {
        Some(false)
    } else if has("on") || has("på") {
        Some(true)
    } else if words.windows(2).any(|p| ["turn", "switch"].contains(&p[0].as_str()) && p[1] == "all") {
        // Whisper often hears "turn off" as "turn all".
        Some(false)
    } else {
        None
    };
    // "Turn them off" names no light; the assistant resolves it against the last lights it used.
    // "Turn on the kitchen": a room on its own is its lights.
    if words.iter().any(|w| LIGHT_WORDS.contains(&w.as_str()))
        || (switching && pronoun)
        || (switching && crate::ha::names_only_room(&words))
    {
        if let Some(on) = on {
            return Intent::Lights { on, target: without_opening(text) };
        }
    }
    if ["reverse", "undo", "revert", "angre"].iter().any(|w| has(w)) || (has("back") && switching) {
        return Intent::Undo;
    }
    let weather = |w: &String| WEATHER_WORDS.contains(&w.as_str()) || WEATHER_STEMS.iter().any(|s| w.contains(s));
    if !words.iter().any(weather) && !wants_clothes(&words) {
        // Short social replies only; longer sentences that merely contain "thanks" are unknown.
        if words.len() <= 5 {
            if ["thanks", "thank", "takk", "cheers", "tusen"].iter().any(|w| has(w)) {
                return Intent::Thanks;
            }
            let never_mind = words.windows(2).any(|p| p[0] == "never" && p[1] == "mind");
            if never_mind || ["nevermind", "cancel", "stop", "forget", "avbryt", "glem"].iter().any(|w| has(w)) {
                return Intent::Cancel;
            }
            // After a wake word the TV set off: whole replies only, so "no lights in here" is not one.
            if NOT_FOR_YOU.contains(&words.join(" ").as_str()) {
                return Intent::Cancel;
            }
        }
        return Intent::Unknown;
    }
    // "Next week" has no forecast day of its own: not today's weather, so the language model or a
    // "Sorry" takes it.
    let next_week = words
        .windows(2)
        .any(|p| ["next", "neste"].contains(&p[0].as_str()) && ["week", "uke", "uka"].contains(&p[1].as_str()));
    if next_week {
        return Intent::Unknown;
    }
    Intent::Weather { place: place_of(&words), day: day_of(&words), focus: focus_of(&words) }
}

/// Words a second request in one sentence starts with ("... and play Careless Whisper"). Anything
/// else after "and" belongs to the first request ("add milk and eggs", "the kitchen and hall lights").
const COMMAND_STARTS: &[&str] = &[
    "turn", "switch", "play", "pause", "resume", "stop", "skip", "set", "start", "dim", "make", "add", "remind",
    "tell", "what", "what's", "whats", "when", "when's", "how", "how's", "is", "are", "who", "who's", "cancel", "run",
    "activate", "put", "lower", "raise", "skru", "slå", "spill", "sett", "legg", "hva", "når", "hvordan", "fortell",
];

/// Two or more requests in one sentence: "turn on the living room lights and play Careless Whisper",
/// "what's the weather, then when's the next bus". Split only where the next part starts with a
/// command word and every part is a request on its own; otherwise `None`.
pub fn split_commands(text: &str) -> Option<Vec<String>> {
    let mut parts: Vec<Vec<&str>> = vec![Vec::new()];
    let words: Vec<&str> = text.split_whitespace().collect();
    let mut i = 0;
    while i < words.len() {
        let word = normalize(words[i]);
        // "and", "and then", "then", "og", "og så": a new part when a command word follows.
        let joiner = match word.as_str() {
            "and" | "og" if words.get(i + 1).is_some_and(|w| ["then", "så"].contains(&normalize(w).as_str())) => 2,
            "and" | "then" | "og" => 1,
            _ => 0,
        };
        let next = words.get(i + joiner).map(|w| normalize(w)).unwrap_or_default();
        if joiner > 0 && !parts.last().is_some_and(Vec::is_empty) && COMMAND_STARTS.contains(&next.as_str()) {
            parts.push(Vec::new());
            i += joiner;
            continue;
        }
        parts.last_mut().unwrap().push(words[i]);
        i += 1;
    }
    if parts.len() < 2 {
        return None;
    }
    let parts: Vec<String> = parts.iter().map(|p| p.join(" ").trim_end_matches([',', ';']).trim().to_owned()).collect();
    let requests = parts.iter().all(|p| !matches!(parse(p), Intent::Unknown | Intent::Thanks | Intent::Cancel));
    requests.then_some(parts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use weather::tests::weather;

    /// Questions must not act: "did I leave the lights on" once switched them on.
    #[test]
    fn questions_do_not_act() {
        let status = |t: &str| Intent::LightsStatus { target: t.to_owned() };
        assert_eq!(parse("Did I leave the lights on"), status("Did I leave the lights on"));
        assert_eq!(parse("Did I leave any lights on?"), status("Did I leave any lights on?"));
        assert_eq!(parse("Have I left a light on"), status("Have I left a light on"));
        assert_eq!(parse("Did I leave the TV on?"), Intent::Tv(TvCommand::Status));
        assert_eq!(parse("Is the TV on?"), Intent::Tv(TvCommand::Status));
        assert_eq!(parse("Er TV-en på?"), Intent::Tv(TvCommand::Status));
        assert_eq!(parse("Is party mode on?"), Intent::Party(crate::party::PartyCommand::Status));
        assert_eq!(
            parse("Turn on party mode"),
            Intent::Party(crate::party::PartyCommand::On(crate::party::Pace::Normal))
        );
        assert_eq!(parse("Turn on the TV"), Intent::Tv(TvCommand::Power(true)));
        for phrase in [
            "Can you tell me if the lights are on",
            "Tell me which lights are on",
            "Check if the lights are on",
            "So did I leave the lights on?",
            "Oh, did I leave the lights on?",
            "Are the lights off?",
            "Did I turn off the kitchen lights?",
            "Er lyset av?",
        ] {
            assert_eq!(parse(phrase), status(phrase), "{phrase}");
        }
        let party_status = Intent::Party(crate::party::PartyCommand::Status);
        assert_eq!(parse("Okay, is party mode on?"), party_status);
        assert_eq!(parse("Tell me if party mode is on"), party_status);
        assert_eq!(parse("Can you check if the TV is on"), Intent::Tv(TvCommand::Status));
        // Requests phrased politely still act.
        assert_eq!(
            parse("Can you turn the lights on"),
            Intent::Lights { on: true, target: "Can you turn the lights on".into() }
        );
        assert_eq!(parse("Turn the lights off"), Intent::Lights { on: false, target: "Turn the lights off".into() });
        assert_eq!(
            parse("Could you start party mode"),
            Intent::Party(crate::party::PartyCommand::On(crate::party::Pace::Normal))
        );
        assert_eq!(parse("Can you turn on the TV"), Intent::Tv(TvCommand::Power(true)));
        assert_eq!(parse("Vet du om lyset er på"), status("Vet du om lyset er på"));
        // Requests behind "if" and the like act.
        assert_eq!(
            parse("If you could turn off the lights"),
            Intent::Lights { on: false, target: "If you could turn off the lights".into() }
        );
        assert_eq!(
            parse("Is it possible to turn off the lights"),
            Intent::Lights { on: false, target: "Is it possible to turn off the lights".into() }
        );
        assert_eq!(parse("If the TV is on, turn it off"), Intent::Tv(TvCommand::Power(false)));
        assert_eq!(parse("If party mode is on turn it off"), Intent::Party(crate::party::PartyCommand::Off));
        // The music's, not the party's: "pause" and "kill" stop the party only without a music word.
        assert!(matches!(parse("Pause the party music"), Intent::Music(_)));
    }

    #[test]
    fn dates() {
        for phrase in [
            "What's the date?",
            "What's the date today?",
            "What's today's date?",
            "What day is it?",
            "What day is it today?",
            "What day of the week is it?",
            "Hvilken dag er det i dag?",
            "Hva er datoen i dag?",
        ] {
            assert_eq!(parse(phrase), Intent::Date, "{phrase}");
        }
        assert_eq!(parse("What time is it?"), Intent::Time);
    }

    #[test]
    fn a_room_on_its_own_after_oh() {
        assert!(matches!(parse("Oh, turn on the kitchen."), Intent::Lights { on: true, .. }));
    }

    /// Requests the living room Voice PE heard wrong, from the engine's logs (12-17 October 2026).
    #[test]
    fn logged_mishearings() {
        use crate::tv::Screen;
        assert_eq!(parse("Stand out."), Intent::Routine(Routine::StandDown));
        // 27 November 2026: "lights" as "lines", only at the end of a switch command.
        let lights = Intent::Lights { on: false, target: "turn off the kitchen lights".into() };
        assert_eq!(parse("Turn off the kitchen lines."), lights);
        assert!(!matches!(parse("Read me the lines."), Intent::Lights { .. }));
        assert_eq!(parse("Initially lockdown."), Intent::Routine(Routine::Lockdown));
        assert_eq!(parse("In the shape, lock down."), Intent::Routine(Routine::Lockdown));
        assert_eq!(parse("Is the lockdown still on?"), Intent::Unknown);
        assert_eq!(parse("Shove timers on the screen."), Intent::Tv(TvCommand::Show(Screen::Timers)));
        assert_eq!(parse("Show Tarmers on the screen."), Intent::Tv(TvCommand::Show(Screen::Timers)));
        assert_eq!(
            parse("Show the best times on the screen to release up it."),
            Intent::Tv(TvCommand::Show(Screen::Departures))
        );
        assert_eq!(parse("The bowling at 60%."), Intent::Music(MusicCommand::VolumeTo(60)));
        assert!(matches!(parse("Set a towel for 10 minutes."), Intent::Timer(_)));
        assert!(matches!(parse("Hello, guys."), Intent::SmallTalk(_)));
        assert_eq!(parse("I always do weather today."), weather(None, Day::Today));
        assert_eq!(parse("Show the dashboard on the TV."), Intent::Tv(TvCommand::Show(Screen::Jarvis)));
        assert_eq!(parse("Show the dashboard."), Intent::Tv(TvCommand::Show(Screen::Jarvis)));
        assert!(matches!(parse("Turn on the kitchen."), Intent::Lights { on: true, .. }));
        assert!(matches!(parse("Switch off the living room."), Intent::Lights { on: false, .. }));
        assert_eq!(parse("Turn on the bedroom fan."), Intent::Unknown);
        assert_eq!(parse("Showed a dashboard on the TV."), Intent::Tv(TvCommand::Show(Screen::Jarvis)));
        assert_eq!(parse("Close up."), Intent::Tv(TvCommand::Hide));
        assert!(matches!(parse("Cancel time."), Intent::Timer(TimerCommand::Cancel(_))));
        // 17-18 October.
        for code in ["Code red.", "Code rent.", "Initiate code ran."] {
            assert_eq!(parse(code), Intent::Routine(Routine::Lockdown), "{code}");
        }
        assert_eq!(parse("How you doing?"), Intent::SmallTalk(Chat::HowAreYou));
        assert_eq!(parse("What's the house status?"), Intent::House(HouseCommand::Status));
        assert_eq!(parse("Who am I on the shopping list?"), Intent::ShoppingList(ListCommand::Read));
        assert!(matches!(parse("Increase the sum."), Intent::Music(_)));
        assert_eq!(parse("I increased the sale to 40 percent."), Intent::Music(MusicCommand::VolumeTo(40)));
    }

    #[test]
    fn follow_ups_refer_back() {
        assert!(matches!(parse("Turn off them."), Intent::Lights { on: false, .. }));
        assert!(matches!(parse("turn it back on"), Intent::Lights { on: true, .. }));
        assert!(matches!(parse("switch them on again"), Intent::Lights { on: true, .. }));
        assert_eq!(parse("Reverse that."), Intent::Undo);
        assert_eq!(parse("undo"), Intent::Undo);
        assert_eq!(parse("switch it back"), Intent::Undo);
        assert_eq!(parse("Thank you."), Intent::Thanks);
        assert_eq!(parse("Good evening."), Intent::Briefing);
        assert_eq!(parse("God kveld"), Intent::Briefing);
        assert_eq!(parse("What's the latest news?"), Intent::News);
        assert_eq!(parse("Who is Jonas Gahr Støre?"), Intent::Lookup("Jonas Gahr Støre".into()));
        assert_eq!(parse("What is the weather tomorrow?"), weather(None, Day::Tomorrow));
        assert_eq!(parse("what's the time"), Intent::Time);
        assert_eq!(parse("What's the time today?"), Intent::Time);
        assert_eq!(parse("tell me the time please"), Intent::Time);
        assert_eq!(parse("What's 5 plus 5?"), Intent::Calculate("5 plus 5".into()));
        assert!(matches!(parse("How long is it from Oslo to Shanghai?"), Intent::Distance(_)));
        assert_eq!(parse("Can you look up Edvard Munch?"), Intent::Lookup("Edvard Munch".into()));
        assert_eq!(parse("read me the headlines"), Intent::News);
        assert_eq!(parse("Hva er nyhetene?"), Intent::News);
        assert_eq!(parse("thanks a lot"), Intent::Thanks);
        assert_eq!(parse("Never mind."), Intent::Cancel);
        assert_eq!(parse("No."), Intent::Cancel);
        assert_eq!(parse("Not you."), Intent::Cancel);
        assert_eq!(parse("go back to sleep"), Intent::Cancel);
        assert_eq!(parse("ikke deg"), Intent::Cancel);
        assert_ne!(parse("no lights on in the kitchen"), Intent::Cancel);
        assert_eq!(parse("cancel"), Intent::Cancel);
        assert_eq!(parse("I wanted to thank my mother for the lovely dinner yesterday"), Intent::Unknown);
    }

    #[test]
    fn joke_requests() {
        for text in [
            "tell me a joke",
            "Do you know any good jokes?",
            "Say something funny",
            "Fortell en vits",
            "kan du fortelle meg en vits om været?",
            "si noe morsomt",
            "Tell me a joik.",
        ] {
            assert_eq!(parse(text), Intent::Joke, "{text}");
        }
    }

    #[test]
    fn a_long_request_parses_quickly() {
        // Rooms and queue words over and over once took exponential time.
        let long = format!("play hello {}", "on the kitchen speaker after this song ".repeat(30));
        let words = "play hello in the kitchen next ".repeat(40);
        for text in [long.as_str(), words.as_str()] {
            let start = std::time::Instant::now();
            parse(text);
            assert!(start.elapsed() < std::time::Duration::from_millis(50), "{:?}", start.elapsed());
        }
    }

    #[test]
    fn time_questions() {
        assert_eq!(parse("What time is it?"), Intent::Time);
        assert_eq!(parse("Hey, what time is it?"), Intent::Time);
        assert_eq!(parse("What's the clock?"), Intent::Time);
        assert_eq!(parse("What does the clock say?"), Intent::Time);
        assert_ne!(parse("What is the clock made of?"), Intent::Time);
        assert_eq!(parse("what is the clock"), Intent::Time);
        assert!(matches!(parse("what is a clock"), Intent::Lookup(_)), "{:?}", parse("what is a clock"));
        assert_eq!(parse("What's the temperaturing size?"), parse("What's the temperature inside?"));
        assert_eq!(parse("What time is this?"), Intent::Time);
        assert_eq!(parse("Oh, I'm home."), Intent::Routine(house::Routine::Home));
        assert!(matches!(parse("Why is your name Darby?"), Intent::SmallTalk(_)));
        assert_eq!(parse("hva er klokka?"), Intent::Time);
        assert_eq!(parse("hvor mye er klokken"), Intent::Time);
        assert_eq!(parse("What time is it now?"), Intent::Time);
        assert_eq!(parse("what time does the shop close"), Intent::Unknown);
        assert_eq!(parse("what time is it in Tokyo"), Intent::Unknown);
    }

    #[test]
    fn timers_come_before_music_and_cancel() {
        let timer = |text: &str| matches!(parse(text), Intent::Timer(_));
        assert!(timer("set a timer for ten minutes"));
        assert!(timer("sett en timer på ti minutter"));
        assert!(timer("pause the timer"));
        assert!(timer("stop the timer"));
        assert!(!timer("play Timer by Sabrina Carpenter"));
        assert_eq!(parse("stop"), Intent::Cancel);
        assert_eq!(parse("Shut up!"), Intent::Cancel);
        assert_eq!(parse("Hold kjeft."), Intent::Cancel);
        assert_eq!(parse("pause"), Intent::Music(MusicCommand::Pause));
        assert!(matches!(parse("next bus"), Intent::Transit(_)));
        assert_eq!(parse("next song"), Intent::Music(MusicCommand::Next));
        assert_eq!(parse("neste sang"), Intent::Music(MusicCommand::Next));
        assert_eq!(parse("neste"), Intent::Music(MusicCommand::Next));
        // Of a day or a week, "next" is not the next song, even short.
        for text in ["og neste helg", "neste uke da", "and next weekend", "next week", "neste kamp"] {
            assert_ne!(parse(text), Intent::Music(MusicCommand::Next), "{text}");
        }
        // A week has no weather day of its own: not today's weather either.
        for text in ["hva blir været neste uke", "what's the weather next week"] {
            assert!(!matches!(parse(text), Intent::Weather { day: Day::Today, .. }), "{text}");
        }
        // A skip verb with "neste" is the next song.
        for text in ["kan du hoppe til neste", "kan du ta neste", "hopp til neste"] {
            assert_eq!(parse(text), Intent::Music(MusicCommand::Next), "{text}");
        }
    }

    #[test]
    fn home_requests() {
        assert!(matches!(parse("which lights are on?"), Intent::LightsStatus { .. }));
        assert!(matches!(parse("Is anything left on?"), Intent::LightsStatus { .. }));
        assert!(matches!(parse("are the kitchen lights on"), Intent::LightsStatus { .. }));
        assert!(matches!(parse("står det noe lys på?"), Intent::LightsStatus { .. }));
        assert!(matches!(parse("turn on the kitchen lights"), Intent::Lights { on: true, .. }));
        assert!(matches!(parse("set the living room to relax"), Intent::Scene(_)));
        assert!(matches!(parse("activate the reading scene"), Intent::Scene(_)));
        assert!(matches!(parse("relax mode in the bedroom"), Intent::Scene(_)));
        assert!(matches!(parse("sett soverommet til relax"), Intent::Scene(_)));
        // Heard on the device.
        assert_eq!(parse("Put on relaxing lights."), Intent::Scene("relax".into()));
        assert_eq!(parse("Wanted to put on relaxing lights."), Intent::Scene("relax".into()));
        assert_eq!(parse("Want to relax?"), Intent::Scene("relax".into()));
        assert_eq!(parse("I want to relax"), Intent::Scene("relax".into()));
        assert_eq!(parse("relaxing lights in the bedroom"), Intent::Scene("relax in bedroom".into()));
        assert_eq!(parse("jeg vil slappe av"), Intent::Scene("relax".into()));
        assert!(matches!(parse("play relaxing music"), Intent::Music(_)));
        // A relaxed party is the lights following the music, not the Relax scene.
        let relaxed = Intent::Party(crate::party::PartyCommand::On(crate::party::Pace::Relaxed));
        assert_eq!(parse("Relaxed party mode."), relaxed);
        assert_eq!(parse("Turn on relaxing party mode"), relaxed);
        assert!(matches!(parse("turn on the reading lamp"), Intent::Lights { on: true, .. }));
        assert!(matches!(parse("run movie time"), Intent::Script(_)));
        assert!(matches!(parse("kjør skriptet movie time"), Intent::Script(_)));
        assert!(matches!(parse("set a timer for ten minutes"), Intent::Timer(_)));
        assert!(matches!(parse("set the volume to 20"), Intent::Music(MusicCommand::VolumeTo(20))));
        assert!(matches!(parse("add milk to the shopping list"), Intent::ShoppingList(_)));
        assert!(matches!(parse("what's on the shopping list"), Intent::ShoppingList(ListCommand::Read)));
        assert!(matches!(parse("play my shopping playlist"), Intent::Music(_)));
    }

    #[test]
    fn people_power_and_briefing() {
        assert_eq!(parse("who's home?"), Intent::WhosHome(None));
        assert_eq!(parse("is anyone home"), Intent::WhosHome(None));
        assert_eq!(parse("is Ingrid home?"), Intent::WhosHome(Some("ingrid".into())));
        assert_eq!(parse("er Ingrid hjemme?"), Intent::WhosHome(Some("ingrid".into())));
        assert_eq!(parse("hvem er hjemme"), Intent::WhosHome(None));
        assert!(!matches!(parse("is it cold at home"), Intent::WhosHome(_)));
    }

    #[test]
    fn unit_conversions() {
        for said in [
            "convert 10 miles to kilometers",
            "5 miles in kilometers",
            "how many grams in a pound",
            "what's 20 degrees fahrenheit in celsius",
            "hvor mange centimeter er en tomme",
            "how many tablespoons in a cup",
            "how many ounces in a litre",
            "convert 6 feet to metres",
            "what's 2 litres in cups",
            "70 kg in pounds",
            "12 inches in centimetres",
        ] {
            assert!(matches!(parse(said), Intent::Convert(_)), "{said} -> {:?}", parse(said));
        }
    }

    /// Phrases a review found the notes taking from other skills: each parses as it did before
    /// the notes (main at 57dc051).
    #[test]
    fn notes_take_nothing_from_other_skills() {
        let before: &[(&str, &str)] = &[
            (r#"remember to take the bins out"#, r#"Unknown"#),
            (r#"remember to buy milk"#, r#"Unknown"#),
            (r#"remember to add milk to the shopping list"#, r#"Unknown"#),
            (
                r#"remind me to call mum at 5"#,
                r#"Timer(Remind { when: Some(At(Clock { hour: 5, minute: 0, pm: None, tomorrow: false })), message: Some("call mum") })"#,
            ),
            (r#"remember me"#, r#"Unknown"#),
            (r#"remember the milk"#, r#"Unknown"#),
            (r#"remember the time we went to Bergen"#, r#"Unknown"#),
            (r#"remember when we went to Bergen"#, r#"Unknown"#),
            (r#"remember that song"#, r#"Unknown"#),
            (r#"husk å kjøpe melk"#, r#"Unknown"#),
            (r#"husk på å ringe mamma"#, r#"Unknown"#),
            (r#"husk melk"#, r#"Unknown"#),
            (r#"forget it, turn off the lights"#, r#"Lights { on: false, target: "forget it, turn off the lights" }"#),
            (r#"forget it"#, r#"Cancel"#),
            (r#"forget about it"#, r#"Cancel"#),
            (r#"forget that"#, r#"Cancel"#),
            (
                r#"forget the timer"#,
                r#"Timer(Remaining(Which { name: None, seconds: None, all: false, reminder: false, alarm: false }))"#,
            ),
            (
                r#"forget the pasta timer"#,
                r#"Timer(Remaining(Which { name: Some("pasta"), seconds: None, all: false, reminder: false, alarm: false }))"#,
            ),
            (
                r#"forget my alarm"#,
                r#"Timer(Remaining(Which { name: None, seconds: None, all: false, reminder: false, alarm: true }))"#,
            ),
            (
                r#"forget the reminder"#,
                r#"Timer(Remaining(Which { name: None, seconds: None, all: false, reminder: true, alarm: false }))"#,
            ),
            (r#"forget milk from the shopping list"#, r#"Unknown"#),
            (r#"forget the shopping list"#, r#"Cancel"#),
            (
                r#"delete the timer"#,
                r#"Timer(Cancel(Which { name: None, seconds: None, all: false, reminder: false, alarm: false }))"#,
            ),
            (r#"delete milk from the shopping list"#, r#"ShoppingList(Remove(["Milk"]))"#),
            (
                r#"delete my alarm"#,
                r#"Timer(Cancel(Which { name: None, seconds: None, all: false, reminder: false, alarm: true }))"#,
            ),
            (
                r#"slett timeren"#,
                r#"Timer(Cancel(Which { name: None, seconds: None, all: false, reminder: false, alarm: false }))"#,
            ),
            (r#"slett melk fra handlelisten"#, r#"ShoppingList(Remove(["Melk"]))"#),
            (
                r#"glem timeren"#,
                r#"Timer(Remaining(Which { name: Some("glem"), seconds: None, all: false, reminder: false, alarm: false }))"#,
            ),
            (r#"glem det, skru av lyset"#, r#"Lights { on: false, target: "glem det, skru av lyset" }"#),
            (r#"glem handlelisten"#, r#"Cancel"#),
            (r#"do you know what time it is"#, r#"Time"#),
            (
                r#"do you know what the weather is tomorrow"#,
                r#"Weather { place: None, day: Tomorrow, focus: General }"#,
            ),
            (r#"do you know where Ingrid is"#, r#"Unknown"#),
            (r#"do you remember what time it is"#, r#"Unknown"#),
            (r#"do you know what's on the shopping list"#, r#"ShoppingList(Read)"#),
            (r#"vet du hva klokka er"#, r#"Unknown"#),
            (r#"vet du hvor Ingrid er"#, r#"Unknown"#),
            (r#"husker du hva klokka er"#, r#"Unknown"#),
            (r#"what do you know about Edvard Munch"#, r#"Lookup("Edvard Munch")"#),
            (r#"what do you know about Bergen"#, r#"Lookup("Bergen")"#),
            (r#"hva vet du om Edvard Munch"#, r#"Unknown"#),
            (r#"what did you say"#, r#"Unknown"#),
            (r#"what did you say about the weather"#, r#"Weather { place: None, day: Today, focus: General }"#),
            (
                r#"what did you say about the timer"#,
                r#"Timer(Remaining(Which { name: None, seconds: None, all: false, reminder: false, alarm: false }))"#,
            ),
            (r#"what's the weather tomorrow"#, r#"Weather { place: None, day: Tomorrow, focus: General }"#),
            (r#"what time is it"#, r#"Time"#),
            (r#"set a timer for 10 minutes"#, r#"Timer(Start { seconds: Some(600), name: None })"#),
            (r#"add milk to the shopping list"#, r#"ShoppingList(Add(["Milk"]))"#),
            (r#"where is Ingrid"#, r#"WhereIs { name: "ingrid", zone: None, arriving: false }"#),
            (r#"where's my phone"#, r#"House(FindPhone(None))"#),
            (r#"where is my phone"#, r#"House(FindPhone(None))"#),
            (r#"where is Bergen"#, r#"WhereIs { name: "bergen", zone: None, arriving: false }"#),
            (r#"who is Edvard Munch"#, r#"Lookup("Edvard Munch")"#),
            (r#"play Remember the Time"#, r#"Music(Song("remember the time"))"#),
            (r#"note that down"#, r#"Unknown"#),
            (r#"make a note"#, r#"Unknown"#),
            (
                r#"cancel the timer"#,
                r#"Timer(Cancel(Which { name: None, seconds: None, all: false, reminder: false, alarm: false }))"#,
            ),
            (
                r#"remind me in 10 minutes to check the oven"#,
                r#"Timer(Remind { when: Some(In(600)), message: Some("check the oven") })"#,
            ),
            (r#"don't let me forget the laundry in half an hour"#, r#"Unknown"#),
            (r#"what's on the shopping list"#, r#"ShoppingList(Read)"#),
            (r#"where did I put my phone"#, r#"House(FindPhone(None))"#),
            (r#"where did I leave my phone"#, r#"House(FindPhone(None))"#),
            (r#"where did I put my phone?"#, r#"House(FindPhone(None))"#),
            (r#"hvor la jeg telefonen"#, r#"House(FindPhone(None))"#),
            (r#"hvor la jeg mobilen min"#, r#"House(FindPhone(None))"#),
            (r#"hvor har jeg lagt telefonen"#, r#"House(FindPhone(None))"#),
            (r#"where is Ingrid's bike"#, r#"WhereIs { name: "ingrid bike", zone: None, arriving: false }"#),
            (r#"where is Ola's jacket"#, r#"WhereIs { name: "ola jacket", zone: None, arriving: false }"#),
        ];
        let changed: Vec<String> = before
            .iter()
            .filter(|(said, was)| format!("{:?}", parse(said)) != *was)
            .map(|(said, was)| format!("{said}: {:?}, was {was}", parse(said)))
            .collect();
        assert!(changed.is_empty(), "{changed:#?}");
    }

    /// When no note is about it, the engine asks the other skills, which take it as on main.
    #[test]
    fn without_notes_the_other_skills_take_it() {
        for (said, was) in [
            (
                "what did I tell you about the timer",
                "Timer(Remaining(Which { name: None, seconds: None, all: false, reminder: false, alarm: false }))",
            ),
            ("forget the milk", "Cancel"),
            ("glem melka", "Cancel"),
            ("forget the lights", "Cancel"),
        ] {
            assert!(matches!(parse(said), Intent::Memory(_)), "{said}");
            assert_eq!(format!("{:?}", parse_without_notes(said)), was, "{said}");
        }
        // A note, though main set an alarm: the words say to keep it.
        assert!(matches!(parse("remember the alarm is at 7"), Intent::Memory(_)));
    }

    #[test]
    fn notes_take_their_words_and_leave_the_rest() {
        use crate::memory::MemoryCommand;
        let note = |s: &str| Intent::Memory(MemoryCommand::Remember(s.into()));
        // Words other skills would take: a timer, a sum, the shopping list.
        assert_eq!(parse("remember that the timer is in the top drawer"), note("the timer is in the top drawer"));
        assert_eq!(parse("remember that my locker is number 12"), note("my locker is number 12"));
        assert_eq!(parse("remember that we need milk on Friday"), note("we need milk on Friday"));
        assert_eq!(parse("forget the spare key"), Intent::Memory(MemoryCommand::Forget("the spare key".into())));
        // Still "never mind", reminders and people.
        assert_eq!(parse("forget it"), Intent::Cancel);
        assert_eq!(parse("glem det"), Intent::Cancel);
        assert!(matches!(parse("remind me in 10 minutes to check the oven"), Intent::Timer(_)));
        assert!(matches!(parse("don't let me forget the laundry in half an hour"), Intent::Timer(_) | Intent::Unknown));
        assert!(matches!(parse("where is Ingrid"), Intent::WhereIs { .. }));
        assert!(matches!(parse("play Remember the Time"), Intent::Music(_)));
    }

    #[test]
    fn sensors_humidity_and_the_dark() {
        use crate::house::{Indoors, Reading, SunEvent};
        use crate::sensors::SensorQuery;
        let sensors = |q| Intent::House(HouseCommand::Sensors(q));
        assert_eq!(parse("which batteries are low"), sensors(SensorQuery::Battery(None)));
        assert_eq!(parse("what's the battery on the leak sensor"), sensors(SensorQuery::Battery(Some("leak".into()))));
        assert_eq!(parse("hvilke batterier er lave"), sensors(SensorQuery::Battery(None)));
        assert_eq!(parse("is there a leak"), sensors(SensorQuery::Leaks));
        assert_eq!(parse("any leaks?"), sensors(SensorQuery::Leaks));
        assert_eq!(parse("er det noen lekkasje"), sensors(SensorQuery::Leaks));
        assert_eq!(
            parse("what's the humidity"),
            Intent::House(HouseCommand::Climate(Indoors::Here, Reading::Humidity))
        );
        assert_eq!(parse("is it dark outside"), Intent::House(HouseCommand::Sun(SunEvent::IsDark(true))));
        assert_eq!(parse("when does it get light"), Intent::House(HouseCommand::Sun(SunEvent::Sunrise)));
        assert!(matches!(parse("when is electricity cheapest tonight"), Intent::Power(_)));
        // Appliances and stretches of hours reach the prices, not the house or a timer.
        for question in [
            "best time to run the washing machine tonight",
            "when is it cheapest to charge the car",
            "billigste fire timer med strøm",
            "when should I start the dryer",
            "when should I charge the car",
            "når bør jeg lade bilen",
        ] {
            assert!(
                matches!(parse(question), Intent::Power(crate::power::PowerQuery::Window { .. })),
                "{question}: {:?}",
                parse(question)
            );
        }
        // Reminders stay reminders.
        assert!(matches!(parse("remind me to run the dishwasher in 3 hours"), Intent::Timer(_)));
        for question in ["når går neste 37-buss", "next train from Oslo S", "when does bus 20 leave"] {
            assert!(matches!(parse(question), Intent::Transit(_)), "{question}: {:?}", parse(question));
        }
        assert_eq!(parse("Good morning!"), Intent::Briefing);
        assert_eq!(parse("god morgen"), Intent::Briefing);
        assert_eq!(parse("give me my morning briefing"), Intent::Briefing);
        assert!(!matches!(parse("good morning, what's the weather in Bergen"), Intent::Briefing));
    }

    #[test]
    fn house_tv_and_routines() {
        use crate::house::{SunEvent, Switch};
        assert_eq!(parse("Is everything okay at home?"), Intent::House(HouseCommand::Status));
        assert_eq!(parse("when is sunset"), Intent::House(HouseCommand::Sun(SunEvent::Sunset)));
        assert_eq!(parse("where's my phone"), Intent::House(HouseCommand::FindPhone(None)));
        assert_eq!(parse("hvor mye strøm har telefonen min"), Intent::House(HouseCommand::PhoneBattery(None)));
        assert_eq!(parse("stop recording"), Intent::House(HouseCommand::Camera(Switch::Off)));
        assert_eq!(parse("turn off the guest wifi"), Intent::House(HouseCommand::GuestWifi(Switch::Off)));
        assert_eq!(parse("pause the TV"), Intent::Tv(TvCommand::Pause));
        assert_eq!(parse("play Netflix on the TV"), Intent::Tv(TvCommand::App("netflix".into())));
        assert_eq!(parse("turn on the TV and dim the lights"), Intent::Tv(TvCommand::Power(true)));
        assert_eq!(parse("I'm leaving"), Intent::Routine(Routine::Leaving));
        assert_eq!(parse("I'm going to bed"), Intent::Routine(Routine::Bedtime));
        assert_eq!(parse("good night"), Intent::Routine(Routine::Bedtime));
        assert_eq!(parse("Goodnight, Jarvis."), Intent::Routine(Routine::Bedtime));
        assert_eq!(parse("Daddy's home!"), Intent::Routine(Routine::Home));
        assert_eq!(parse("pause"), Intent::Music(MusicCommand::Pause));
        assert_eq!(parse("stop"), Intent::Cancel);
        assert!(matches!(parse("turn off the alarm"), Intent::Timer(_) | Intent::Unknown));
        assert_eq!(split_commands("turn on the TV and dim the lights").unwrap().len(), 2);
    }

    #[test]
    fn other_requests_are_unknown() {
        assert_eq!(parse("what's the capital of France"), Intent::Unknown);
        assert_eq!(parse(""), Intent::Unknown);
    }

    #[test]
    fn splits_several_requests() {
        let split = |t: &str| split_commands(t);
        assert_eq!(
            split("Turn on the light in the living room and play Careless Whisper").unwrap(),
            ["Turn on the light in the living room", "play Careless Whisper"]
        );
        assert_eq!(
            split("what's the weather, and then when's the next bus?").unwrap(),
            ["what's the weather", "when's the next bus?"]
        );
        assert_eq!(split("turn off the kitchen lights then set a timer for 10 minutes").unwrap().len(), 2);
        assert_eq!(split("skru på lyset i stua og spill Beatles").unwrap().len(), 2);
    }

    #[test]
    fn keeps_one_request_whole() {
        assert_eq!(split_commands("add milk and eggs to the shopping list"), None);
        assert_eq!(split_commands("turn on the kitchen and hallway lights"), None);
        assert_eq!(split_commands("play Simon and Garfunkel"), None);
        assert_eq!(split_commands("play rock and roll"), None);
        assert_eq!(split_commands("what's the weather"), None);
        // The second part is no request on its own.
        assert_eq!(split_commands("turn on the lights and what"), None);
    }
}
