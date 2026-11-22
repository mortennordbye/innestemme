//! Follow-on requests: "what about inside?", "and tomorrow?", "and the hallway" go on from the
//! request answered last. The rest is joined to that request's topic in a sentence the rules parse
//! as usual, so rooms, days and places are read the same way as in a whole request. After a
//! Norwegian opening the sentence is Norwegian, and English when the Norwegian one reads otherwise.

use super::weather::{day_of, place_of, HOME_WORDS, WEATHER_STEMS, WEATHER_WORDS, WEEKDAYS};
use super::{parse, Day, Focus, Intent, LightLevel, MusicCommand};
use crate::house::{HouseCommand, Indoors, Reading, HUMIDITY_WORDS, TEMPERATURE_WORDS};
use crate::lang::Lang;
use crate::timer::TimerCommand;

/// How a follow-on opens, longest first: "and what about" is not "and" with "what about" left.
const STARTS: &[&[&str]] = &[
    &["and", "what's", "it", "like"],
    &["and", "whats", "it", "like"],
    &["what", "is", "it", "like"],
    &["and", "what", "about"],
    &["and", "how", "about"],
    &["og", "hva", "med"],
    &["what's", "it", "like"],
    &["whats", "it", "like"],
    &["what", "about"],
    &["how", "about"],
    &["hva", "med"],
    &["and"],
    &["og"],
];
/// Openings that are Norwegian: what follows them is joined in Norwegian.
const NORWEGIAN_STARTS: &[&str] = &["og", "hva"];
/// Words after the topic that change nothing: "and the hallway too".
const TAIL: &[&str] = &["too", "also", "then", "please", "instead", "også", "da"];
/// "A bit more", "even more", "again": the same volume step once more.
const MORE: &[&str] =
    &["a", "bit", "little", "more", "again", "even", "some", "tad", "just", "please", "litt", "mer", "igjen", "enda"];
const MORE_KEYS: &[&str] = &["more", "again", "mer", "igjen"];
const LIGHT_WORDS: &[&str] = &["light", "lights", "lamp", "lamps", "lys", "lyset", "lysene", "lampe", "lampen"];
/// Words that say where a climate reading is, so the last request's room is not added.
const WHERE_WORDS: &[&str] = &["in", "inside", "indoors", "outside", "outdoors", "here", "i", "på", "inne", "ute"];
/// "And another", "and one more", "og en til": another timer, which these words do not name.
const ANOTHER: &[&str] =
    &["another", "one", "a", "an", "more", "new", "second", "en", "ett", "et", "til", "ny", "annen"];
/// A timer's length in words: "for 10 minutes", "for half an hour", "på ti minutter".
const DURATION_WORDS: &[&str] = &[
    "a",
    "an",
    "and",
    "of",
    "half",
    "quarter",
    "one",
    "two",
    "three",
    "four",
    "five",
    "six",
    "seven",
    "eight",
    "nine",
    "ten",
    "eleven",
    "twelve",
    "fifteen",
    "twenty",
    "thirty",
    "forty",
    "fifty",
    "sixty",
    "ninety",
    "second",
    "seconds",
    "sec",
    "secs",
    "minute",
    "minutes",
    "min",
    "mins",
    "hour",
    "hours",
    "en",
    "ett",
    "et",
    "to",
    "tre",
    "fire",
    "fem",
    "seks",
    "sju",
    "syv",
    "åtte",
    "ni",
    "ti",
    "elleve",
    "tolv",
    "femten",
    "tjue",
    "tretti",
    "førti",
    "femti",
    "seksti",
    "og",
    "halv",
    "halvtime",
    "halvannen",
    "kvarter",
    "sekund",
    "sekunder",
    "minutt",
    "minutter",
    "time",
    "timer",
];
/// Words that join a timer's name and length: "one for the pasta for 10 minutes".
const TIMER_WORDS: &[&str] = &["for", "på", "called", "named", "the", "timer", "kalt", "til"];
/// Norwegian rooms that take "i", not "på": "i stua", "på kjøkkenet".
const ROOMS_WITH_I: &[&str] = &["stua", "gangen", "spisestua"];

/// The request that `text` makes when it goes on from `last`, and its intent: `None` when it does
/// not open like a follow-on, or when the joined request lands in another skill. Meant for words the
/// rules do not take on their own.
pub fn follow_on(last: &Intent, text: &str) -> Option<(String, Intent)> {
    let words = crate::dialog::words(text);
    let start = opening(&words);
    let no = start.is_some_and(|start| NORWEGIAN_STARTS.contains(&start[0]));
    let rest = start.map(|start| &words[start.len()..]);
    if let Intent::Music(step @ (MusicCommand::Louder | MusicCommand::Quieter | MusicCommand::VolumeBy(_))) = last {
        // "A bit more" and "more please" go on without an opening; a bare "more" or "again" is too
        // often a word of the room's.
        let more_rest = match rest {
            Some(rest) => Some(rest),
            None if words.len() >= 2 => Some(words.as_slice()),
            None => None,
        };
        let more = more_rest.is_some_and(|rest| {
            !rest.is_empty()
                && rest.iter().all(|w| MORE.contains(&w.as_str()))
                && rest.iter().any(|w| MORE_KEYS.contains(&w.as_str()))
        });
        if more {
            let carrier = match step {
                MusicCommand::Louder => "turn it up".to_owned(),
                MusicCommand::Quieter => "turn it down".to_owned(),
                MusicCommand::VolumeBy(by) if *by > 0 => format!("turn it up by {by} percent"),
                MusicCommand::VolumeBy(by) => format!("turn it down by {} percent", by.unsigned_abs()),
                _ => return None,
            };
            let intent = parse(&carrier);
            return (intent == *last).then_some((carrier, intent));
        }
        // Not more: "and in the kitchen" may still turn it up there.
    }
    let mut rest = rest?.to_vec();
    while rest.last().is_some_and(|w| TAIL.contains(&w.as_str())) {
        rest.pop();
    }
    if rest.is_empty() {
        return None;
    }
    let said = rest.join(" ");
    let has = |list: &[&str]| rest.iter().any(|w| list.contains(&w.as_str()));
    let same_topic = |intent: &Intent| family(intent).is_some() && family(intent) == family(last) && intent != last;
    let carriers = match last {
        Intent::Weather { place, day, focus } => {
            // "What about inside?", "and in the bedroom?": the rooms' sensors, not the forecast.
            let indoors = in_lang(
                no,
                Some(format!("hva er temperaturen {said}")),
                Some(format!("what's the temperature {said}")),
            );
            let climate = |intent: &Intent| matches!(intent, Intent::House(HouseCommand::Climate(..)));
            if let Some(found) = first_kept(indoors, climate) {
                return Some(found);
            }
            in_lang(
                no,
                Some(weather(place.as_deref(), *day, *focus, &rest, Lang::Norwegian)),
                Some(weather(place.as_deref(), *day, *focus, &rest, Lang::English)),
            )
        }
        Intent::House(HouseCommand::Climate(indoors, reading)) => {
            // "Luftfuktigheten" is read by its stem, as the rules read it.
            let humidity = has(HUMIDITY_WORDS) || rest.iter().any(|w| w.starts_with("luftfukt"));
            if has(TEMPERATURE_WORDS) || humidity {
                let what = said.strip_prefix("the ").unwrap_or(&said);
                let (english, norwegian) = if has(WHERE_WORDS) {
                    (String::new(), String::new())
                } else {
                    (where_indoors(indoors, Lang::English), where_indoors(indoors, Lang::Norwegian))
                };
                in_lang(no, Some(format!("hva er {what}{norwegian}")), Some(format!("what's the {what}{english}")))
            } else {
                in_lang(
                    no,
                    Some(format!("hva er {} {said}", reading_words(*reading, Lang::Norwegian))),
                    Some(format!("what's the {} {said}", reading_words(*reading, Lang::English))),
                )
            }
        }
        // Only a room on its own: "and tomorrow" switches nothing, and "and not the hallway" or "and
        // the bedroom is a mess" are no request for its lights.
        Intent::Lights { on, .. } => {
            let room = bare_room(&rest)?;
            in_lang(
                no,
                norwegian_room(room).map(|r| format!("skru {} lyset {r}", if *on { "på" } else { "av" })),
                Some(format!("turn {} the lights in the {room}", if *on { "on" } else { "off" })),
            )
        }
        Intent::LightsStatus { .. } => {
            let room = bare_room(&rest)?;
            in_lang(
                no,
                norwegian_room(room).map(|r| format!("er lyset på {r}")),
                Some(format!("are the lights on in the {room}")),
            )
        }
        Intent::LightLevel { level, .. } => {
            let room = bare_room(&rest)?;
            let target = format!("the lights in the {room}");
            let english = match level {
                LightLevel::Percent(percent) => format!("set {target} to {percent} percent"),
                LightLevel::Brighter => format!("make {target} brighter"),
                LightLevel::Dimmer => format!("dim {target}"),
                LightLevel::Color(colour) => format!("make {target} {colour}"),
                LightLevel::Warm => format!("make {target} warmer"),
                LightLevel::Cool => format!("make {target} cooler"),
            };
            // The colours are English names, so a colour stays in English.
            let norwegian = norwegian_room(room).map(|r| format!("lyset {r}")).and_then(|target| match level {
                LightLevel::Percent(percent) => Some(format!("sett {target} til {percent} prosent")),
                LightLevel::Brighter => Some(format!("gjør {target} lysere")),
                LightLevel::Dimmer => Some(format!("demp {target}")),
                LightLevel::Color(_) => None,
                LightLevel::Warm => Some(format!("gjør {target} varmere")),
                LightLevel::Cool => Some(format!("gjør {target} kaldere")),
            });
            in_lang(no, norwegian, Some(english))
        }
        Intent::Power(_) => in_lang(
            no,
            Some(format!("hva er strømprisen {said}")),
            Some(format!("what's the electricity price {said}")),
        ),
        Intent::Timer(TimerCommand::Start { .. }) => return timer(&rest, no),
        Intent::Transit(query) => {
            let lang = if no { Lang::Norwegian } else { Lang::English };
            vec![crate::transit::follow_on(query, &rest, lang)]
        }
        Intent::Music(command) => return music_in_room(command, &rest, no),
        Intent::Scene(request) => return scene_in_room(request, &rest, no),
        _ => return None,
    };
    // The same request again adds nothing: "the kitchen" after the power price is no price.
    first_kept(carriers, same_topic)
}

/// Whether `parsed`, the rules' own reading of the words, may still go on from `last`: words no
/// skill takes, and a reading with no place after one in a room. "And the humidity?" is this
/// room's humidity on its own, but the kitchen's after the kitchen's temperature.
pub fn open_to_follow_on(last: &Intent, parsed: &Intent) -> bool {
    let climate = matches!(last, Intent::House(HouseCommand::Climate(..)));
    *parsed == Intent::Unknown || (climate && matches!(parsed, Intent::House(HouseCommand::Climate(Indoors::Here, _))))
}

/// A follow-on's opening; `None` when the words have none.
fn opening(words: &[String]) -> Option<&'static [&'static str]> {
    STARTS.iter().copied().find(|start| words.len() >= start.len() && start.iter().zip(words).all(|(s, w)| s == w))
}

/// The sentences to try in turn: the Norwegian one first after a Norwegian opening, else only the
/// English one.
fn in_lang(no: bool, norwegian: Option<String>, english: Option<String>) -> Vec<Option<String>> {
    if no {
        vec![norwegian, english]
    } else {
        vec![english]
    }
}

/// The first of `carriers` whose reading `keep` takes, with that reading.
fn first_kept(carriers: Vec<Option<String>>, keep: impl Fn(&Intent) -> bool) -> Option<(String, Intent)> {
    carriers.into_iter().flatten().find_map(|carrier| {
        let intent = parse(&carrier);
        keep(&intent).then_some((carrier, intent))
    })
}

/// Requests a follow-on may move between: the forecast and the rooms' readings are one topic, and
/// switching, asking about and dimming the lights another.
fn family(intent: &Intent) -> Option<&'static str> {
    match intent {
        Intent::Weather { .. } | Intent::House(HouseCommand::Climate(..)) => Some("weather"),
        Intent::Lights { .. } | Intent::LightsStatus { .. } | Intent::LightLevel { .. } => Some("lights"),
        Intent::Music(MusicCommand::Louder | MusicCommand::Quieter | MusicCommand::VolumeBy(_)) => Some("volume"),
        Intent::Power(_) => Some("power"),
        Intent::Transit(_) => Some("transit"),
        _ => None,
    }
}

/// Another timer: the rest is its length, and maybe its name, and nothing more. "And another for
/// 5 minutes", "and one for the pasta for 10 minutes", "og en til på ti minutter". The same timer
/// again is meant here, so it is no reason to drop it.
fn timer(rest: &[String], no: bool) -> Option<(String, Intent)> {
    let rest = &rest[rest.iter().take_while(|w| ANOTHER.contains(&w.as_str())).count()..];
    if rest.is_empty() {
        return None;
    }
    let said = rest.join(" ");
    // "A pasta timer for 10 minutes" says "timer" itself.
    let forms = |verb: &str| {
        let (inserted, as_said) = (Some(format!("{verb} timer {said}")), Some(format!("{verb} {said}")));
        if rest.iter().any(|w| w == "timer") {
            [as_said, inserted]
        } else {
            [inserted, as_said]
        }
    };
    let carriers = if no { [forms("sett en"), forms("set a")].concat() } else { forms("set a").to_vec() };
    first_kept(carriers, |intent| {
        let Intent::Timer(TimerCommand::Start { seconds: Some(_), name }) = intent else { return false };
        let name: Vec<&str> = name.as_deref().map(|name| name.split_whitespace().collect()).unwrap_or_default();
        rest.iter().all(|w| {
            let w = w.as_str();
            w.chars().all(|c| c.is_ascii_digit())
                || DURATION_WORDS.contains(&w)
                || TIMER_WORDS.contains(&w)
                || name.contains(&w)
        })
    })
}

/// The same music command in another room: "and in the bedroom" after "play jazz in the kitchen".
/// Only a room on its own, as for the lights.
fn music_in_room(command: &MusicCommand, rest: &[String], no: bool) -> Option<(String, Intent)> {
    let (was, base) = match command {
        MusicCommand::InRoom(room, inner) => (Some(room.as_str()), inner.as_ref()),
        MusicCommand::InRooms(rooms, inner) => (rooms.first().map(String::as_str), inner.as_ref()),
        command => (None, command),
    };
    let room = bare_room(rest)?;
    if was.is_some_and(|was| crate::ha::is_room(was, room)) {
        return None;
    }
    let (english, norwegian) = music_words(base)?;
    let carriers = in_lang(
        no,
        norwegian.zip(norwegian_room(room)).map(|(command, room)| format!("{command} {room}")),
        Some(format!("{english} in the {room}")),
    );
    first_kept(carriers, |intent| {
        matches!(intent, Intent::Music(MusicCommand::InRoom(heard, inner))
            if **inner == *base && crate::ha::is_room(heard, room))
    })
}

/// How the rules read `command` back, in English and in Norwegian; `None` for a command that has
/// no room of its own (moving or queueing music) or a step that is not said twice the same way.
fn music_words(command: &MusicCommand) -> Option<(String, Option<String>)> {
    let fixed = |english: &str, norwegian: &str| (english.to_owned(), Some(norwegian.to_owned()));
    Some(match command {
        MusicCommand::Liked => fixed("play my liked songs", "spill likte sanger"),
        MusicCommand::Playlist(list) => {
            (format!("play the playlist {list}"), Some(format!("spill spillelisten {list}")))
        }
        MusicCommand::Song(song) => (format!("play {song}"), Some(format!("spill {song}"))),
        MusicCommand::Artist(artist) => (format!("play music by {artist}"), Some(format!("spill musikk av {artist}"))),
        MusicCommand::Album(album) => (format!("play the album {album}"), Some(format!("spill albumet {album}"))),
        MusicCommand::Resume => fixed("resume the music", "fortsett musikken"),
        MusicCommand::Pause => fixed("pause the music", "pause musikken"),
        MusicCommand::Stop => fixed("stop the music", "stopp musikken"),
        MusicCommand::Next => fixed("skip to the next song", "hopp til neste sang"),
        MusicCommand::NowPlaying => fixed("what's playing", "hva spilles"),
        MusicCommand::Mute(true) => fixed("mute the music", "demp musikken"),
        MusicCommand::Mute(false) => ("unmute the music".into(), None),
        MusicCommand::Louder => fixed("turn the music up", "skru opp musikken"),
        MusicCommand::Quieter => fixed("turn the music down", "skru ned musikken"),
        MusicCommand::VolumeTo(to) => {
            (format!("set the volume to {to} percent"), Some(format!("sett volumet til {to} prosent")))
        }
        _ => return None,
    })
}

/// The same scene in another room: "and the bedroom" after "relaxing light in the living room".
/// Only a room on its own, as for the lights.
fn scene_in_room(request: &str, rest: &[String], no: bool) -> Option<(String, Intent)> {
    let (name, was) = crate::ha::scene_named(request)?;
    let room = bare_room(rest)?;
    if was == Some(room) {
        return None;
    }
    let carriers = in_lang(
        no,
        norwegian_room(room).map(|r| format!("aktiver {name} {r}")),
        Some(format!("activate {name} in the {room}")),
    );
    first_kept(
        carriers,
        |intent| matches!(intent, Intent::Scene(text) if crate::ha::scene_named(text) == Some((name.clone(), Some(room)))),
    )
}

/// The forecast with what the follow-on names, and the rest from the last one: "how about in
/// Bergen?" keeps the day, "and tomorrow?" the place, and either keeps rain or wind.
fn weather(place: Option<&str>, day: Day, focus: Focus, rest: &[String], lang: Lang) -> String {
    let no = lang == Lang::Norwegian;
    let has = |list: &[&str]| rest.iter().any(|w| list.contains(&w.as_str()));
    let weather_word =
        rest.iter().any(|w| WEATHER_WORDS.contains(&w.as_str()) || WEATHER_STEMS.iter().any(|s| w.contains(s)));
    let mut parts: Vec<String> = Vec::new();
    if !weather_word {
        parts.push(
            match (focus, no) {
                (Focus::General, false) => "what's the weather",
                (Focus::Rain, false) => "will it rain",
                (Focus::Wind, false) => "how windy is it",
                (Focus::General, true) => "hva er været",
                (Focus::Rain, true) => "kommer det til å regne",
                (Focus::Wind, true) => "blåser det",
            }
            .into(),
        );
    }
    parts.push(rest.join(" "));
    let today = has(&["today", "now", "idag", "nå"]) || rest.windows(2).any(|p| p[0] == "i" && p[1] == "dag");
    if day_of(rest) == Day::Today && !today {
        parts.extend(day_words(day, lang));
    }
    if let (None, false, Some(place)) = (place_of(rest), has(HOME_WORDS), place) {
        parts.push(format!("{} {place}", if no { "i" } else { "in" }));
    }
    parts.join(" ")
}

/// How the rules read `day` back.
fn day_words(day: Day, lang: Lang) -> Option<String> {
    let no = lang == Lang::Norwegian;
    let name = |weekday| {
        WEEKDAYS
            .iter()
            .find(|(d, _, _)| *d == weekday)
            .map(|(_, english, norwegian)| if no { *norwegian } else { *english })
    };
    Some(match (day, no) {
        (Day::Today, _) => return None,
        (Day::Tonight, false) => "tonight".into(),
        (Day::Tomorrow, false) => "tomorrow".into(),
        (Day::AfterTomorrow, false) => "the day after tomorrow".into(),
        (Day::Weekday(weekday), false) => format!("on {}", name(weekday)?),
        (Day::NextWeekday(weekday), false) => format!("next {}", name(weekday)?),
        (Day::Weekend, false) => "this weekend".into(),
        (Day::NextWeekend, false) => "next weekend".into(),
        (Day::Tonight, true) => "i kveld".into(),
        (Day::Tomorrow, true) => "i morgen".into(),
        (Day::AfterTomorrow, true) => "i overmorgen".into(),
        (Day::Weekday(weekday), true) => format!("på {}", name(weekday)?),
        (Day::NextWeekday(weekday), true) => format!("neste {}", name(weekday)?),
        (Day::Weekend, true) => "i helgen".into(),
        (Day::NextWeekend, true) => "neste helg".into(),
    })
}

fn where_indoors(indoors: &Indoors, lang: Lang) -> String {
    match (indoors, lang) {
        (Indoors::Room(room), Lang::English) => format!(" in the {room}"),
        (Indoors::Room(room), Lang::Norwegian) => {
            norwegian_room(room).map_or_else(|| format!(" i {room}"), |room| format!(" {room}"))
        }
        (Indoors::Everywhere, Lang::English) => " inside".into(),
        (Indoors::Everywhere, Lang::Norwegian) => " inne".into(),
        (Indoors::Here, Lang::English) => " in here".into(),
        (Indoors::Here, Lang::Norwegian) => " her inne".into(),
    }
}

fn reading_words(reading: Reading, lang: Lang) -> &'static str {
    match (reading, lang) {
        (Reading::Temperature, Lang::English) => "temperature",
        (Reading::Humidity, Lang::English) => "humidity",
        (Reading::Both, Lang::English) => "temperature and humidity",
        (Reading::Temperature, Lang::Norwegian) => "temperaturen",
        (Reading::Humidity, Lang::Norwegian) => "luftfuktigheten",
        (Reading::Both, Lang::Norwegian) => "temperaturen og luftfuktigheten",
    }
}

/// The room by its English name, in Norwegian with its preposition: "på kjøkkenet".
fn norwegian_room(english: &str) -> Option<String> {
    let room = crate::ha::room_name(english, Lang::Norwegian)?;
    Some(format!("{} {room}", if ROOMS_WITH_I.contains(&room) { "i" } else { "på" }))
}

/// The room that `rest` is and nothing more: "the hallway", "in the bedroom", "the kitchen lights",
/// "the hallway as well"; its English name.
fn bare_room(rest: &[String]) -> Option<&'static str> {
    let mut words: Vec<String> = rest.to_vec();
    if words.ends_with(&["as".to_owned(), "well".to_owned()]) {
        words.truncate(words.len() - 2);
    }
    while words.last().is_some_and(|w| LIGHT_WORDS.contains(&w.as_str()) || TAIL.contains(&w.as_str())) {
        words.pop();
    }
    let lead = words.iter().take(2).take_while(|w| ["in", "i", "på", "the"].contains(&w.as_str())).count();
    let words = &words[lead..];
    let phrase = words.join(" ");
    crate::ha::room_named(words).filter(|room| !phrase.is_empty() && crate::ha::is_room(&phrase, room))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::power::PowerQuery;

    fn on(last: &str, text: &str) -> Option<Intent> {
        follow_on(&parse(last), text).map(|(_, intent)| intent)
    }

    fn climate(indoors: Indoors, reading: Reading) -> Option<Intent> {
        Some(Intent::House(HouseCommand::Climate(indoors, reading)))
    }

    #[test]
    fn the_weather_goes_on_inside_and_to_other_days_and_places() {
        assert_eq!(parse("what's the temperature inside"), climate(Indoors::Everywhere, Reading::Temperature).unwrap());
        assert_eq!(on("what's the weather", "What about inside?"), climate(Indoors::Everywhere, Reading::Temperature));
        assert_eq!(
            on("will it rain tomorrow", "what about inside?"),
            climate(Indoors::Everywhere, Reading::Temperature)
        );
        assert_eq!(
            on("what's the weather", "What about in the living room?"),
            climate(Indoors::Room("living room".into()), Reading::Temperature)
        );
        let weather = |place: Option<&str>, day, focus| Intent::Weather { place: place.map(Into::into), day, focus };
        assert_eq!(
            on("what's the weather in Bergen", "And tomorrow?"),
            Some(weather(Some("bergen"), Day::Tomorrow, Focus::General))
        );
        assert_eq!(on("will it rain today", "and tomorrow?"), Some(weather(None, Day::Tomorrow, Focus::Rain)));
        assert_eq!(
            on("what's the weather tomorrow", "How about in Bergen?"),
            Some(weather(Some("bergen"), Day::Tomorrow, Focus::General))
        );
        assert_eq!(
            on("what's the weather on saturday", "what's it like in Bergen?"),
            Some(weather(Some("bergen"), Day::Weekday(jiff::civil::Weekday::Saturday), Focus::General))
        );
        assert_eq!(on("what's the weather", "hva med i morgen?"), Some(weather(None, Day::Tomorrow, Focus::General)));
    }

    #[test]
    fn the_rooms_readings_go_on_to_another_reading_room_or_outside() {
        let kitchen = "what's the temperature in the kitchen";
        assert_eq!(on(kitchen, "And the humidity?"), climate(Indoors::Room("kitchen".into()), Reading::Humidity));
        assert_eq!(
            on(kitchen, "What about the bedroom?"),
            climate(Indoors::Room("bedroom".into()), Reading::Temperature)
        );
        let now = Intent::Weather { place: None, day: Day::Today, focus: Focus::General };
        assert_eq!(on(kitchen, "What about outside?"), Some(now));
    }

    #[test]
    fn only_words_no_skill_takes_go_on() {
        let kitchen = parse("what's the temperature in the kitchen");
        assert!(open_to_follow_on(&kitchen, &parse("And the humidity?")));
        assert!(open_to_follow_on(&kitchen, &parse("And tomorrow?")));
        assert!(!open_to_follow_on(&kitchen, &parse("what's the humidity in the bedroom")));
        assert!(!open_to_follow_on(&parse("what's the weather"), &parse("And the humidity?")));
        assert!(!open_to_follow_on(&kitchen, &parse("turn off the lights")));
    }

    #[test]
    fn the_lights_go_on_to_another_room() {
        let Some(Intent::Lights { on: false, target }) = on("turn off the kitchen lights", "and the hallway") else {
            panic!("not the lights");
        };
        assert!(target.contains("hallway"), "{target}");
        let Some(Intent::Lights { on: true, target }) = on("turn on the kitchen", "and the bedroom too") else {
            panic!("not the lights");
        };
        assert!(target.contains("bedroom"), "{target}");
        let Some(Intent::LightsStatus { target }) = on("are the lights on in the kitchen", "what about the bedroom?")
        else {
            panic!("not the lights' status");
        };
        assert!(target.contains("bedroom"), "{target}");
        let Some(Intent::LightLevel { level: LightLevel::Percent(30), target }) =
            on("set the kitchen to 30 percent", "and the living room")
        else {
            panic!("not the light level");
        };
        assert!(target.contains("living room"), "{target}");
    }

    #[test]
    fn the_volume_steps_again() {
        for more in ["a bit more", "more please", "and more", "and again", "even more", "litt mer"] {
            assert_eq!(on("turn it up", more), Some(Intent::Music(MusicCommand::Louder)), "{more}");
            assert_eq!(on("turn the music down", more), Some(Intent::Music(MusicCommand::Quieter)), "{more}");
        }
        assert_eq!(on("turn it up by 10 percent", "a bit more"), Some(Intent::Music(MusicCommand::VolumeBy(10))));
        assert_eq!(on("turn it up", "a bit"), None);
        // Alone, one word is as likely the room's.
        assert_eq!(on("turn it up", "more"), None);
        assert_eq!(on("turn it up", "again"), None);
        assert_eq!(on("turn it up", "more cowbell"), None);
    }

    #[test]
    fn the_power_price_goes_on_to_tomorrow() {
        assert_eq!(on("what's the electricity price", "And tomorrow?"), Some(Intent::Power(PowerQuery::Tomorrow)));
        assert_eq!(on("what's the electricity price", "and the kitchen"), None);
    }

    #[test]
    fn only_follow_ons_in_the_same_skill() {
        // Another skill, nothing to switch, or no follow-on opening.
        assert_eq!(on("turn off the kitchen lights", "and tomorrow?"), None);
        assert_eq!(on("what's the electricity price", "what about the hallway lights?"), None);
        assert_eq!(on("what's the weather", "the kitchen"), None);
        assert_eq!(on("what's the weather", "tomorrow"), None);
        assert_eq!(on("turn off the kitchen lights", "the hallway"), None);
        // More than a room after the opening.
        for not in [
            "and not the hallway",
            "and leave the hallway",
            "and the hallway stays on",
            "and the bedroom is a mess",
            "what about the kitchen table",
            "and then the hallway",
        ] {
            assert_eq!(on("turn off the kitchen lights", not), None, "{not}");
        }
        assert_eq!(on("what's the weather", "and"), None);
        // Requests with no topic to go on from.
        assert_eq!(on("tell me a joke", "and another one?"), None);
        assert_eq!(on("tell me a joke", "what about tomorrow?"), None);
        assert_eq!(on("what time is it", "and in Bergen?"), None);
        assert_eq!(on("purple monkey dishwasher", "and tomorrow?"), None);
    }

    #[test]
    fn another_timer_goes_on_from_a_timer() {
        let start = |seconds, name: Option<&str>| {
            Some(Intent::Timer(TimerCommand::Start { seconds: Some(seconds), name: name.map(Into::into) }))
        };
        let five = "set a timer for 5 minutes";
        // The same length again is another timer, not nothing new.
        assert_eq!(on(five, "and another for 5 minutes"), start(300, None));
        assert_eq!(on(five, "And one for 10 minutes."), start(600, None));
        assert_eq!(on(five, "and another one for 2 minutes"), start(120, None));
        assert_eq!(on(five, "and one for the pasta for 10 minutes"), start(600, Some("pasta")));
        assert_eq!(on("set a pasta timer for 10 minutes", "and one for 5 minutes"), start(300, None));
        assert_eq!(on("sett en timer på 5 minutter", "og en til på 10 minutter"), start(600, None));
        // No length, or more than a name and a length.
        for not in [
            "and another",
            "and one more",
            "and another for the oven",
            "and one for 5 minutes is enough",
            "and another for 5 minutes to check the oven",
            "and one for 5 minutes and turn off the lights",
            "and the kitchen",
            "and tomorrow",
            "another for 5 minutes",
        ] {
            assert_eq!(on(five, not), None, "{not}");
        }
        assert_eq!(on("what's the weather", "and another for 5 minutes"), None);
        assert_eq!(on("cancel the timer", "and another for 5 minutes"), None);
    }

    #[test]
    fn departures_go_on_to_another_mode_stop_or_line() {
        use crate::transit::{Mode, TransitQuery};
        let q = |mode, line: Option<&str>, to: Option<&str>, from: Option<&str>| {
            Some(Intent::Transit(TransitQuery {
                mode,
                line: line.map(Into::into),
                destination: to.map(Into::into),
                from: from.map(Into::into),
            }))
        };
        let bus = "when's the next bus";
        assert_eq!(on(bus, "and the tram?"), q(Mode::Tram, None, None, None));
        assert_eq!(on(bus, "what about to Fjellby?"), q(Mode::Bus, None, Some("fjellby"), None));
        assert_eq!(on(bus, "and the tram to Fjellby"), q(Mode::Tram, None, Some("fjellby"), None));
        let to_fjellby = "when's the next bus to Fjellby";
        assert_eq!(on(to_fjellby, "and the train?"), q(Mode::Rail, None, Some("fjellby"), None));
        assert_eq!(on(to_fjellby, "and from Dalby?"), q(Mode::Bus, None, Some("fjellby"), Some("dalby")));
        assert_eq!(on("when is the next 31 bus", "and to Fjellby"), q(Mode::Bus, Some("31"), Some("fjellby"), None));
        assert_eq!(on("when is the next 31 bus", "and line 20"), q(Mode::Any, Some("20"), None, None));
        assert_eq!(on("når går neste buss", "og trikken til Fjellby"), q(Mode::Tram, None, Some("fjellby"), None));
        // The question asks for the next departures only, and the rest must be a mode, line or stop.
        for not in [
            "and the next one?",
            "what about after that?",
            "and the bus",
            "and the bus is late",
            "and to be honest",
            "and to the big old red barn",
            "and the kitchen",
            "and tomorrow",
        ] {
            assert_eq!(on(bus, not), None, "{not}");
        }
        assert_eq!(on("what's the weather", "and the tram?"), None);
    }

    #[test]
    fn music_goes_on_to_another_room() {
        use MusicCommand::*;
        let room = |room: &str, command| Some(Intent::Music(InRoom(room.into(), Box::new(command))));
        assert_eq!(on("play jazz in the kitchen", "and in the bedroom"), room("bedroom", Song("jazz".into())));
        assert_eq!(on("play jazz", "what about in the bedroom?"), room("bedroom", Song("jazz".into())));
        assert_eq!(on("pause the music in the kitchen", "and the living room"), room("living room", Pause));
        assert_eq!(on("play my liked songs", "and the bedroom too"), room("bedroom", Liked));
        assert_eq!(on("turn it up", "and in the bedroom"), room("bedroom", Louder));
        assert_eq!(on("spill musikk på kjøkkenet", "og på soverommet"), room("soverommet", Liked));
        // The same room, more than a room, or music with no room of its own.
        assert_eq!(on("play jazz in the kitchen", "and the kitchen"), None);
        for not in
            ["and the bedroom is a mess", "and not the bedroom", "and tomorrow", "and the kitchen table", "the bedroom"]
        {
            assert_eq!(on("play jazz in the kitchen", not), None, "{not}");
        }
        assert_eq!(on("move the music to the bedroom", "and the kitchen"), None);
        assert_eq!(on("turn it up", "the bedroom"), None);
    }

    #[test]
    fn a_scene_goes_on_to_another_room() {
        let scene = |text: &str| Some(Intent::Scene(text.into()));
        assert_eq!(on("relaxing light in the living room", "and the bedroom"), scene("activate relax in the bedroom"));
        assert_eq!(
            on("set the kitchen to reading", "what about the living room?"),
            scene("activate reading in the living room")
        );
        assert_eq!(on("activate the relax scene", "and in the kitchen"), scene("activate relax in the kitchen"));
        assert_eq!(on("sett stua til relax", "og soverommet"), scene("aktiver relax på soverommet"));
        assert_eq!(on("relaxing light in the living room", "and the living room"), None);
        for not in ["and the bedroom is a mess", "and not the bedroom", "and tomorrow", "the bedroom"] {
            assert_eq!(on("relaxing light in the living room", not), None, "{not}");
        }
    }

    #[test]
    fn a_norwegian_follow_on_is_joined_in_norwegian() {
        let carrier = |last: &str, text: &str| follow_on(&parse(last), text).map(|(carrier, _)| carrier);
        assert_eq!(carrier("hva er været", "hva med inne?").as_deref(), Some("hva er temperaturen inne"));
        assert_eq!(on("hva er været", "hva med inne?"), climate(Indoors::Everywhere, Reading::Temperature));
        assert_eq!(carrier("hva er været", "og i morgen?").as_deref(), Some("hva er været i morgen"));
        assert_eq!(
            carrier("hva er været i morgen", "hva med i Bergen?").as_deref(),
            Some("hva er været i bergen i morgen")
        );
        assert_eq!(
            on("hva er været i morgen", "hva med i Bergen?"),
            Some(Intent::Weather { place: Some("bergen".into()), day: Day::Tomorrow, focus: Focus::General })
        );
        let kitchen = "hva er temperaturen på kjøkkenet";
        assert_eq!(carrier(kitchen, "og luftfuktigheten?").as_deref(), Some("hva er luftfuktigheten på kjøkkenet"));
        assert_eq!(on(kitchen, "og luftfuktigheten?"), climate(Indoors::Room("kitchen".into()), Reading::Humidity));
        assert_eq!(carrier("skru av lyset på kjøkkenet", "og stua").as_deref(), Some("skru av lyset i stua"));
        assert_eq!(carrier("hva er strømprisen", "og i morgen?").as_deref(), Some("hva er strømprisen i morgen"));
        assert_eq!(
            carrier("sett en timer på 5 minutter", "og en til på 10 minutter").as_deref(),
            Some("sett en timer på 10 minutter")
        );
        assert_eq!(carrier("når går neste buss", "og trikken?").as_deref(), Some("når går neste trikk"));
        // A colour has no Norwegian sentence: English it is.
        assert_eq!(
            carrier("make the kitchen red", "og stua").as_deref(),
            Some("make the lights in the living room red")
        );
        // English stays English.
        assert_eq!(
            carrier("what's the weather", "what about inside?").as_deref(),
            Some("what's the temperature inside")
        );
        assert_eq!(
            carrier("turn off the kitchen lights", "and the hallway").as_deref(),
            Some("turn off the lights in the hallway")
        );
    }

    #[test]
    fn every_room_has_a_norwegian_name_the_rules_read() {
        for room in ["living room", "kitchen", "bedroom", "office", "hallway", "bathroom", "dining room", "kids room"] {
            let norwegian = norwegian_room(room).unwrap();
            assert_eq!(crate::ha::room_named(&crate::dialog::words(&norwegian)), Some(room), "{norwegian}");
        }
    }
}
