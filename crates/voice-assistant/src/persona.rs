//! A butler's way of speaking for English answers, after JARVIS: "As you wish, sir. The living room
//! lights are off. Darkness it is." The rules' answers stay plain; this rewrites them on the way out,
//! so every skill gets the style without its own wording. Wording is picked at random from small
//! pools, with now and then a remark about the answer, so it does not sound canned without a
//! language model.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rand::rngs::StdRng;
use rand::{RngExt, SeedableRng};

/// Each followed by ", sir."
const OPENERS: &[&str] = &[
    "As you wish",
    "Very good",
    "Right away",
    "Of course",
    "Certainly",
    "Consider it done",
    "Yes",
    "At once",
    "With pleasure",
    "Absolutely",
    "Straight away",
    "Very well",
];
const WELCOME: &[&str] = &[
    "At your service",
    "Always a pleasure",
    "Anytime",
    "Happy to help",
    "My pleasure",
    "Not at all",
    "Glad to be of use",
    "Don't mention it",
];
/// For "Sorry, I didn't catch that."; `{h}` is the honorific.
const MISSED: &[&str] = &[
    "I'm afraid I didn't catch that, {h}.",
    "Pardon me, {h}, I missed that.",
    "Could you say that again, {h}?",
    "I didn't quite get that, {h}.",
    "Once more, {h}?",
    "Forgive me, {h}, I didn't follow.",
];
/// "Okay, paused." becomes "Paused, sir." up to this many words; longer ones get an opener.
const SHORT_WORDS: usize = 2;
/// Questions up to this many words get the honorific ("Which room, sir?").
const SHORT_QUESTION_WORDS: usize = 4;
/// How often an answer gets a remark, when one fits.
const QUIP_CHANCE: f64 = 0.4;
/// Longer answers (a briefing) get no remark: it would land after the last item, not the one it is about.
const QUIP_MAX_SENTENCES: usize = 3;
/// The first answer after this long may open with "Good evening".
const GREETING_GAP: Duration = Duration::from_secs(3 * 3600);
const GREETING_CHANCE: f64 = 0.5;
/// Remarks for the first request after a quiet spell in the small hours, when no other remark fits.
const LATE: &[&str] = &["Burning the midnight oil, I see.", "Do try to get some sleep.", "The house is fast asleep."];
const LATE_HOURS: std::ops::RangeInclusive<u8> = 0..=4;

/// Remarks by what the answer says: any of the words (lowercase), then the remarks to pick from.
const QUIPS: &[(&[&str], &[&str])] = &[
    (&["rain", "showers", "drizzle"], &["You may want an umbrella.", "Not a day for the bicycle, I suspect."]),
    (&["snow", "sleet"], &["Do wrap up warm.", "Perhaps the sensible boots today."]),
    (&["minus"], &["Rather chilly, I'm afraid.", "I'd recommend a coat."]),
    (&["clear sky", "sunny", "fair"], &["A fine day for it.", "Splendid weather, for once.", "Sunglasses weather."]),
    (&["fog"], &["Mind how you go.", "Rather atmospheric."]),
    (&["thunder"], &["Best stay indoors.", "Quite dramatic."]),
    (&["lights are off", "light is off", "lights off"], &["Darkness it is.", "Lights out."]),
    (&["lights are on", "light is on", "lights on"], &["Much better.", "Let there be light."]),
    (&["paused"], &["Enjoy the silence.", "Peace and quiet."]),
    (&["next song"], &["Let's hope this one is better.", "Moving swiftly on."]),
    (&["previous song"], &["A classic, then.", "Once more, with feeling."]),
    (&["okay, muted", "tv is muted"], &["Blissful quiet.", "Silence is golden."]),
    (&["remind you", "timer set"], &["I'll keep an eye on the clock.", "I'll let you know.", "The clock is ticking."]),
    (&["timer is done"], &["Time's up.", "Right on schedule."]),
    (&["on the list"], &["Noted.", "It won't be forgotten.", "Consider it remembered."]),
    (&["off the list"], &["One less thing.", "Crossed off."]),
    (&["the tv is off"], &["Back to the real world.", "Enough screen time."]),
    (&["the tv is on"], &["Enjoy the show.", "Showtime."]),
    (&["good night."], &["Sleep well.", "Sweet dreams."]),
    (&["see you later."], &["I'll hold the fort.", "Safe travels."]),
    (&["lockdown initiated"], &["Nobody gets in.", "Fortress mode."]),
    (&["standing down"], &["Crisis averted.", "Back to normal."]),
    (
        &["in 1 minute", "in 2 minutes", "in 3 minutes", "in 4 minutes"],
        &["You may want to hurry.", "Best get your shoes on."],
    ),
];

pub struct Persona {
    /// How the user is addressed: "sir".
    honorific: String,
    rng: StdRng,
    last_quip: Option<&'static str>,
    last_opener: Option<&'static str>,
    last_spoke: Option<Instant>,
}

impl Persona {
    pub fn new(honorific: &str) -> Self {
        let seed = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64);
        Self::with_seed(honorific, seed)
    }

    pub fn with_seed(honorific: &str, seed: u64) -> Self {
        Self {
            honorific: honorific.trim().to_owned(),
            rng: StdRng::seed_from_u64(seed),
            last_quip: None,
            last_opener: None,
            last_spoke: None,
        }
    }

    /// The answer in the persona's words; `hour` is the local hour, for greetings. Answers that
    /// already address the user are left alone.
    pub fn style(&mut self, answer: &str, hour: u8) -> String {
        let answer = answer.trim();
        let long_quiet = self.last_spoke.is_none_or(|at| at.elapsed() > GREETING_GAP);
        self.last_spoke = Some(Instant::now());
        if answer.is_empty() || has_word(answer, &self.honorific) {
            return answer.to_owned();
        }
        let greeting = match hour {
            5..=11 => Some("Good morning"),
            12..=17 => Some("Good afternoon"),
            18..=23 => Some("Good evening"),
            _ => None,
        };
        // The briefing opens with its own greeting.
        let greets = ["Good morning", "Good afternoon", "Good evening"].iter().any(|g| answer.starts_with(g));
        let styled = match greeting {
            Some(greeting)
                if long_quiet && !greets && !answer.ends_with('?') && self.rng.random_bool(GREETING_CHANCE) =>
            {
                format!("{greeting}, {}. {}", self.honorific, plain(answer))
            }
            _ => self.addressed(answer),
        };
        let late = long_quiet && LATE_HOURS.contains(&hour);
        match self.quip(answer).or_else(|| if late { self.late() } else { None }) {
            Some(quip) if !styled.ends_with('?') => format!("{styled} {quip}"),
            _ => styled,
        }
    }

    fn late(&mut self) -> Option<&'static str> {
        self.rng.random_bool(QUIP_CHANCE).then(|| self.pick(LATE))
    }

    /// The answer with the honorific worked in once.
    fn addressed(&mut self, answer: &str) -> String {
        let h = self.honorific.clone();
        if answer == "You're welcome." {
            return format!("{}, {h}.", self.pick(WELCOME));
        }
        if answer == "Sorry, I didn't catch that." {
            return self.pick(MISSED).replace("{h}", &h);
        }
        if answer == "Okay." {
            return format!("{}, {h}.", self.opener());
        }
        if let Some(rest) = answer.strip_prefix("Okay, ") {
            let (first, tail) = split_first(rest);
            let words = first.trim_end_matches(['.', '!']).split_whitespace().count();
            if words <= SHORT_WORDS && first.ends_with('.') {
                return format!("{}{tail}", with_honorific(&capitalize(first), &h));
            }
            return format!("{}, {h}. {}", self.opener(), capitalize(rest));
        }
        if let Some(rest) = answer.strip_prefix("Sorry, ") {
            let (first, tail) = split_first(rest);
            return if self.rng.random_bool(0.5) {
                format!("I'm afraid {}{tail}", with_honorific(first, &h))
            } else {
                format!("My apologies, {h}. {}", capitalize(rest))
            };
        }
        let (first, tail) = split_first(answer);
        if first.ends_with('?') && first.split_whitespace().count() > SHORT_QUESTION_WORDS {
            return answer.to_owned();
        }
        format!("{}{tail}", with_honorific(first, &h))
    }

    /// The answer after a lead-in that already addressed the user ("Checking the forecast, sir."):
    /// no second honorific or opener, now and then a remark.
    pub fn after_lead(&mut self, answer: &str) -> String {
        let answer = answer.trim();
        self.last_spoke = Some(Instant::now());
        let styled = plain(answer);
        match self.quip(answer) {
            Some(quip) if !styled.ends_with('?') => format!("{styled} {quip}"),
            _ => styled,
        }
    }

    /// Every fixed sentence the persona says, to render ahead of time. Answers are spoken a
    /// sentence at a time, so these are whole sentences as they are spoken.
    pub fn phrases(&self) -> Vec<String> {
        let h = &self.honorific;
        let mut phrases: Vec<String> = OPENERS.iter().chain(WELCOME).map(|o| format!("{o}, {h}.")).collect();
        phrases.extend(["Good morning", "Good afternoon", "Good evening"].map(|g| format!("{g}, {h}.")));
        phrases.extend(MISSED.iter().map(|m| m.replace("{h}", h)));
        phrases.push(format!("My apologies, {h}."));
        phrases.extend(QUIPS.iter().flat_map(|(_, quips)| quips.iter().map(|q| q.to_string())));
        phrases.extend(LATE.iter().map(|q| q.to_string()));
        phrases
    }

    /// Every way [`Persona::style`] can put a fixed answer, without greetings and remarks (those
    /// are in [`Persona::phrases`]), for rendering ahead of time.
    pub fn forms(&self, answer: &str) -> Vec<String> {
        let answer = answer.trim();
        // Only these are worded at random; the rest have one form, and the time alone is 1440 answers.
        let varies = answer.starts_with("Okay") || answer.starts_with("Sorry, ") || answer == "You're welcome.";
        let seeds = if varies { 256 } else { 1 };
        let mut forms: Vec<String> =
            (0..seeds).map(|seed| Persona::with_seed(&self.honorific, seed).addressed(answer)).collect();
        forms.sort();
        forms.dedup();
        forms
    }

    /// Now and then a remark that fits the answer, never the same one twice running.
    fn quip(&mut self, answer: &str) -> Option<&'static str> {
        if answer.split_inclusive(['.', '?', '!']).filter(|s| !s.trim().is_empty()).count() > QUIP_MAX_SENTENCES {
            return None;
        }
        let lower = answer.to_lowercase();
        let (_, quips) = QUIPS.iter().find(|(words, _)| words.iter().any(|w| lower.contains(w)))?;
        if !self.rng.random_bool(QUIP_CHANCE) {
            return None;
        }
        let fresh: Vec<&'static str> = quips.iter().copied().filter(|q| Some(*q) != self.last_quip).collect();
        let quip = *fresh.get(self.rng.random_range(0..fresh.len().max(1)))?;
        self.last_quip = Some(quip);
        Some(quip)
    }

    /// An opener, never the same one twice running.
    fn opener(&mut self) -> &'static str {
        let fresh: Vec<&'static str> = OPENERS.iter().copied().filter(|o| Some(*o) != self.last_opener).collect();
        let opener = fresh[self.rng.random_range(0..fresh.len())];
        self.last_opener = Some(opener);
        opener
    }

    fn pick(&mut self, options: &[&'static str]) -> &'static str {
        options[self.rng.random_range(0..options.len())]
    }
}

/// The answer without its "Okay," or "Sorry," lead, for after a greeting.
fn plain(answer: &str) -> String {
    if answer == "Okay." {
        return "Done.".into();
    }
    if let Some(rest) = answer.strip_prefix("Okay, ") {
        return capitalize(rest);
    }
    if let Some(rest) = answer.strip_prefix("Sorry, ") {
        return format!("I'm afraid {rest}");
    }
    answer.to_owned()
}

/// The first sentence and the rest, the rest with its leading space. A full stop between digits
/// ("3.5 degrees") does not end a sentence.
fn split_first(text: &str) -> (&str, &str) {
    let end = text
        .char_indices()
        .find(|&(i, c)| matches!(c, '.' | '?' | '!') && text[i + 1..].chars().next().is_none_or(char::is_whitespace))
        .map_or(text.len(), |(i, _)| i + 1);
    text.split_at(end)
}

/// "Paused." -> "Paused, sir."
fn with_honorific(sentence: &str, honorific: &str) -> String {
    match sentence.char_indices().last() {
        Some((i, c)) if matches!(c, '.' | '?' | '!') => format!("{}, {honorific}{c}", &sentence[..i]),
        _ => format!("{sentence}, {honorific}."),
    }
}

fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map_or_else(String::new, |c| c.to_uppercase().chain(chars).collect())
}

fn has_word(text: &str, word: &str) -> bool {
    text.split(|c: char| !c.is_alphanumeric()).any(|w| w.eq_ignore_ascii_case(word))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Night: no greeting, so only the openers and remarks vary.
    const NIGHT: u8 = 3;

    fn sir_once(text: &str) -> bool {
        text.matches("sir").count() == 1
    }

    #[test]
    fn a_briefing_greets_once_and_has_no_remark() {
        let briefing =
            "Good evening! It's 18:02. It's 4 degrees and rain in Oslo. In the news. Peace Prize reaction rolls in.";
        for seed in 0..50 {
            // A fresh persona has been quiet long enough to greet.
            let styled = Persona::with_seed("sir", seed).style(briefing, 18);
            assert!(styled.starts_with("Good evening, sir! It's 18:02."), "{styled}");
            assert!(styled.ends_with("Peace Prize reaction rolls in."), "{styled}");
        }
    }

    #[test]
    fn forms_cover_every_styling() {
        let p = Persona::with_seed("sir", 1);
        assert_eq!(p.forms("Okay, paused."), ["Paused, sir."]);
        assert_eq!(p.forms("Okay.").len(), OPENERS.len());
        for seed in 0..20 {
            let styled = Persona::with_seed("sir", seed).addressed("Okay, the TV is off.");
            assert!(p.forms("Okay, the TV is off.").contains(&styled), "{styled}");
        }
    }

    #[test]
    fn speaks_like_a_butler() {
        for seed in 0..50 {
            let mut p = Persona::with_seed("sir", seed);
            let lights = p.style("Okay, the living room lights are off.", NIGHT);
            assert!(OPENERS.iter().any(|o| lights.starts_with(&format!("{o}, sir. The living room lights are off."))));
            assert!(sir_once(&lights), "{lights}");
            assert!(p.style("Okay, paused.", NIGHT).starts_with("Paused, sir."));
            let sorry = p.style("Sorry, I couldn't reach Entur.", NIGHT);
            assert!(
                ["I'm afraid I couldn't reach Entur, sir.", "My apologies, sir. I couldn't reach Entur."]
                    .contains(&sorry.as_str()),
                "{sorry}"
            );
            assert_eq!(p.style("Which room?", NIGHT), "Which room, sir?");
            let weather = p.style("Tomorrow in Oslo: heavy rain, 3 to 6 degrees.", NIGHT);
            assert!(weather.starts_with("Tomorrow in Oslo: heavy rain, 3 to 6 degrees, sir."), "{weather}");
            assert!(sir_once(&p.style("You're welcome.", NIGHT)));
            assert!(sir_once(&p.style("Sorry, I didn't catch that.", NIGHT)));
        }
        let mut p = Persona::with_seed("sir", 1);
        assert!(p.style("It's 3.5 degrees. Dry all day.", NIGHT).starts_with("It's 3.5 degrees, sir. Dry all day."));
    }

    #[test]
    fn varies_and_does_not_repeat_remarks() {
        let mut p = Persona::with_seed("sir", 7);
        let answers: Vec<String> = (0..40).map(|_| p.style("Okay, the kitchen lights are on.", NIGHT)).collect();
        let distinct: std::collections::HashSet<_> = answers.iter().collect();
        assert!(distinct.len() > 5, "{distinct:?}");
        let quips: Vec<&str> = answers
            .iter()
            .filter_map(|a| ["Much better.", "Let there be light."].into_iter().find(|q| a.ends_with(q)))
            .collect();
        assert!(!quips.is_empty());
        assert!(quips.windows(2).all(|w| w[0] != w[1]), "{quips:?}");
    }

    #[test]
    fn openers_do_not_repeat() {
        let mut p = Persona::with_seed("sir", 5);
        let said: Vec<String> = (0..40).map(|_| p.style("Okay.", NIGHT)).collect();
        assert!(said.windows(2).all(|w| w[0] != w[1]), "{said:?}");
    }

    #[test]
    fn remarks_on_the_small_hours_once() {
        let late = |text: &str| LATE.iter().any(|q| text.ends_with(q));
        assert!((0..50).any(|seed| late(&Persona::with_seed("sir", seed).style("Okay, stopped.", 2))));
        assert!((0..50).all(|seed| !late(&Persona::with_seed("sir", seed).style("Okay, stopped.", 14))));
        for seed in 0..50 {
            let mut p = Persona::with_seed("sir", seed);
            p.style("Okay, stopped.", 2);
            assert!(!late(&p.style("Okay, stopped.", 2)));
        }
    }

    #[test]
    fn unmuted_gets_no_mute_remark() {
        for seed in 0..50 {
            let styled = Persona::with_seed("sir", seed).style("Okay, unmuted.", NIGHT);
            assert!(!styled.contains("quiet") && !styled.contains("Silence"), "{styled}");
        }
    }

    #[test]
    fn greets_after_a_quiet_spell() {
        let greeted = (0..50).any(|seed| {
            Persona::with_seed("sir", seed)
                .style("Okay, the hall lights are on.", 20)
                .starts_with("Good evening, sir. The hall lights are on.")
        });
        assert!(greeted);
        let mut p = Persona::with_seed("sir", 3);
        p.style("Okay.", 20);
        assert!(!p.style("Okay.", 20).starts_with("Good evening"));
    }

    #[test]
    fn leaves_some_answers_alone() {
        let mut p = Persona::with_seed("sir", 0);
        assert_eq!(p.style("Very well, sir.", NIGHT), "Very well, sir.");
        let question = "Which place do you want the weather for?";
        assert_eq!(p.style(question, NIGHT), question);
    }
}
