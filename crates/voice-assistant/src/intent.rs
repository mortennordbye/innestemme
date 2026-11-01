//! Rule-based intent parsing, English and Norwegian. A placeholder for the LLM step: enough to
//! route a weather question and pull out the place and day.

use crate::dialog::normalize;
use crate::house::{self, HouseCommand, Routine};
use crate::power::{self, PowerQuery};
use crate::shopping::{self, ListCommand};
use crate::smalltalk::Chat;
use crate::timer::{self, TimerCommand};
use crate::transit::{self, TransitQuery};
use crate::tv::{self, TvCommand};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Day {
    Today,
    /// This evening and the night after it.
    Tonight,
    Tomorrow,
    /// The day after tomorrow.
    AfterTomorrow,
    /// The next such day, today when it is one.
    Weekday(jiff::civil::Weekday),
    /// The coming Saturday and Sunday, or what is left of them.
    Weekend,
    /// "Next Monday": the next such day after today, a week ahead when today is one.
    NextWeekday(jiff::civil::Weekday),
    /// "Next weekend": the coming one, or the one after when it is the weekend now.
    NextWeekend,
}

/// What a weather question is about: everything, or only rain or wind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    General,
    /// "Will it rain later?", "do I need an umbrella?": when rain starts or stops.
    Rain,
    /// "How windy is it?": the wind in metres per second.
    Wind,
}

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
/// Verbs that only change the brightness next to a light or brightness word: "lower the brightness".
const RAISE_WORDS: &[&str] = &["increase", "raise", "øk", "øke"];
const LOWER_WORDS: &[&str] = &["decrease", "lower", "reduce", "senk", "senke"];
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

/// "Off" as the on/off word: "turn off", "skru av". Not "av" as the preposition in "noen av lysene"
/// (some of the lights).
pub fn says_off(words: &[String]) -> bool {
    const OF: &[&str] =
        &["noen", "alle", "en", "ett", "ei", "ingen", "hver", "begge", "flere", "mange", "resten", "del", "to", "tre"];
    words.iter().enumerate().any(|(i, w)| w == "off" || (w == "av" && !(i > 0 && OF.contains(&words[i - 1].as_str()))))
}

const FLOOR_WORDS: &[&str] = &["etasje", "etasjen", "etg", "floor"];

/// Whether the number at `raw[i]` is a time of day: "at 7 pm", "klokka 7", "at 7" in a switching
/// request ("turn on the lights at 7"), or "7pm", "19:00" in one word.
fn clock_time(raw: &[&str], i: usize) -> bool {
    let lower = |j: usize| raw.get(j).map(|t| t.to_lowercase()).unwrap_or_default();
    let Ok(number) = raw[i].parse::<u8>() else { return false };
    let after = lower(i + 1);
    let before = if i > 0 { lower(i - 1) } else { String::new() };
    // Norwegian says a time with "klokka": after "på" a bare number is a level ("skru på lyset på
    // 10"), and "i" after it is a room ("på 10 i stua") unless a time of day follows ("på 11 i kveld").
    let pa = before == "på";
    let day_part = ["kveld", "morgen", "morra", "natt", "ettermiddag"].contains(&lower(i + 2).as_str());
    let said_as_time = ["am", "pm", "a.m", "p.m", "o'clock", "oclock", "tonight", "tomorrow"]
        .contains(&after.trim_end_matches('.'))
        || (["i", "in"].contains(&after.as_str()) && (!pa || day_part));
    let clock = ["klokka", "klokken", "kl"].contains(&before.trim_end_matches('.'));
    let switching = raw.iter().any(|t| ["turn", "switch", "skru", "slå"].contains(&t.to_lowercase().as_str()));
    said_as_time && number <= 24 || clock || (switching && !pa && (1..=12).contains(&number))
}

/// "Turn the lights off at 11", "turn on the lights at 7 pm": a switch at a time of day, which the
/// assistant cannot schedule. Acting now would be wrong.
fn scheduled_switch(words: &[String], text: &str) -> bool {
    let raw: Vec<&str> = text.split_whitespace().map(|t| t.trim_matches(|c: char| ",.!?".contains(c))).collect();
    let switching = words.iter().any(|w| ["turn", "switch", "skru", "slå"].contains(&w.as_str()));
    let light = words.iter().any(|w| LIGHT_WORDS.contains(&w.as_str()));
    let on_off = says_off(words) || words.iter().any(|w| w == "on" || w == "på");
    let unit = raw.iter().any(|t| t.ends_with('%') || ["percent", "prosent"].contains(&t.to_lowercase().as_str()));
    let at_time = raw.iter().enumerate().any(|(i, t)| {
        let before = if i > 0 { raw[i - 1].to_lowercase() } else { String::new() };
        let marker = ["at", "på", "klokka", "klokken", "kl"].contains(&before.as_str());
        let one_word = {
            let t = t.to_lowercase();
            let digits = t.trim_end_matches("pm").trim_end_matches("am");
            (digits.len() < t.len() && digits.parse::<u8>().is_ok_and(|n| (1..=12).contains(&n)))
                || t.split_once(':').is_some_and(|(h, m)| h.parse::<u8>().is_ok() && m.parse::<u8>().is_ok())
        };
        let floor = raw.get(i + 1).is_some_and(|n| FLOOR_WORDS.contains(&n.to_lowercase().as_str()));
        (marker && clock_time(&raw, i) && !floor) || (marker && one_word)
    });
    (switching || light) && light && on_off && !unit && at_time
}

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
        // "Set the living room lights to 50", "sett lyset på 50": a level after "to"/"at" counts when the
        // request is about light. Not a clock time ("turn on the lights at 7"), a floor ("lyset på 2.
        // etasje"), or with "off": "turn the lights off at 11" is not a brightness.
        let before = if i > 0 { raw[i - 1].to_lowercase() } else { String::new() };
        let to = ["to", "til", "at", "på"].contains(&before.as_str())
            && !says_off(words)
            && !(["at", "på"].contains(&before.as_str()) && clock_time(&raw, i))
            && !raw.get(i + 1).is_some_and(|n| FLOOR_WORDS.contains(&n.to_lowercase().as_str()));
        (unit || to).then_some((number.min(100), unit))
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
    if let Some((percent, unit)) = percent {
        if about_brightness || (unit && ["set", "sett"].contains(&first)) {
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
    if about_brightness && any(RAISE_WORDS) {
        return Some(LightLevel::Brighter);
    }
    if about_brightness && any(LOWER_WORDS) {
        return Some(LightLevel::Dimmer);
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
            || RAISE_WORDS.contains(&w)
            || LOWER_WORDS.contains(&w)
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

/// Words that make a "where is" request about something other than a person.
const NOT_A_PERSON: &[&str] = &[
    "the",
    "a",
    "an",
    "my",
    "your",
    "our",
    "it",
    "this",
    "that",
    "there",
    "here",
    "you",
    "i",
    "me",
    "we",
    "he",
    "she",
    "they",
    "everyone",
    "everybody",
    "anyone",
    "anybody",
    "someone",
    "somebody",
    "nearest",
    "closest",
    "den",
    "det",
    "de",
    "min",
    "mi",
    "mitt",
    "mine",
    "din",
    "du",
    "jeg",
    "meg",
    "vi",
    "han",
    "hun",
    "dere",
    "alle",
    "noen",
    "nærmeste",
];

/// A name of one or two words that could be a person's.
fn person_name(words: &[String]) -> Option<String> {
    let ok = !words.is_empty()
        && words.len() <= 2
        && words.iter().all(|w| !NOT_A_PERSON.contains(&w.as_str()) && !w.chars().any(|c| c.is_ascii_digit()));
    ok.then(|| words.iter().map(|w| w.trim_end_matches("'s")).collect::<Vec<_>>().join(" "))
}

/// "Where is Ingrid", "where's Ingrid", "is Ingrid at work", "when will Ingrid be home", "hvor er
/// Ingrid", "er Ingrid på jobb", "når kommer Ingrid hjem".
fn where_is(words: &[String]) -> Option<Intent> {
    let w: Vec<&str> = words.iter().map(String::as_str).collect();
    let found = |name: String, zone: Option<String>, arriving: bool| Intent::WhereIs { name, zone, arriving };
    match w.as_slice() {
        ["where's" | "wheres", rest @ ..] | ["where", "is" | "are", rest @ ..] | ["hvor", "er", rest @ ..] => {
            let rest: Vec<String> =
                rest.iter().filter(|w| !["now", "right", "nå"].contains(w)).map(|w| (*w).to_owned()).collect();
            if let [one] = rest.as_slice() {
                if ["everyone", "everybody", "alle"].contains(&one.as_str()) {
                    return Some(Intent::WhosHome(None));
                }
            }
            if w[1] == "are" {
                return None;
            }
            return person_name(&rest).map(|name| found(name, None, false));
        }
        ["when" | "når", ..] => {
            let homeward = ["home", "hjem", "hjemme", "back", "tilbake"];
            if !w.iter().any(|x| homeward.contains(x)) || w.len() > 7 {
                return None;
            }
            let verbs = ["will", "is", "does", "did", "should", "kommer", "er", "blir", "skal"];
            let start = w.iter().position(|x| verbs.contains(x))? + 1;
            let ends = ["be", "get", "gets", "come", "coming", "back", "home", "hjem", "hjemme", "komme", "tilbake"];
            let end = start + w[start..].iter().position(|x| ends.contains(x))?;
            let name: Vec<String> = w[start..end].iter().map(|w| (*w).to_owned()).collect();
            return person_name(&name).map(|name| found(name, None, true));
        }
        _ => {}
    }
    // "Is Ingrid at work", "er Ingrid på jobb": a short question, a zone after the name.
    if !["is", "er"].contains(w.first()?) || w.len() > 6 {
        return None;
    }
    let at = w.iter().position(|x| ["at", "på"].contains(x))?;
    let name: Vec<String> = w[1..at].iter().map(|w| (*w).to_owned()).collect();
    let zone: Vec<&str> = w[at + 1..].iter().copied().filter(|x| !["the", "her", "his", "sin"].contains(x)).collect();
    let home = ["home", "hjemme", "hjem"];
    // "Is dinner at seven", "is Kiwi at Storo open": a time or a state, not a zone.
    let not_a_zone = [
        "one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten", "eleven", "twelve", "noon",
        "midnight", "o'clock", "open", "closed", "åpen", "åpent", "stengt", "ett", "tre", "fem", "seks", "sju", "syv",
        "åtte", "ni", "ti", "elleve", "tolv",
    ];
    if zone.is_empty()
        || zone.len() > 2
        || zone.iter().any(|z| home.contains(z) || not_a_zone.contains(z) || z.chars().any(|c| c.is_ascii_digit()))
    {
        return None;
    }
    person_name(&name).map(|name| found(name, Some(zone.join(" ")), false))
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
    /// "Increase the sound by 25%": percentage points up (or down when negative).
    VolumeBy(i8),
    /// "Set the volume to 40 percent".
    VolumeTo(u8),
    /// "Stop the music": ends it, where a bare "stop" pauses.
    Stop,
    /// Mute (true) or unmute the speaker, leaving the music playing.
    Mute(bool),
    /// "What's playing?"
    NowPlaying,
    Shuffle(bool),
    Repeat(Repeat),
    /// "Play music by Ghost", "play the artist Ghost".
    Artist(String),
    /// "Play the album Rumours".
    Album(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Repeat {
    Off,
    All,
    /// The current song.
    One,
}

const PLAY_WORDS: &[&str] = &["play", "spill"];
/// Polite words before "play": "can you please play", "kan du spille".
const POLITE: &[&str] = &[
    "can", "could", "would", "will", "you", "please", "just", "now", "ok", "okay", "kan", "du", "vil", "vær", "så",
    "snill",
];
/// Replies that send the assistant back to sleep.
const NOT_FOR_YOU: &[&str] = &[
    "no",
    "nope",
    "not you",
    "not now",
    "go back to sleep",
    "back to sleep",
    "false alarm",
    "wrong",
    "nothing",
    "ignore that",
    "ignore me",
    // Said over an answer: stop, not a remark to answer.
    "shut up",
    "be quiet",
    "quiet",
    "hold kjeft",
    "vær stille",
    "nei",
    "ikke deg",
    "ikke nå",
    "ingenting",
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
const LIKED_HEARD_AS: &[&str] = &["light", "lights", "life", "life's", "lifes"];
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
const GENERIC: &[&str] = &["music", "some", "musikk", "noe", "the", "litt", "something", "a", "song", "songs"];

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
        if let Some(album) = after_any(&rest, &[&["the", "album"], &["album"], &["albumet"]]) {
            return Some(MusicCommand::Album(album));
        }
        let by_artist: &[&[&str]] = &[
            &["the", "artist"],
            &["artist"],
            &["artisten"],
            &["music", "by"],
            &["songs", "by"],
            &["something", "by"],
            &["some", "music", "by"],
            &["some", "songs", "by"],
            &["musikk", "av"],
            &["noe", "av"],
            &["litt", "musikk", "av"],
        ];
        if let Some(artist) = after_any(&rest, by_artist) {
            return Some(MusicCommand::Artist(artist));
        }
        if rest.iter().all(|w| GENERIC.contains(w)) {
            return Some(MusicCommand::Liked);
        }
        if rest.iter().any(|w| LIKED_WORDS.contains(w))
            && rest.iter().all(|w| LIKED_WORDS.contains(w) || LIKED_FILLER.contains(w))
        {
            return Some(MusicCommand::Liked);
        }
        // Whisper's "liked": "play my light songs", "play my life's songs". Only with "songs", so
        // "play Light" stays a song.
        let songs = rest.iter().any(|w| ["song", "songs"].contains(w));
        if songs
            && rest.iter().any(|w| LIKED_HEARD_AS.contains(w))
            && rest.iter().all(|w| LIKED_HEARD_AS.contains(w) || LIKED_FILLER.contains(w))
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
    let now_playing = (has("what") || has("what's") || has("whats") || has("who") || has("hva"))
        && (has("playing")
            || has("song")
            || has("sings")
            || has("singing")
            || has("artist")
            || has("spilles")
            || has("sangen"));
    if now_playing && !has("play") {
        return Some(MusicCommand::NowPlaying);
    }
    let off = ["off", "av", "stop", "stopp", "disable"].iter().any(|w| has(w));
    if has("shuffle") || has("shuffling") || has("tilfeldig") {
        return Some(MusicCommand::Shuffle(!off));
    }
    if has("repeat") || has("repeating") || has("gjenta") {
        let one = ["this", "song", "track", "one", "denne", "sangen"].iter().any(|w| has(w));
        return Some(MusicCommand::Repeat(match (off, one) {
            (true, _) => Repeat::Off,
            (false, true) => Repeat::One,
            (false, false) => Repeat::All,
        }));
    }
    if has("unmute") || (has("demp") && has("ikke")) {
        return Some(MusicCommand::Mute(false));
    }
    if (has("mute") || has("demp")) && (about_music || words.len() <= 2) {
        return Some(MusicCommand::Mute(true));
    }
    if about_music && (has("stop") || has("stopp")) {
        return Some(MusicCommand::Stop);
    }
    if has("pause") || (about_music && ["off", "av"].iter().any(|w| has(w))) {
        return Some(MusicCommand::Pause);
    }
    if has("resume") || has("unpause") || (about_music && ["continue", "fortsett", "start"].iter().any(|w| has(w))) {
        return Some(MusicCommand::Resume);
    }
    // "Neste" is also "neste helg", "neste buss": only music when said of music or on its own, and
    // never of a day or a week ("og neste helg", "and next weekend" follow a weather answer).
    const WHEN_WORDS: &[&str] = &[
        "weekend",
        "week",
        "day",
        "month",
        "year",
        "monday",
        "tuesday",
        "wednesday",
        "thursday",
        "friday",
        "saturday",
        "sunday",
        "game",
        "match",
        "helg",
        "helgen",
        "uke",
        "uka",
        "dag",
        "dagen",
        "måned",
        "år",
        "mandag",
        "tirsdag",
        "onsdag",
        "torsdag",
        "fredag",
        "lørdag",
        "søndag",
        "kamp",
        "kampen",
    ];
    let about_when = words.iter().any(|w| WHEN_WORDS.contains(&w.trim_end_matches("'s")));
    // "Kan du hoppe til neste", "ta neste": a skip verb with it is music too.
    let skip_verb = ["hopp", "hoppe", "ta", "gå", "go", "skip"].iter().any(|w| has(w));
    let next = has("next") || has("neste");
    if has("skip") || (next && !about_when && (about_music || skip_verb || words.len() <= 3)) {
        return Some(MusicCommand::Next);
    }
    if has("previous") || has("forrige") {
        return Some(MusicCommand::Previous);
    }
    const VOLUME: &[&str] = &["volume", "volumet", "sound", "lyd", "lyden", "turn", "skru", "it", "music", "musikken"];
    // "Lower the brightness", "turn the lights down": the lights, unless the sound is named too.
    let sound = ["volume", "volumet", "sound", "lyd", "lyden", "music", "musikk", "musikken"].iter().any(|w| has(w));
    let lights = ["brightness", "lysstyrke", "lysstyrken"].iter().any(|w| has(w))
        || words.iter().any(|w| LIGHT_WORDS.contains(&w.as_str()));
    if lights && !sound {
        return None;
    }
    let volume = words.iter().any(|w| VOLUME.contains(&w.as_str()));
    // "Up" and "down" only after a volume word ("turn it up", "skru ned lyden"): in "show the bus
    // times ... up it" the "it" comes too late to be turned up.
    let particle = |list: &[&str]| {
        let first_volume = words.iter().position(|w| VOLUME.contains(&w.as_str()));
        words.iter().enumerate().any(|(i, w)| list.contains(&w.as_str()) && first_volume.is_some_and(|v| v < i))
    };
    let up =
        ["increase", "increased", "raise", "raised", "øk", "øke"].iter().any(|w| has(w)) || particle(&["up", "opp"]);
    let down = ["decrease", "decreased", "reduce", "reduced", "lowered", "senk", "senke"].iter().any(|w| has(w))
        || particle(&["down", "ned"]);
    let louder = has("louder") || has("høyere") || (volume && up);
    let quieter = ["quieter", "softer", "lower", "lavere"].iter().any(|w| has(w)) || (volume && down);
    if let Some((to, amount)) = volume_amount(words).filter(|_| volume || louder || quieter) {
        return Some(match (to, louder, quieter) {
            (true, _, _) => MusicCommand::VolumeTo(amount),
            (false, true, false) => MusicCommand::VolumeBy(amount as i8),
            (false, false, true) => MusicCommand::VolumeBy(-(amount as i8)),
            // "Volume 30", "music 30 percent": no direction is a level.
            (false, false, false) => MusicCommand::VolumeTo(amount),
            _ => return None,
        });
    }
    if louder {
        return Some(MusicCommand::Louder);
    }
    if quieter {
        return Some(MusicCommand::Quieter);
    }
    None
}

/// What follows one of these openings of a play request: "the album Rumours" gives "rumours".
fn after_any(rest: &[&str], openings: &[&[&str]]) -> Option<String> {
    openings.iter().find(|o| rest.len() > o.len() && rest.starts_with(o)).map(|o| rest[o.len()..].join(" "))
}

/// A percentage in a volume request, and whether it is the level to set ("to 40") rather than a
/// change ("by 25%", "20 percent up"). A transcriber writes "25%", which normalises to "25".
fn volume_amount(words: &[String]) -> Option<(bool, u8)> {
    let at = words.iter().position(|w| w.parse::<u8>().is_ok_and(|n| (1..=100).contains(&n)))?;
    let amount = words[at].parse().ok()?;
    let to = at > 0 && ["to", "til", "på", "at"].contains(&words[at - 1].as_str());
    Some((to, amount))
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
    "rainy",
    "umbrella",
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
    "blåser",
    "paraply",
    "paraplyen",
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
/// Times of day and other times, which end a place name and are never one ("in the morning",
/// "in the next hour", "på lørdag", "i helgen"). Numbers ("at 6") count as times too.
const TIME_WORDS: &[&str] = &[
    // English
    "morning",
    "afternoon",
    "evening",
    "night",
    "later",
    "soon",
    "next",
    "hour",
    "hours",
    "minute",
    "minutes",
    "noon",
    "midday",
    "weekend",
    "week",
    "rest",
    "day",
    "days",
    "after",
    "o'clock",
    "am",
    "pm",
    "monday",
    "tuesday",
    "wednesday",
    "thursday",
    "friday",
    "saturday",
    "sunday", // Norwegian
    "formiddag",
    "formiddagen",
    "ettermiddag",
    "ettermiddagen",
    "kvelden",
    "natten",
    "senere",
    "etterpå",
    "snart",
    "neste",
    "time",
    "timen",
    "timene",
    "helg",
    "helgen",
    "helga",
    "uke",
    "uka",
    "uken",
    "resten",
    "overmorgen",
    "mandag",
    "tirsdag",
    "onsdag",
    "torsdag",
    "fredag",
    "lørdag",
    "søndag",
];
/// A place that means the home: "the weather at home", "here".
const HOME_WORDS: &[&str] = &["home", "here", "house", "hjemme", "her", "huset"];
/// Clothes asked about for rain, with a verb: "should I bring a jacket", "trenger jeg jakke".
const CLOTHES: &[&str] = &["jacket", "coat", "raincoat", "jakke", "jakken", "regnjakke", "regntøy"];
const CLOTHES_VERBS: &[&str] = &["need", "bring", "take", "wear", "trenger", "ta", "bør"];
const RAIN_WORDS: &[&str] = &[
    "rain",
    "raining",
    "rainy",
    "umbrella",
    "snow",
    "snowing",
    "regn",
    "regne",
    "regner",
    "paraply",
    "paraplyen",
    "snø",
    "snøe",
    "snør",
];
const WIND_WORDS: &[&str] = &["wind", "windy", "vind", "vindfullt", "blåser"];
/// Words that ask for the whole weather even when rain or wind is named too.
const GENERAL_WORDS: &[&str] =
    &["vær", "temperature", "degrees", "cold", "warm", "hot", "temperatur", "grader", "kaldt", "varmt"];
const WEEKDAYS: &[(jiff::civil::Weekday, &str, &str)] = {
    use jiff::civil::Weekday::*;
    &[
        (Monday, "monday", "mandag"),
        (Tuesday, "tuesday", "tirsdag"),
        (Wednesday, "wednesday", "onsdag"),
        (Thursday, "thursday", "torsdag"),
        (Friday, "friday", "fredag"),
        (Saturday, "saturday", "lørdag"),
        (Sunday, "sunday", "søndag"),
    ]
};

/// The day a weather question asks about, from its normalised words; today when it names none.
fn day_of(words: &[String]) -> Day {
    let has = |w: &str| words.iter().any(|x| x.trim_end_matches("'s") == w);
    let pair = |a: &str, b: &[&str]| {
        words.windows(2).any(|p| p[0].trim_end_matches("'s") == a && b.contains(&p[1].trim_end_matches("'s")))
    };
    if pair("after", &["tomorrow"]) || has("overmorgen") || has("iovermorgen") {
        return Day::AfterTomorrow;
    }
    let next = |day: &str| pair("next", &[day]) || pair("neste", &[day]);
    if let Some((day, en, no)) = WEEKDAYS.iter().find(|(_, en, no)| has(en) || has(no)) {
        return if next(en) || next(no) { Day::NextWeekday(*day) } else { Day::Weekday(*day) };
    }
    let weekend = ["weekend", "helg", "helgen", "helga"];
    if weekend.iter().any(|w| has(w)) {
        return if weekend.iter().any(|w| next(w)) { Day::NextWeekend } else { Day::Weekend };
    }
    // Whisper sometimes writes "i morgen" (tomorrow) as "i morgenen".
    if has("tomorrow") || has("imorgen") || pair("i", &["morgen", "morgenen"]) {
        return Day::Tomorrow;
    }
    if ["tonight", "evening", "night", "ikveld", "inatt", "kveld", "kvelden", "natt", "natten"].iter().any(|w| has(w)) {
        return Day::Tonight;
    }
    Day::Today
}

/// A day as the language model's weather tool names it ("tomorrow", "saturday", "weekend").
pub fn day_named(text: &str) -> Day {
    day_of(&text.split_whitespace().map(normalize).collect::<Vec<_>>())
}

fn focus_of(words: &[String]) -> Focus {
    let any = |list: &[&str]| words.iter().any(|w| list.contains(&w.as_str()));
    let general = any(GENERAL_WORDS) || words.iter().any(|w| WEATHER_STEMS.iter().any(|s| w.contains(s)));
    if general {
        Focus::General
    } else if any(WIND_WORDS) {
        Focus::Wind
    } else if any(RAIN_WORDS) || wants_clothes(words) {
        Focus::Rain
    } else {
        Focus::General
    }
}

/// "Should I bring a jacket?": clothes only count with a verb, so "where is my jacket" is no weather.
fn wants_clothes(words: &[String]) -> bool {
    let any = |list: &[&str]| words.iter().any(|w| list.contains(&w.as_str()));
    any(CLOTHES) && any(CLOTHES_VERBS)
}

/// Where a weather question asks about: the name after the last place marker, without times;
/// `None` for the home.
fn place_of(words: &[String]) -> Option<String> {
    let time = |w: &str| TIME_WORDS.contains(&w.trim_end_matches("'s")) || w.starts_with(|c: char| c.is_ascii_digit());
    // "I" first or after these is the English pronoun, not the Norwegian "in": "I always do weather
    // today", "do I need an umbrella".
    let pronoun = |i: usize| {
        i == 0
            || ["do", "should", "will", "can", "shall", "would", "could", "must", "if"].contains(&words[i - 1].as_str())
    };
    let place = words
        .iter()
        .enumerate()
        .rev()
        .filter(|(i, w)| PLACE_MARKERS.contains(&w.as_str()) && !(*w == "i" && pronoun(*i)))
        .find_map(|(i, _)| {
            let name: Vec<&str> = words[i + 1..]
                .iter()
                .map(String::as_str)
                .filter(|w| !FILLER.contains(w))
                .take_while(|w| !STOP_WORDS.contains(w) && !time(w))
                .collect();
            (!name.is_empty()).then(|| name.join(" "))
        })?;
    (!HOME_WORDS.contains(&place.as_str())).then_some(place)
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

fn is_lights_question(words: &[String]) -> bool {
    let has = |w: &str| words.iter().any(|x| x == w);
    // "Did I leave the lights on" is a question too, not "turn the lights on".
    let question = [
        "which", "what", "are", "is", "any", "did", "have", "has", "do", "does", "if", "whether", "hvilke", "hvilket",
        "er", "står", "hva", "har", "glemte", "lot", "om",
    ];
    let asks = question.contains(&question_word(words));
    let about = words.iter().any(|w| LIGHT_WORDS.contains(&w.as_str()))
        || ["anything", "everything", "something", "noe", "alt"].iter().any(|w| has(w));
    // "Are the lights off?" asks too; switching them off would answer by acting.
    asks && about && (has("on") || has("på") || has("left") || has("off") || has("av"))
}

const LIGHT_WORDS: &[&str] =
    &["light", "lights", "lamp", "lamps", "lys", "lyset", "lysene", "lampe", "lampen", "lampene"];

/// Words for Hue's standard scenes, and the scene's own name. "Read" is left out: "the reading
/// lamp" is a light.
const MOODS: &[(&str, &str)] = &[
    ("relax", "relax"),
    ("relaxing", "relax"),
    ("relaxed", "relax"),
    ("unwind", "relax"),
    ("concentrate", "concentrate"),
    ("concentrating", "concentrate"),
    ("focus", "concentrate"),
    ("energize", "energize"),
    ("energise", "energize"),
    ("energizing", "energize"),
];

/// The scene a mood asks for: "put on relaxing lights", or on its own, "I want to relax". Without
/// a light word only short sentences, so "relax" in a longer question is no scene.
fn mood(words: &[String]) -> Option<&'static str> {
    let lights = words.iter().any(|w| LIGHT_WORDS.contains(&w.as_str()));
    // "Slappe av" ends in "av", which is "off" anywhere else.
    if words.windows(2).any(|p| ["slappe", "slapp"].contains(&p[0].as_str()) && p[1] == "av") {
        return Some("relax");
    }
    if !lights && words.len() > 6 {
        return None;
    }
    words.iter().find_map(|w| MOODS.iter().find(|(said, _)| said == w).map(|(_, scene)| *scene))
}

pub fn parse(text: &str) -> Intent {
    parse_with(text, true)
}

/// As the other skills take the words, leaving notes out: for "what did I tell you about the
/// timer" or "forget the milk" when no note is about them.
pub fn parse_without_notes(text: &str) -> Intent {
    parse_with(text, false)
}

fn parse_with(text: &str, notes: bool) -> Intent {
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

/// The request with known mishearings put right, as normalised words; unchanged when there are none.
fn corrected(text: &str) -> std::borrow::Cow<'_, str> {
    let words: Vec<String> = text.split_whitespace().map(normalize).filter(|w| !w.is_empty()).collect();
    let mut out: Vec<&str> = Vec::with_capacity(words.len());
    let (mut i, mut changed) = (0, false);
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
    let words: Vec<String> = text.split_whitespace().map(normalize).filter(|w| !w.is_empty()).collect();
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
            return Intent::Lights { on, target: text.to_owned() };
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

    fn weather(place: Option<&str>, day: Day) -> Intent {
        Intent::Weather { place: place.map(str::to_owned), day, focus: Focus::General }
    }

    fn rain(place: Option<&str>, day: Day) -> Intent {
        Intent::Weather { place: place.map(str::to_owned), day, focus: Focus::Rain }
    }

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
    }

    #[test]
    fn levels_without_a_percent_sign() {
        let level = |p: u8, t: &str| Intent::LightLevel { level: LightLevel::Percent(p), target: t.to_owned() };
        assert_eq!(parse("Set the living room lights to 50"), level(50, "the living room lights"));
        assert_eq!(parse("Set the brightness to 30"), level(30, "the"));
        assert_eq!(parse("Set the volume to 30"), Intent::Music(MusicCommand::VolumeTo(30)));
        assert!(matches!(parse("Sett lyset i stua på 50"), Intent::LightLevel { level: LightLevel::Percent(50), .. }));
        assert!(matches!(parse("Turn the lights to 10"), Intent::LightLevel { level: LightLevel::Percent(10), .. }));
        assert!(matches!(parse("Set the lights at 50"), Intent::LightLevel { level: LightLevel::Percent(50), .. }));
        assert!(matches!(parse("Turn on the lights at 70"), Intent::LightLevel { level: LightLevel::Percent(70), .. }));
        // Norwegian says a time with "klokka": after "på" a bare number is a level.
        assert!(matches!(parse("skru på lyset på 10"), Intent::LightLevel { level: LightLevel::Percent(10), .. }));
        assert!(matches!(
            parse("skru på lyset på 10 i stua"),
            Intent::LightLevel { level: LightLevel::Percent(10), .. }
        ));
        assert!(matches!(parse("skru av lyset på 10"), Intent::Lights { on: false, .. }));
        // A time of day cannot be scheduled yet: nothing switches now.
        for phrase in [
            "Turn the lights off at 11",
            "Turn on the lights at 7",
            "Turn on the lights at 7 pm",
            "turn on the lights at 7 o'clock",
            "turn off the lights at 11pm",
            "skru av lyset klokka 23",
            "skru på lyset klokka 7",
            "skru på lyset kl. 7",
            "skru på lyset på 7 i kveld",
            "skru av lyset på 11 i kveld",
            "skru på lyset på 7 i morgen",
        ] {
            assert_eq!(parse(phrase), Intent::Unknown, "{phrase}");
        }
        assert_eq!(
            parse("Skru på lyset på 2. etasje"),
            Intent::Lights { on: true, target: "Skru på lyset på 2. etasje".into() }
        );
        assert!(says_off(&["skru".into(), "av".into(), "lyset".into()]));
        assert!(!says_off(&["noen".into(), "av".into(), "lysene".into()]));
        assert_eq!(
            parse("Set a timer for 10 minutes"),
            Intent::Timer(TimerCommand::Start { seconds: Some(600), name: None })
        );
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
    fn up_and_down_follow_a_volume_word() {
        let music = |text: &str| match parse(text) {
            Intent::Music(command) => Some(command),
            _ => None,
        };
        assert_eq!(music("turn it up"), Some(MusicCommand::Louder));
        assert_eq!(music("turn up the volume"), Some(MusicCommand::Louder));
        assert_eq!(music("skru ned lyden"), Some(MusicCommand::Quieter));
        assert_eq!(music("release up it"), None);
    }

    #[test]
    fn english_weather_questions() {
        assert_eq!(parse("what's the weather in Oslo?"), weather(Some("oslo"), Day::Today));
        assert_eq!(parse("What is the weather like in New York today"), weather(Some("new york"), Day::Today));
        assert_eq!(parse("will it rain in Bergen tomorrow"), rain(Some("bergen"), Day::Tomorrow));
        assert_eq!(parse("what's the temperature for Trondheim"), weather(Some("trondheim"), Day::Today));
        assert_eq!(parse("what's the forecast for tomorrow"), weather(None, Day::Tomorrow));
        assert_eq!(parse("how's the weather"), weather(None, Day::Today));
    }

    #[test]
    fn norwegian_weather_questions() {
        assert_eq!(parse("Hvordan blir været i Oslo i morgen?"), weather(Some("oslo"), Day::Tomorrow));
        assert_eq!(parse("Hva er temperaturen i Tromsø?"), weather(Some("tromsø"), Day::Today));
        assert_eq!(parse("Blir det regn på Lillehammer i dag?"), rain(Some("lillehammer"), Day::Today));
        assert_eq!(parse("Hvordan er været?"), weather(None, Day::Today));
        assert_eq!(parse("Hva er trømperaturen i Tromsø?"), weather(Some("tromsø"), Day::Today));
        assert_eq!(parse("Hvordan blir været i morgen?"), weather(None, Day::Tomorrow));
        assert_eq!(parse("hvordan blir været i Bergen i morgenen?"), weather(Some("bergen"), Day::Tomorrow));
    }

    #[test]
    fn weather_days() {
        use jiff::civil::Weekday::*;
        assert_eq!(parse("what's the weather on Saturday"), weather(None, Day::Weekday(Saturday)));
        assert_eq!(parse("what's the weather in Bergen on Monday"), weather(Some("bergen"), Day::Weekday(Monday)));
        assert_eq!(parse("what's the weather this weekend"), weather(None, Day::Weekend));
        assert_eq!(parse("what's the forecast for the weekend"), weather(None, Day::Weekend));
        assert_eq!(parse("what's the weather tonight"), weather(None, Day::Tonight));
        assert_eq!(parse("what's the weather this evening"), weather(None, Day::Tonight));
        assert_eq!(parse("what's the weather the day after tomorrow"), weather(None, Day::AfterTomorrow));
        assert_eq!(parse("Hvordan blir været på lørdag?"), weather(None, Day::Weekday(Saturday)));
        assert_eq!(parse("Blir det regn i helgen?"), rain(None, Day::Weekend));
        assert_eq!(parse("Hvordan blir været i overmorgen?"), weather(None, Day::AfterTomorrow));
        assert_eq!(parse("Hvordan blir været i Oslo i kveld?"), weather(Some("oslo"), Day::Tonight));
        assert_eq!(parse("Hvordan blir været i Bergen i morgen kveld?"), weather(Some("bergen"), Day::Tomorrow));
        assert_eq!(day_named("tomorrow"), Day::Tomorrow);
        assert_eq!(day_named("day after tomorrow"), Day::AfterTomorrow);
        assert_eq!(day_named("Saturday"), Day::Weekday(Saturday));
        assert_eq!(day_named("weekend"), Day::Weekend);
        assert_eq!(parse("what's the weather next Monday"), weather(None, Day::NextWeekday(Monday)));
        assert_eq!(parse("what is the weather next weekend"), weather(None, Day::NextWeekend));
        assert_eq!(parse("hva blir været neste helg"), weather(None, Day::NextWeekend));
        assert_eq!(day_named("next Monday's"), Day::NextWeekday(Monday));
        assert_eq!(day_named("today"), Day::Today);
    }

    #[test]
    fn times_are_no_places() {
        assert_eq!(parse("what's the weather in the morning"), weather(None, Day::Today));
        assert_eq!(parse("will it rain in the next hour"), rain(None, Day::Today));
        assert_eq!(parse("what's the weather at 6"), weather(None, Day::Today));
        assert_eq!(parse("what's the weather at home"), weather(None, Day::Today));
        assert_eq!(parse("how's the weather here"), weather(None, Day::Today));
        assert_eq!(parse("will it rain later"), rain(None, Day::Today));
        assert_eq!(parse("will it rain in Oslo later"), rain(Some("oslo"), Day::Today));
        assert_eq!(parse("will it rain this afternoon"), rain(None, Day::Today));
        assert_eq!(parse("what's the weather in Bergen in the afternoon"), weather(Some("bergen"), Day::Today));
        assert_eq!(parse("what's the weather in Bergen at 6 pm"), weather(Some("bergen"), Day::Today));
        assert_eq!(parse("Hvordan er været hjemme?"), weather(None, Day::Today));
    }

    #[test]
    fn rain_and_wind_questions() {
        let wind = |place: Option<&str>| Intent::Weather {
            place: place.map(str::to_owned),
            day: Day::Today,
            focus: Focus::Wind,
        };
        assert_eq!(parse("do I need an umbrella"), rain(None, Day::Today));
        assert_eq!(parse("do I need an umbrella today?"), rain(None, Day::Today));
        assert_eq!(parse("should I bring a jacket"), rain(None, Day::Today));
        assert_eq!(parse("is it raining?"), rain(None, Day::Today));
        assert_eq!(parse("will it rain tonight"), rain(None, Day::Tonight));
        assert_eq!(parse("trenger jeg paraply?"), rain(None, Day::Today));
        assert_eq!(parse("how windy is it"), wind(None));
        assert_eq!(parse("how windy is it in Bergen"), wind(Some("bergen")));
        assert_eq!(parse("Blåser det mye i dag?"), wind(None));
        // The weather asked for as a whole stays the whole weather.
        assert_eq!(parse("what's the weather, is it windy?"), weather(None, Day::Today));
        assert_ne!(parse("where is my jacket"), rain(None, Day::Today));
    }

    /// Places Parakeet heard wrong stay places; the geocoder and the sound-alike check sort them out.
    #[test]
    fn misheard_places_stay_places() {
        assert_eq!(parse("How is the weather in us of tomorrow?"), weather(Some("us of"), Day::Tomorrow));
        assert_eq!(parse("How is the weather at all slow tomorrow?"), weather(Some("all slow"), Day::Tomorrow));
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
        assert_eq!(lights("Turn all the living room lights."), Some(false));
        assert_eq!(lights("turn all the lights on"), Some(true));
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
    fn music_requests() {
        use MusicCommand::*;
        let music = |text: &str| match parse(text) {
            Intent::Music(command) => Some(command),
            _ => None,
        };
        let song = |s: &str| Some(Song(s.into()));
        assert_eq!(music("Play my liked songs."), Some(Liked));
        assert_eq!(music("Play my light songs."), Some(Liked));
        assert_eq!(music("play my life's songs"), Some(Liked));
        assert_eq!(music("play Light My Love"), song("light my love"));
        assert_eq!(music("play my like playlist"), Some(Liked));
        assert_eq!(music("Can you play my favorite songs please"), Some(Liked));
        assert_eq!(music("play some music"), Some(Liked));
        assert_eq!(music("play a song"), Some(Liked));
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
        assert_eq!(music("stop the music"), Some(Stop));
        assert_eq!(music("turn off the music"), Some(Pause));
        assert_eq!(music("stopp musikken"), Some(Stop));
        assert_eq!(music("mute the music"), Some(Mute(true)));
        assert_eq!(music("mute"), Some(Mute(true)));
        assert_eq!(music("unmute the music"), Some(Mute(false)));
        assert_eq!(music("what's playing"), Some(NowPlaying));
        assert_eq!(music("What song is this?"), Some(NowPlaying));
        assert_eq!(music("who sings this"), Some(NowPlaying));
        assert_eq!(music("turn on shuffle"), Some(Shuffle(true)));
        assert_eq!(music("stop shuffling"), Some(Shuffle(false)));
        assert_eq!(music("repeat this song"), Some(Repeat(super::Repeat::One)));
        assert_eq!(music("turn on repeat"), Some(Repeat(super::Repeat::All)));
        assert_eq!(music("turn off repeat"), Some(Repeat(super::Repeat::Off)));
        assert_eq!(music("play the album Rumours"), Some(Album("rumours".into())));
        assert_eq!(music("play music by Ghost"), Some(Artist("ghost".into())));
        assert_eq!(music("play some songs by Fleetwood Mac"), Some(Artist("fleetwood mac".into())));
        assert_eq!(music("play the artist Kygo"), Some(Artist("kygo".into())));
        assert_eq!(music("continue the music"), Some(Resume));
        assert_eq!(music("next song"), Some(Next));
        assert_eq!(music("skip this one"), Some(Next));
        assert_eq!(music("neste sang"), Some(Next));
        assert_eq!(music("previous song"), Some(Previous));
        assert_eq!(music("turn it up"), Some(Louder));
        assert_eq!(music("louder"), Some(Louder));
        assert_eq!(music("turn the music down"), Some(Quieter));
        assert_eq!(music("skru ned musikken"), Some(Quieter));
        assert_eq!(music("increase the sound"), Some(Louder));
        assert_eq!(music("Jarvis increased the sound"), Some(Louder));
        assert_eq!(music("lower the volume"), Some(Quieter));
        assert_eq!(music("Increase the sound by 25%."), Some(VolumeBy(25)));
        assert_eq!(music("turn it down 10 percent"), Some(VolumeBy(-10)));
        assert_eq!(music("set the volume to 40%"), Some(VolumeTo(40)));
        assert_eq!(music("put the music at 30%."), Some(VolumeTo(30)));
        assert_eq!(music("volume 25"), Some(VolumeTo(25)));
        assert_eq!(music("skru opp lyden med 20 prosent"), Some(VolumeBy(20)));
        // Not music.
        assert_eq!(music("turn off the light in my living room"), None);
        assert_eq!(music("turn up the lights"), None);
        assert_eq!(music("stop"), None);
        assert_eq!(music("what's the weather tomorrow"), None);
    }

    #[test]
    fn time_questions() {
        assert_eq!(parse("What time is it?"), Intent::Time);
        assert_eq!(parse("Hey, what time is it?"), Intent::Time);
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
        // From the engine's log: these were not understood, and "lower" was the music.
        is("Increase the brightness.", Brighter, "the");
        is("Increase the brightness in the living room.", Brighter, "the in the living room.");
        is("lower the brightness", Dimmer, "the");
        is("lower the lights in the kitchen", Dimmer, "the lights in the kitchen");
        assert_eq!(parse("lower the volume"), Intent::Music(MusicCommand::Quieter));
        assert_eq!(parse("lower the music"), Intent::Music(MusicCommand::Quieter));
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
    }

    #[test]
    fn where_someone_is() {
        let at = |name: &str, zone: Option<&str>, arriving: bool| Intent::WhereIs {
            name: name.into(),
            zone: zone.map(str::to_owned),
            arriving,
        };
        // As the transcriber wrote it on the Voice PE; the name is matched by sound later.
        assert_eq!(parse("where is Jon?"), at("jon", None, false));
        assert_eq!(parse("where's Kari"), at("kari", None, false));
        assert_eq!(parse("where is Ingrid right now"), at("ingrid", None, false));
        assert_eq!(parse("hvor er Ingrid"), at("ingrid", None, false));
        assert_eq!(parse("is Ingrid at work"), at("ingrid", Some("work"), false));
        assert_eq!(parse("er Ola på jobb"), at("ola", Some("jobb"), false));
        assert_eq!(parse("is Ola at the fire station"), at("ola", Some("fire station"), false));
        assert_eq!(parse("when will Ingrid be home"), at("ingrid", None, true));
        assert_eq!(parse("when is Ola coming home?"), at("ola", None, true));
        assert_eq!(parse("når kommer Ingrid hjem"), at("ingrid", None, true));
        assert_eq!(parse("where is everyone"), Intent::WhosHome(None));
        assert_eq!(parse("is Ingrid at home"), Intent::WhosHome(Some("ingrid".into())));
        assert_eq!(parse("where's my phone"), Intent::House(HouseCommand::FindPhone(None)));
        for other in [
            "where is the remote",
            "where are my keys",
            "is the dinner at 6",
            "is dinner at seven",
            "is the dinner at seven",
            "is Kiwi at Storo open",
            "where do you live",
            "when is the next bus home",
        ] {
            assert!(!matches!(parse(other), Intent::WhereIs { .. }), "{other}");
        }
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
