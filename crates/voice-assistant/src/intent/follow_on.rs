//! Follow-on requests: "what about inside?", "and tomorrow?", "and the hallway" go on from the
//! request answered last. The rest is joined to that request's topic in a sentence the rules parse
//! as usual, so rooms, days and places are read the same way as in a whole request.

use super::weather::{day_of, place_of, HOME_WORDS, WEATHER_STEMS, WEATHER_WORDS, WEEKDAYS};
use super::{parse, Day, Focus, Intent, LightLevel, MusicCommand};
use crate::house::{HouseCommand, Indoors, Reading, HUMIDITY_WORDS, TEMPERATURE_WORDS};

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
/// Words after the topic that change nothing: "and the hallway too".
const TAIL: &[&str] = &["too", "also", "then", "please", "instead", "også", "da"];
/// "A bit more", "even more", "again": the same volume step once more.
const MORE: &[&str] =
    &["a", "bit", "little", "more", "again", "even", "some", "tad", "just", "please", "litt", "mer", "igjen", "enda"];
const MORE_KEYS: &[&str] = &["more", "again", "mer", "igjen"];
const LIGHT_WORDS: &[&str] = &["light", "lights", "lamp", "lamps", "lys", "lyset", "lysene", "lampe", "lampen"];
/// Words that say where a climate reading is, so the last request's room is not added.
const WHERE_WORDS: &[&str] = &["in", "inside", "indoors", "outside", "outdoors", "here", "i", "på", "inne", "ute"];

/// The request that `text` makes when it goes on from `last`, and its intent: `None` when it does
/// not open like a follow-on, or when the joined request lands in another skill. Meant for words the
/// rules do not take on their own.
pub fn follow_on(last: &Intent, text: &str) -> Option<(String, Intent)> {
    let words = crate::dialog::words(text);
    let rest = after_start(&words);
    if let Intent::Music(step @ (MusicCommand::Louder | MusicCommand::Quieter | MusicCommand::VolumeBy(_))) = last {
        // "A bit more" and "more please" go on without an opening; a bare "more" or "again" is too
        // often a word of the room's.
        let rest = match rest {
            Some(rest) => rest,
            None if words.len() >= 2 => &words,
            None => return None,
        };
        let more = !rest.is_empty()
            && rest.iter().all(|w| MORE.contains(&w.as_str()))
            && rest.iter().any(|w| MORE_KEYS.contains(&w.as_str()));
        let carrier = match step {
            MusicCommand::Louder => "turn it up".to_owned(),
            MusicCommand::Quieter => "turn it down".to_owned(),
            MusicCommand::VolumeBy(by) if *by > 0 => format!("turn it up by {by} percent"),
            MusicCommand::VolumeBy(by) => format!("turn it down by {} percent", by.unsigned_abs()),
            _ => return None,
        };
        let intent = parse(&carrier);
        return (more && intent == *last).then_some((carrier, intent));
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
    let carrier = match last {
        Intent::Weather { place, day, focus } => {
            // "What about inside?", "and in the bedroom?": the rooms' sensors, not the forecast.
            let indoors = format!("what's the temperature {said}");
            if let intent @ Intent::House(HouseCommand::Climate(..)) = parse(&indoors) {
                return Some((indoors, intent));
            }
            weather(place.as_deref(), *day, *focus, &rest)
        }
        Intent::House(HouseCommand::Climate(indoors, reading)) => {
            if has(TEMPERATURE_WORDS) || has(HUMIDITY_WORDS) {
                let what = said.strip_prefix("the ").unwrap_or(&said);
                let place = if has(WHERE_WORDS) { String::new() } else { where_indoors(indoors) };
                format!("what's the {what}{place}")
            } else {
                format!("what's the {} {said}", reading_words(*reading))
            }
        }
        // Only a room on its own: "and tomorrow" switches nothing, and "and not the hallway" or "and
        // the bedroom is a mess" are no request for its lights.
        Intent::Lights { on, .. } => {
            format!("turn {} the lights in the {}", if *on { "on" } else { "off" }, bare_room(&rest)?)
        }
        Intent::LightsStatus { .. } => format!("are the lights on in the {}", bare_room(&rest)?),
        Intent::LightLevel { level, .. } => {
            let target = format!("the lights in the {}", bare_room(&rest)?);
            match level {
                LightLevel::Percent(percent) => format!("set {target} to {percent} percent"),
                LightLevel::Brighter => format!("make {target} brighter"),
                LightLevel::Dimmer => format!("dim {target}"),
                LightLevel::Color(colour) => format!("make {target} {colour}"),
                LightLevel::Warm => format!("make {target} warmer"),
                LightLevel::Cool => format!("make {target} cooler"),
            }
        }
        Intent::Power(_) => format!("what's the electricity price {said}"),
        _ => return None,
    };
    let intent = parse(&carrier);
    // The same request again adds nothing: "the kitchen" after the power price is no price.
    (family(&intent).is_some() && family(&intent) == family(last) && intent != *last).then_some((carrier, intent))
}

/// Whether `parsed`, the rules' own reading of the words, may still go on from `last`: words no
/// skill takes, and a reading with no place after one in a room. "And the humidity?" is this
/// room's humidity on its own, but the kitchen's after the kitchen's temperature.
pub fn open_to_follow_on(last: &Intent, parsed: &Intent) -> bool {
    let climate = matches!(last, Intent::House(HouseCommand::Climate(..)));
    *parsed == Intent::Unknown || (climate && matches!(parsed, Intent::House(HouseCommand::Climate(Indoors::Here, _))))
}

/// The words after a follow-on's opening; `None` when it has none.
fn after_start(words: &[String]) -> Option<&[String]> {
    STARTS.iter().find_map(|start| {
        let opens = words.len() >= start.len() && start.iter().zip(words).all(|(s, w)| s == w);
        opens.then(|| &words[start.len()..])
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
        _ => None,
    }
}

/// The forecast with what the follow-on names, and the rest from the last one: "how about in
/// Bergen?" keeps the day, "and tomorrow?" the place, and either keeps rain or wind.
fn weather(place: Option<&str>, day: Day, focus: Focus, rest: &[String]) -> String {
    let has = |list: &[&str]| rest.iter().any(|w| list.contains(&w.as_str()));
    let weather_word =
        rest.iter().any(|w| WEATHER_WORDS.contains(&w.as_str()) || WEATHER_STEMS.iter().any(|s| w.contains(s)));
    let mut parts: Vec<String> = Vec::new();
    if !weather_word {
        parts.push(
            match focus {
                Focus::General => "what's the weather",
                Focus::Rain => "will it rain",
                Focus::Wind => "how windy is it",
            }
            .into(),
        );
    }
    parts.push(rest.join(" "));
    let today = has(&["today", "now", "idag", "nå"]) || rest.windows(2).any(|p| p[0] == "i" && p[1] == "dag");
    if day_of(rest) == Day::Today && !today {
        parts.extend(day_words(day));
    }
    if let (None, false, Some(place)) = (place_of(rest), has(HOME_WORDS), place) {
        parts.push(format!("in {place}"));
    }
    parts.join(" ")
}

/// How the rules read `day` back.
fn day_words(day: Day) -> Option<String> {
    let english = |weekday| WEEKDAYS.iter().find(|(d, _, _)| *d == weekday).map(|(_, en, _)| *en);
    Some(match day {
        Day::Today => return None,
        Day::Tonight => "tonight".into(),
        Day::Tomorrow => "tomorrow".into(),
        Day::AfterTomorrow => "the day after tomorrow".into(),
        Day::Weekday(weekday) => format!("on {}", english(weekday)?),
        Day::NextWeekday(weekday) => format!("next {}", english(weekday)?),
        Day::Weekend => "this weekend".into(),
        Day::NextWeekend => "next weekend".into(),
    })
}

fn where_indoors(indoors: &Indoors) -> String {
    match indoors {
        Indoors::Room(room) => format!(" in the {room}"),
        Indoors::Everywhere => " inside".into(),
        Indoors::Here => " in here".into(),
    }
}

fn reading_words(reading: Reading) -> &'static str {
    match reading {
        Reading::Temperature => "temperature",
        Reading::Humidity => "humidity",
        Reading::Both => "temperature and humidity",
    }
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
}
