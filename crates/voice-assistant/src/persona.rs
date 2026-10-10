//! A butler's way of speaking for English answers, after JARVIS: "As you wish, sir. The living room
//! lights are off. Darkness it is." The rules' answers stay plain; this rewrites them on the way out,
//! so every skill gets the style without its own wording. Wording is picked at random from small
//! pools, with now and then a remark about the answer, so it does not sound canned without a
//! language model.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rand::rngs::StdRng;
use rand::{RngExt, SeedableRng};

/// Each followed by ", sir."
const OPENERS: &[&str] =
    &["As you wish", "Very good", "Right away", "Of course", "Certainly", "Consider it done", "Yes"];
const WELCOME: &[&str] = &["At your service", "Always a pleasure", "Anytime", "Happy to help"];
/// "Okay, paused." becomes "Paused, sir." up to this many words; longer ones get an opener.
const SHORT_WORDS: usize = 2;
/// Questions up to this many words get the honorific ("Which room, sir?").
const SHORT_QUESTION_WORDS: usize = 4;
/// How often an answer gets a remark, when one fits.
const QUIP_CHANCE: f64 = 0.4;
/// The first answer after this long may open with "Good evening".
const GREETING_GAP: Duration = Duration::from_secs(3 * 3600);
const GREETING_CHANCE: f64 = 0.5;

/// Remarks by what the answer says: any of the words (lowercase), then the remarks to pick from.
const QUIPS: &[(&[&str], &[&str])] = &[
    (&["rain", "showers", "drizzle"], &["You may want an umbrella.", "Not a day for the bicycle, I suspect."]),
    (&["snow", "sleet"], &["Do wrap up warm.", "Perhaps the sensible boots today."]),
    (&["minus"], &["Rather chilly, I'm afraid.", "I'd recommend a coat."]),
    (&["clear sky", "sunny", "fair"], &["A fine day for it.", "Splendid weather, for once."]),
    (&["lights are off", "light is off", "lights off"], &["Darkness it is.", "Lights out."]),
    (&["lights are on", "light is on", "lights on"], &["Much better.", "Let there be light."]),
    (&["paused"], &["Enjoy the silence."]),
    (&["next song"], &["Let's hope this one is better."]),
    (&["remind you", "timer set"], &["I'll keep an eye on the clock.", "I'll let you know."]),
    (&["on the list"], &["Noted.", "It won't be forgotten."]),
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
        let styled = match greeting {
            Some(greeting) if long_quiet && !answer.ends_with('?') && self.rng.random_bool(GREETING_CHANCE) => {
                format!("{greeting}, {}. {}", self.honorific, plain(answer))
            }
            _ => self.addressed(answer),
        };
        match self.quip(answer) {
            Some(quip) if !styled.ends_with('?') => format!("{styled} {quip}"),
            _ => styled,
        }
    }

    /// The answer with the honorific worked in once.
    fn addressed(&mut self, answer: &str) -> String {
        let h = self.honorific.clone();
        if answer == "You're welcome." {
            return format!("{}, {h}.", self.pick(WELCOME));
        }
        if answer == "Sorry, I didn't catch that." {
            let options = [
                format!("I'm afraid I didn't catch that, {h}."),
                format!("Pardon me, {h}, I missed that."),
                format!("Could you say that again, {h}?"),
            ];
            return options[self.rng.random_range(0..options.len())].clone();
        }
        if answer == "Okay." {
            return format!("{}, {h}.", self.pick(OPENERS));
        }
        if let Some(rest) = answer.strip_prefix("Okay, ") {
            let (first, tail) = split_first(rest);
            let words = first.trim_end_matches(['.', '!']).split_whitespace().count();
            if words <= SHORT_WORDS && first.ends_with('.') {
                return format!("{}{tail}", with_honorific(&capitalize(first), &h));
            }
            return format!("{}, {h}. {}", self.pick(OPENERS), capitalize(rest));
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

    /// Now and then a remark that fits the answer, never the same one twice running.
    fn quip(&mut self, answer: &str) -> Option<&'static str> {
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
