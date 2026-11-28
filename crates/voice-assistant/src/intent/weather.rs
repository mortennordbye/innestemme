//! Weather questions: the day, the place and whether rain or wind is the point.

use super::*;

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

pub(super) const WEATHER_WORDS: &[&str] = &[
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
pub(super) const WEATHER_STEMS: &[&str] = &["weather", "forecast", "peratur", "værmeld", "været"];
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
pub(super) const HOME_WORDS: &[&str] = &["home", "here", "house", "hjemme", "her", "huset"];
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
pub(super) const WEEKDAYS: &[(jiff::civil::Weekday, &str, &str)] = {
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
pub(super) fn day_of(words: &[String]) -> Day {
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

pub(super) fn focus_of(words: &[String]) -> Focus {
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
pub(super) fn wants_clothes(words: &[String]) -> bool {
    let any = |list: &[&str]| words.iter().any(|w| list.contains(&w.as_str()));
    any(CLOTHES) && any(CLOTHES_VERBS)
}

/// Verbs after "I" that make it the English pronoun.
const ENGLISH_VERBS: &[&str] = &[
    "don't", "dont", "do", "did", "didn't", "am", "was", "think", "know", "want", "need", "guess", "mean", "have",
    "will", "would", "can", "can't", "just", "really",
];

/// Where a weather question asks about: the name after the last place marker, without times;
/// `None` for the home.
pub(super) fn place_of(words: &[String]) -> Option<String> {
    let time = |w: &str| TIME_WORDS.contains(&w.trim_end_matches("'s")) || w.starts_with(|c: char| c.is_ascii_digit());
    // "I" first or after these is the English pronoun, not the Norwegian "in": "I always do weather
    // today", "do I need an umbrella".
    // Before an English verb too: "What's the temperature raising? I don't know." took "don't
    // know" as the place (25 Nov).
    let pronoun = |i: usize| {
        i == 0
            || ["do", "should", "will", "can", "shall", "would", "could", "must", "if"].contains(&words[i - 1].as_str())
            || words.get(i + 1).is_some_and(|next| ENGLISH_VERBS.contains(&next.as_str()))
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

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    pub(in crate::intent) fn weather(place: Option<&str>, day: Day) -> Intent {
        Intent::Weather { place: place.map(str::to_owned), day, focus: Focus::General }
    }

    fn rain(place: Option<&str>, day: Day) -> Intent {
        Intent::Weather { place: place.map(str::to_owned), day, focus: Focus::Rain }
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
    fn i_before_a_verb_is_no_place() {
        let words = |t: &str| crate::dialog::words(t);
        assert_eq!(place_of(&words("What's the temperature raising? I don't know.")), None);
        assert_eq!(place_of(&words("what's the weather i think")), None);
        assert_eq!(place_of(&words("hva er været i Bergen")).as_deref(), Some("bergen"));
    }

    #[test]
    fn misheard_places_stay_places() {
        assert_eq!(parse("How is the weather in us of tomorrow?"), weather(Some("us of"), Day::Tomorrow));
        assert_eq!(parse("How is the weather at all slow tomorrow?"), weather(Some("all slow"), Day::Tomorrow));
    }
}
