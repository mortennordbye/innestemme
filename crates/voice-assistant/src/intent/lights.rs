//! Light requests: brightness, colour, moods, questions about what is on, and switches at a
//! time of day that cannot be scheduled.

use super::*;

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
pub(super) fn scheduled_switch(words: &[String], text: &str) -> bool {
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
pub(super) fn light_level(words: &[String], text: &str) -> Option<LightLevel> {
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
pub(super) fn level_target(text: &str) -> String {
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

pub(super) fn is_lights_question(words: &[String]) -> bool {
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

pub(super) const LIGHT_WORDS: &[&str] =
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
pub(super) fn mood(words: &[String]) -> Option<&'static str> {
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
