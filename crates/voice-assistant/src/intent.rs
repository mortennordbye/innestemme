//! Rule-based intent parsing, English and Norwegian. A placeholder for the LLM step: enough to
//! route a weather question and pull out the place and day.

use crate::dialog::normalize;
use crate::power::{self, PowerQuery};
use crate::shopping::{self, ListCommand};
use crate::timer::{self, TimerCommand};
use crate::transit::{self, TransitQuery};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Day {
    Today,
    Tomorrow,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Intent {
    /// `place` is `None` when the request names no place.
    Weather {
        place: Option<String>,
        day: Day,
    },
    Joke,
    /// "What time is it?", "hva er klokka?"
    Time,
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
    /// "Good morning": the time, the weather at home and the shopping list in a few sentences.
    Briefing,
    /// The shopping list (a Home Assistant to-do list).
    ShoppingList(ListCommand),
    /// A Home Assistant scene; the request text, resolved later ("set the bedroom to relax").
    Scene(String),
    /// A Home Assistant script; the request text, resolved later ("run movie time").
    Script(String),
    /// Reverse the last action: "reverse that", "undo", "switch it back".
    Undo,
    Thanks,
    /// "Never mind", "cancel": end the conversation without an answer.
    Cancel,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LightLevel {
    /// Brightness in percent; 0 switches off.
    Percent(u8),
    Brighter,
    Dimmer,
    /// A CSS colour name, as Home Assistant's `color_name` takes it.
    Color(String),
    Warm,
    Cool,
}

/// Spoken colours and the CSS names Home Assistant knows.
const COLOURS: &[(&str, &[&str])] = &[
    ("red", &["red", "rød", "rødt", "røde"]),
    ("blue", &["blue", "blå", "blått", "blåe"]),
    ("green", &["green", "grønn", "grønt", "grønne"]),
    ("yellow", &["yellow", "gul", "gult", "gule"]),
    ("orange", &["orange", "oransje"]),
    ("purple", &["purple", "lilla"]),
    ("pink", &["pink", "rosa"]),
    ("turquoise", &["turquoise", "turkis"]),
    ("white", &["white", "hvit", "hvitt", "hvite"]),
];
const DIM_WORDS: &[&str] = &["dim", "dimm", "dimme", "demp", "dempe", "darker", "dimmer", "mørkere"];
const BRIGHT_WORDS: &[&str] = &["brighten", "brighter", "lysere", "sterkere"];
const WARM_WORDS: &[&str] = &["warm", "warmer", "varm", "varmt", "varmere", "cosy", "cozy"];
const COOL_WORDS: &[&str] = &["cool", "cooler", "cold", "colder", "kald", "kaldt", "kaldere"];
/// Verbs that make a colour a light request without the word "light": "make the bedroom red".
const COLOUR_VERBS: &[&str] = &["make", "set", "turn", "change", "gjør", "sett", "skru", "endre", "farg"];
/// Words of a level request that are not the room or light.
const LEVEL_FILLER: &[&str] = &[
    "too",
    "it's",
    "its",
    "er",
    "for",
    "bright",
    "dark",
    "lyst",
    "mørkt",
    "percent",
    "prosent",
    "brightness",
    "lysstyrke",
    "lysstyrken",
    "full",
    "max",
    "maximum",
    "up",
    "down",
    "opp",
    "ned",
    "a",
    "bit",
    "little",
    "litt",
    "more",
    "mer",
    "make",
    "set",
    "change",
    "gjør",
    "sett",
    "endre",
    "to",
    "til",
    "color",
    "colour",
    "farge",
    "fargen",
    "light",
    "lighting",
    "belysning",
    "turn",
    "skru",
    "switch",
];

/// A brightness or colour request, from the words and the raw text (for "30%").
fn light_level(words: &[String], text: &str) -> Option<LightLevel> {
    let has = |w: &str| words.iter().any(|x| x == w);
    let any = |list: &[&str]| words.iter().any(|w| list.contains(&w.as_str()));
    let light = any(LIGHT_WORDS);
    let first = words.iter().map(String::as_str).find(|w| !POLITE.contains(w)).unwrap_or_default();
    let raw: Vec<&str> = text.split_whitespace().map(|t| t.trim_matches(|c: char| ",.!?".contains(c))).collect();
    let percent = raw.iter().enumerate().find_map(|(i, t)| {
        let number: u8 = t.trim_end_matches('%').parse().ok()?;
        let unit = t.ends_with('%')
            || raw.get(i + 1).is_some_and(|n| ["percent", "prosent", "%"].contains(&n.to_lowercase().as_str()));
        unit.then_some(number.min(100))
    });
    // "It's too bright in here", "det er for mørkt på kjøkkenet".
    let too = words
        .windows(2)
        .any(|p| (p[0] == "too" || p[0] == "for") && ["bright", "lyst", "dark", "mørkt"].contains(&p[1].as_str()));
    if too {
        return Some(if has("bright") || has("lyst") { LightLevel::Dimmer } else { LightLevel::Brighter });
    }
    let dim = any(DIM_WORDS);
    let about_brightness = light || dim || has("brightness") || has("lysstyrke") || has("lysstyrken");
    if let Some(percent) = percent {
        if about_brightness || ["set", "sett"].contains(&first) {
            return Some(LightLevel::Percent(percent));
        }
    }
    if about_brightness && (has("full") || has("max") || has("maximum")) {
        return Some(LightLevel::Percent(100));
    }
    if dim {
        return Some(LightLevel::Dimmer);
    }
    if any(BRIGHT_WORDS) {
        return Some(LightLevel::Brighter);
    }
    if light && (has("up") || has("opp")) {
        return Some(LightLevel::Brighter);
    }
    if light && (has("down") || has("ned")) {
        return Some(LightLevel::Dimmer);
    }
    // Warm and cool only with a light word: "make it warmer in here" is the heating.
    if light && any(WARM_WORDS) {
        return Some(LightLevel::Warm);
    }
    if light && any(COOL_WORDS) {
        return Some(LightLevel::Cool);
    }
    if light || COLOUR_VERBS.contains(&first) {
        for (css, spoken) in COLOURS {
            if any(spoken) {
                return Some(LightLevel::Color((*css).into()));
            }
        }
    }
    None
}

/// The request without the level words, for finding the lights.
fn level_target(text: &str) -> String {
    let level_word = |w: &str| {
        LEVEL_FILLER.contains(&w)
            || DIM_WORDS.contains(&w)
            || BRIGHT_WORDS.contains(&w)
            || WARM_WORDS.contains(&w)
            || COOL_WORDS.contains(&w)
            || COLOURS.iter().any(|(_, spoken)| spoken.contains(&w))
            || w.chars().all(|c| c.is_ascii_digit())
    };
    text.split_whitespace().filter(|t| !level_word(&normalize(t))).collect::<Vec<_>>().join(" ")
}

/// "Who's home", "is anyone home", "is Ingrid home", "hvem er hjemme", "er Ingrid hjemme".
fn whos_home(words: &[String]) -> Option<Option<String>> {
    let at = words.iter().position(|w| w == "home" || w == "hjemme")?;
    let first = words.first()?.as_str();
    if ["who's", "whos", "who", "hvem"].contains(&first) && at <= 3 {
        return Some(None);
    }
    if !["is", "er"].contains(&first) || at == 1 || at > 3 || at + 2 < words.len() {
        return None;
    }
    let name: Vec<&str> =
        words[1..at].iter().map(String::as_str).filter(|w| !["at", "still", "fortsatt", "på"].contains(w)).collect();
    let anyone = ["anyone", "anybody", "someone", "somebody", "noen", "everyone", "everybody", "alle"];
    match name.as_slice() {
        [] => None,
        [one] if anyone.contains(one) => Some(None),
        // Not a person: "is it cold at home".
        words if words.iter().any(|w| ["it", "the", "det", "den", "there", "anything", "noe"].contains(w)) => None,
        words => Some(Some(words.join(" "))),
    }
}

/// "Good morning" on its own, or asking for the briefing.
fn is_briefing(words: &[String]) -> bool {
    let phrase: Vec<&str> = words
        .iter()
        .map(String::as_str)
        .filter(|w| !["and", "hey", "hi", "ok", "og", "hei", "to", "you", "deg"].contains(w))
        .collect();
    matches!(phrase.as_slice(), ["good", "morning"] | ["god", "morgen"] | ["morning"] | ["morn"])
        || words.iter().any(|w| w == "briefing")
        || words.windows(2).any(|p| (p[0] == "brief" && p[1] == "me") || (p[0] == "dagens" && p[1] == "oversikt"))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MusicCommand {
    /// The user's Liked Songs, shuffled.
    Liked,
    /// One of the user's playlists, by name.
    Playlist(String),
    /// A song search: "careless whisper", "hello by adele".
    Song(String),
    Resume,
    Pause,
    Next,
    Previous,
    Louder,
    Quieter,
}

const PLAY_WORDS: &[&str] = &["play", "spill"];
/// Polite words before "play": "can you please play", "kan du spille".
const POLITE: &[&str] = &[
    "can", "could", "would", "will", "you", "please", "just", "now", "ok", "okay", "kan", "du", "vil", "vær", "så",
    "snill",
];
const MUSIC_WORDS: &[&str] =
    &["music", "musikk", "musikken", "song", "songs", "sang", "sangen", "låt", "låten", "track", "spotify"];
/// Every word of a Liked Songs request comes from here ("my liked songs", "de likte sangene mine"),
/// so a song title that contains "like" is still a song.
const LIKED_WORDS: &[&str] = &[
    "liked",
    "like",
    "likes",
    "favorite",
    "favorites",
    "favourite",
    "favourites",
    "likte",
    "favoritt",
    "favoritter",
    "favorittsanger",
    "favorittsangene",
];
const LIKED_FILLER: &[&str] = &[
    "my",
    "the",
    "songs",
    "song",
    "playlist",
    "music",
    "tracks",
    "list",
    "some",
    "of",
    "from",
    "mine",
    "min",
    "mi",
    "de",
    "sangene",
    "sanger",
    "låtene",
    "låter",
    "musikk",
    "musikken",
    "spillelisten",
    "spilleliste",
    "spillelista",
    "liste",
    "lista",
    "fra",
];
const PLAYLIST_WORDS: &[&str] = &["playlist", "spilleliste", "spillelisten", "spillelista"];
const PLAYLIST_FILLER: &[&str] = &["my", "the", "called", "named", "min", "mi", "mine", "som", "heter", "den", "det"];
/// Dropped from the end of a play request: "... please", "... on the sonos".
const TAIL: &[&str] = &[
    "please",
    "takk",
    "now",
    "nå",
    "for",
    "me",
    "meg",
    "on",
    "the",
    "sonos",
    "speaker",
    "spotify",
    "på",
    "høyttaleren",
];
const GENERIC: &[&str] = &["music", "some", "musikk", "noe", "the", "litt", "something"];

fn music(words: &[String]) -> Option<MusicCommand> {
    let has = |w: &str| words.iter().any(|x| x == w);
    let start = words.iter().position(|w| !POLITE.contains(&w.as_str()))?;
    if PLAY_WORDS.contains(&words[start].as_str()) {
        let mut rest: Vec<&str> = words[start + 1..].iter().map(String::as_str).collect();
        // "spill av" is "play" in Norwegian.
        if words[start] == "spill" && rest.first() == Some(&"av") {
            rest.remove(0);
        }
        while rest.last().is_some_and(|w| TAIL.contains(w)) {
            rest.pop();
        }
        if rest.is_empty() {
            return Some(MusicCommand::Resume);
        }
        if rest.iter().all(|w| GENERIC.contains(w)) {
            return Some(MusicCommand::Liked);
        }
        if rest.iter().any(|w| LIKED_WORDS.contains(w))
            && rest.iter().all(|w| LIKED_WORDS.contains(w) || LIKED_FILLER.contains(w))
        {
            return Some(MusicCommand::Liked);
        }
        if rest.iter().any(|w| PLAYLIST_WORDS.contains(w)) {
            let name: Vec<&str> =
                rest.iter().copied().filter(|w| !PLAYLIST_WORDS.contains(w) && !PLAYLIST_FILLER.contains(w)).collect();
            return Some(if name.is_empty() { MusicCommand::Liked } else { MusicCommand::Playlist(name.join(" ")) });
        }
        return Some(MusicCommand::Song(rest.join(" ")));
    }
    if words.iter().any(|w| LIGHT_WORDS.contains(&w.as_str())) {
        return None;
    }
    let about_music = words.iter().any(|w| MUSIC_WORDS.contains(&w.as_str()));
    if has("pause") || (about_music && ["stop", "stopp", "off", "av", "mute"].iter().any(|w| has(w))) {
        return Some(MusicCommand::Pause);
    }
    if has("resume") || has("unpause") || (about_music && ["continue", "fortsett", "start"].iter().any(|w| has(w))) {
        return Some(MusicCommand::Resume);
    }
    if has("skip") || has("neste") || (has("next") && (about_music || words.len() <= 3)) {
        return Some(MusicCommand::Next);
    }
    if has("previous") || has("forrige") {
        return Some(MusicCommand::Previous);
    }
    let up = has("up") || has("opp");
    let down = has("down") || has("ned");
    let volume = ["volume", "volumet", "turn", "skru", "it", "music", "musikken"].iter().any(|w| has(w));
    if has("louder") || has("høyere") || (volume && up) {
        return Some(MusicCommand::Louder);
    }
    if ["quieter", "softer", "lower", "lavere"].iter().any(|w| has(w)) || (volume && down) {
        return Some(MusicCommand::Quieter);
    }
    None
}

/// "What time is it (now)?", "what's the time", "hva/hvor mye er klokka (nå)?"; the local time
/// only: anything more ("in Tokyo", "does the shop close") is left to the language model.
fn is_time_question(words: &[String]) -> bool {
    let phrase = words.iter().map(String::as_str).filter(|w| !["now", "please", "right", "nå", "da"].contains(w));
    let phrase: Vec<&str> = phrase.collect();
    matches!(
        phrase.as_slice(),
        ["what", "time", "is", "it"]
            | ["what's", "the", "time"]
            | ["what", "is", "the", "time"]
            | ["hva", "er", "klokka" | "klokken"]
            | ["hvor", "mye", "er", "klokka" | "klokken"]
    )
}

const WEATHER_WORDS: &[&str] = &[
    // English
    "weather",
    "temperature",
    "forecast",
    "rain",
    "raining",
    "snow",
    "snowing",
    "sunny",
    "cold",
    "warm",
    "hot",
    "degrees",
    "wind",
    "windy",
    // Norwegian
    "vær",
    "været",
    "været",
    "værmelding",
    "værmeldingen",
    "temperatur",
    "temperaturen",
    "regn",
    "regne",
    "regner",
    "snø",
    "snøe",
    "snør",
    "sol",
    "kaldt",
    "varmt",
    "grader",
    "vind",
    "vindfullt",
];
/// Word parts that survive small mishearings ("trømperaturen", "weather's").
const WEATHER_STEMS: &[&str] = &["weather", "forecast", "peratur", "værmeld", "været"];
/// Words a place name follows.
const PLACE_MARKERS: &[&str] = &["in", "for", "at", "i", "på"];
/// Words that end a place name.
const STOP_WORDS: &[&str] = &[
    // English
    "today", "tomorrow", "tonight", "now", "right", "please", "like", "going", "be", "this", "at", "on", "for",
    "in", // Norwegian
    "i", "på", "for", "dag", "idag", "morgen", "morgenen", "imorgen", "kveld", "natt", "nå", "akkurat", "da", "takk",
    "være", "bli", "blir",
];
const FILLER: &[&str] = &["the", "byen"];

/// "joke", "vits" and their forms; checked before weather, so "a joke about the weather" is a joke.
/// "joik" is how NB-Whisper sometimes spells "joke".
const JOKE_STEMS: &[&str] = &["joke", "joik", "vits", "morsom", "funny"];

/// "Which lights are on", "is anything left on", "står det noe lys på", "er lyset på i stua".
fn is_lights_question(words: &[String]) -> bool {
    let has = |w: &str| words.iter().any(|x| x == w);
    let question = ["which", "what", "are", "is", "any", "hvilke", "hvilket", "er", "står", "hva"];
    let asks = words.first().is_some_and(|w| question.contains(&w.as_str()));
    let about = words.iter().any(|w| LIGHT_WORDS.contains(&w.as_str()))
        || ["anything", "everything", "something", "noe", "alt"].iter().any(|w| has(w));
    asks && about && (has("on") || has("på") || has("left"))
}

const LIGHT_WORDS: &[&str] =
    &["light", "lights", "lamp", "lamps", "lys", "lyset", "lysene", "lampe", "lampen", "lampene"];

pub fn parse(text: &str) -> Intent {
    let words: Vec<String> = text.split_whitespace().map(normalize).filter(|w| !w.is_empty()).collect();
    // Timers before music ("pause the timer"), except for a song called "Timer".
    let play = words.iter().find(|w| !POLITE.contains(&w.as_str())).is_some_and(|w| PLAY_WORDS.contains(&w.as_str()));
    if !play {
        // First: items are any words ("add pause buttons to the list").
        if let Some(command) = shopping::parse(text) {
            return Intent::ShoppingList(command);
        }
        if let Some(command) = timer::parse(text) {
            return Intent::Timer(command);
        }
        // Before music: "next bus" is not "next song".
        if let Some(query) = transit::parse(text) {
            return Intent::Transit(query);
        }
        if let Some(query) = power::parse(text) {
            return Intent::Power(query);
        }
        if is_briefing(&words) {
            return Intent::Briefing;
        }
    }
    // Then music: song titles contain every other kind of word ("Here Comes the Rain Again").
    if let Some(command) = music(&words) {
        return Intent::Music(command);
    }
    if is_time_question(&words) {
        return Intent::Time;
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
    if let Some(level) = light_level(&words, text) {
        return Intent::LightLevel { level, target: level_target(text) };
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
    let switching = ["turn", "switch", "skru", "slå"].iter().any(|w| has(w));
    let pronoun = ["them", "it", "those", "these", "they", "that", "dem", "den", "det", "de"].iter().any(|w| has(w));
    // "off"/"av" first: in Norwegian "på" is also the preposition ("av på kjøkkenet").
    let on = if has("off") || has("av") {
        Some(false)
    } else if has("on") || has("på") {
        Some(true)
    } else {
        None
    };
    // "Turn them off" names no light; the assistant resolves it against the last lights it used.
    if words.iter().any(|w| LIGHT_WORDS.contains(&w.as_str())) || (switching && pronoun) {
        if let Some(on) = on {
            return Intent::Lights { on, target: text.to_owned() };
        }
    }
    if ["reverse", "undo", "revert", "angre"].iter().any(|w| has(w)) || (has("back") && switching) {
        return Intent::Undo;
    }
    let weather = |w: &String| WEATHER_WORDS.contains(&w.as_str()) || WEATHER_STEMS.iter().any(|s| w.contains(s));
    if !words.iter().any(weather) {
        // Short social replies only; longer sentences that merely contain "thanks" are unknown.
        if words.len() <= 5 {
            if ["thanks", "thank", "takk", "cheers", "tusen"].iter().any(|w| has(w)) {
                return Intent::Thanks;
            }
            let never_mind = words.windows(2).any(|p| p[0] == "never" && p[1] == "mind");
            if never_mind || ["nevermind", "cancel", "stop", "forget", "avbryt", "glem"].iter().any(|w| has(w)) {
                return Intent::Cancel;
            }
        }
        return Intent::Unknown;
    }
    let tomorrow = words.iter().any(|w| w == "tomorrow" || w == "imorgen")
        // Whisper sometimes writes "i morgen" (tomorrow) as "i morgenen".
        || words.windows(2).any(|p| p[0] == "i" && (p[1] == "morgen" || p[1] == "morgenen"));
    let day = if tomorrow { Day::Tomorrow } else { Day::Today };
    // The place follows the last marker that is followed by a name ("i morgen" is not a place).
    let place =
        words.iter().enumerate().rev().filter(|(_, w)| PLACE_MARKERS.contains(&w.as_str())).find_map(|(i, _)| {
            let name: Vec<&str> = words[i + 1..]
                .iter()
                .map(String::as_str)
                .take_while(|w| !STOP_WORDS.contains(w))
                .filter(|w| !FILLER.contains(w))
                .collect();
            (!name.is_empty()).then(|| name.join(" "))
        });
    Intent::Weather { place, day }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn weather(place: Option<&str>, day: Day) -> Intent {
        Intent::Weather { place: place.map(str::to_owned), day }
    }

    #[test]
    fn english_weather_questions() {
        assert_eq!(parse("what's the weather in Oslo?"), weather(Some("oslo"), Day::Today));
        assert_eq!(parse("What is the weather like in New York today"), weather(Some("new york"), Day::Today));
        assert_eq!(parse("will it rain in Bergen tomorrow"), weather(Some("bergen"), Day::Tomorrow));
        assert_eq!(parse("what's the temperature for Trondheim"), weather(Some("trondheim"), Day::Today));
        assert_eq!(parse("what's the forecast for tomorrow"), weather(None, Day::Tomorrow));
        assert_eq!(parse("how's the weather"), weather(None, Day::Today));
    }

    #[test]
    fn norwegian_weather_questions() {
        assert_eq!(parse("Hvordan blir været i Oslo i morgen?"), weather(Some("oslo"), Day::Tomorrow));
        assert_eq!(parse("Hva er temperaturen i Tromsø?"), weather(Some("tromsø"), Day::Today));
        assert_eq!(parse("Blir det regn på Lillehammer i dag?"), weather(Some("lillehammer"), Day::Today));
        assert_eq!(parse("Hvordan er været?"), weather(None, Day::Today));
        assert_eq!(parse("Hva er trømperaturen i Tromsø?"), weather(Some("tromsø"), Day::Today));
        assert_eq!(parse("Hvordan blir været i morgen?"), weather(None, Day::Tomorrow));
        assert_eq!(parse("hvordan blir været i Bergen i morgenen?"), weather(Some("bergen"), Day::Tomorrow));
    }

    #[test]
    fn light_requests() {
        let lights = |text: &str| match parse(text) {
            Intent::Lights { on, .. } => Some(on),
            _ => None,
        };
        assert_eq!(lights("turn off the light in my living room"), Some(false));
        assert_eq!(lights("Turn on the kitchen lights."), Some(true));
        assert_eq!(lights("living room lights off please"), Some(false));
        assert_eq!(lights("skru av lyset i stua"), Some(false));
        assert_eq!(lights("slå på lyset på kjøkkenet"), Some(true));
        assert_eq!(lights("skru av lyset på kjøkkenet"), Some(false));
        assert_eq!(lights("how bright are the lights"), None);
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
        assert_eq!(parse("thanks a lot"), Intent::Thanks);
        assert_eq!(parse("Never mind."), Intent::Cancel);
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
    fn music_requests() {
        use MusicCommand::*;
        let music = |text: &str| match parse(text) {
            Intent::Music(command) => Some(command),
            _ => None,
        };
        let song = |s: &str| Some(Song(s.into()));
        assert_eq!(music("Play my liked songs."), Some(Liked));
        assert_eq!(music("play my like playlist"), Some(Liked));
        assert_eq!(music("Can you play my favorite songs please"), Some(Liked));
        assert_eq!(music("play some music"), Some(Liked));
        assert_eq!(music("spill de likte sangene mine"), Some(Liked));
        assert_eq!(music("play Careless Whisper"), song("careless whisper"));
        assert_eq!(music("Play Hello by Adele on the Sonos."), song("hello by adele"));
        assert_eq!(music("play Here Comes the Rain Again"), song("here comes the rain again"));
        assert_eq!(music("play I Like It"), song("i like it"));
        assert_eq!(music("spill av Kygo"), song("kygo"));
        assert_eq!(music("play my running playlist"), Some(Playlist("running".into())));
        assert_eq!(music("play the playlist called Chill Vibes"), Some(Playlist("chill vibes".into())));
        assert_eq!(music("play"), Some(Resume));
        assert_eq!(music("pause"), Some(Pause));
        assert_eq!(music("stop the music"), Some(Pause));
        assert_eq!(music("turn off the music"), Some(Pause));
        assert_eq!(music("stopp musikken"), Some(Pause));
        assert_eq!(music("continue the music"), Some(Resume));
        assert_eq!(music("next song"), Some(Next));
        assert_eq!(music("skip this one"), Some(Next));
        assert_eq!(music("neste sang"), Some(Next));
        assert_eq!(music("previous song"), Some(Previous));
        assert_eq!(music("turn it up"), Some(Louder));
        assert_eq!(music("louder"), Some(Louder));
        assert_eq!(music("turn the music down"), Some(Quieter));
        assert_eq!(music("skru ned musikken"), Some(Quieter));
        // Not music.
        assert_eq!(music("turn off the light in my living room"), None);
        assert_eq!(music("turn up the lights"), None);
        assert_eq!(music("stop"), None);
        assert_eq!(music("what's the weather tomorrow"), None);
    }

    #[test]
    fn time_questions() {
        assert_eq!(parse("What time is it?"), Intent::Time);
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
        assert_eq!(parse("pause"), Intent::Music(MusicCommand::Pause));
        assert!(matches!(parse("next bus"), Intent::Transit(_)));
        assert_eq!(parse("next song"), Intent::Music(MusicCommand::Next));
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
        assert!(matches!(parse("run movie time"), Intent::Script(_)));
        assert!(matches!(parse("kjør skriptet movie time"), Intent::Script(_)));
        assert!(matches!(parse("set a timer for ten minutes"), Intent::Timer(_)));
        assert!(matches!(parse("set the volume to 20"), Intent::Unknown));
        assert!(matches!(parse("add milk to the shopping list"), Intent::ShoppingList(_)));
        assert!(matches!(parse("what's on the shopping list"), Intent::ShoppingList(ListCommand::Read)));
        assert!(matches!(parse("play my shopping playlist"), Intent::Music(_)));
    }

    #[test]
    fn light_levels() {
        use LightLevel::*;
        let level = |text: &str| match parse(text) {
            Intent::LightLevel { level, target } => Some((level, target)),
            _ => None,
        };
        let is = |text: &str, want: LightLevel, target: &str| {
            assert_eq!(level(text), Some((want, target.to_owned())), "{text}");
        };
        is("dim the living room to 30%", Percent(30), "the living room");
        is("set the kitchen lights to 50 percent", Percent(50), "the kitchen lights");
        is("dim the lights in the bedroom", Dimmer, "the lights in the bedroom");
        is("make the living room brighter", Brighter, "the living room");
        is("turn up the lights", Brighter, "the lights");
        is("skru ned lyset i stua", Dimmer, "lyset i stua");
        is("demp lyset på soverommet", Dimmer, "lyset på soverommet");
        is("make the bedroom lights red", Color("red".into()), "the bedroom lights");
        is("set the living room to blue", Color("blue".into()), "the living room");
        is("gjør lyset i stua rødt", Color("red".into()), "lyset i stua");
        is("make the lights warmer", Warm, "the lights");
        is("full brightness in the kitchen", Percent(100), "in the kitchen");
        is("make them brighter", Brighter, "them");
        is("it's a bit too bright in here", Dimmer, "in here");
        is("it's too dark in the kitchen", Brighter, "in the kitchen");
        is("det er for mørkt på kjøkkenet", Brighter, "det på kjøkkenet");
        assert_eq!(level("make it warmer in here"), None);
        assert_eq!(level("set the living room to relax"), None);
        assert!(matches!(parse("play Purple Rain"), Intent::Music(_)));
        assert!(matches!(parse("turn on the kitchen lights"), Intent::Lights { on: true, .. }));
    }

    #[test]
    fn people_power_and_briefing() {
        assert_eq!(parse("who's home?"), Intent::WhosHome(None));
        assert_eq!(parse("is anyone home"), Intent::WhosHome(None));
        assert_eq!(parse("is Ingrid home?"), Intent::WhosHome(Some("ingrid".into())));
        assert_eq!(parse("er Ingrid hjemme?"), Intent::WhosHome(Some("ingrid".into())));
        assert_eq!(parse("hvem er hjemme"), Intent::WhosHome(None));
        assert!(!matches!(parse("is it cold at home"), Intent::WhosHome(_)));
        assert!(matches!(parse("when is electricity cheapest tonight"), Intent::Power(_)));
        assert_eq!(parse("Good morning!"), Intent::Briefing);
        assert_eq!(parse("god morgen"), Intent::Briefing);
        assert_eq!(parse("give me my morning briefing"), Intent::Briefing);
        assert!(!matches!(parse("good morning, what's the weather in Bergen"), Intent::Briefing));
    }

    #[test]
    fn other_requests_are_unknown() {
        assert_eq!(parse("what's the capital of France"), Intent::Unknown);
        assert_eq!(parse(""), Intent::Unknown);
    }
}
