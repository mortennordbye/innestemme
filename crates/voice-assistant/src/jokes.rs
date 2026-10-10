//! Jokes: English ones live from icanhazdadjoke.com (no key), with a built-in fallback, or only the
//! built-in ones when speech is rendered ahead of time (`built_in_only`); Norwegian ones from a
//! built-in list, since there is no free Norwegian joke service. The last few are not repeated.

use std::collections::VecDeque;
use std::time::Duration;

use serde::Deserialize;
use tracing::warn;

use crate::lang::Lang;

const TIMEOUT: Duration = Duration::from_secs(3);
const REMEMBER: usize = 8;

const ENGLISH: &[&str] = &[
    "Why don't skeletons fight each other? They don't have the guts.",
    "I'm reading a book about anti-gravity. It's impossible to put down.",
    "What do you call a fake noodle? An impasta.",
    "Why did the scarecrow win an award? Because he was outstanding in his field.",
    "I told my wife she was drawing her eyebrows too high. She looked surprised.",
    "What do you call a bear with no teeth? A gummy bear.",
    "Why can't a bicycle stand up by itself? It's two tired.",
    "What did the ocean say to the beach? Nothing, it just waved.",
    "Why do cows wear bells? Because their horns don't work.",
    "What do you call a factory that makes okay products? A satisfactory.",
    "Why did the coffee file a police report? It got mugged.",
    "I used to hate facial hair, but then it grew on me.",
    "What do you call a pile of cats? A meowntain.",
    "Why don't eggs tell jokes? They'd crack each other up.",
    "How does a penguin build its house? Igloos it together.",
    "What do you call a fish wearing a bowtie? Sofishticated.",
    "Why did the math book look so sad? Because it had too many problems.",
    "I would tell you a construction joke, but I'm still working on it.",
    "What did the grape do when it got stepped on? It let out a little wine.",
    "Why couldn't the leopard play hide and seek? Because he was always spotted.",
    "What do you call cheese that isn't yours? Nacho cheese.",
    "Why did the golfer bring two pairs of trousers? In case he got a hole in one.",
    "I only know twenty five letters of the alphabet. I don't know y.",
    "What do you call an alligator in a vest? An investigator.",
    "Why are elevator jokes so classic? They work on many levels.",
    "Did you hear about the claustrophobic astronaut? He just needed a little space.",
    "What did one wall say to the other? I'll meet you at the corner.",
    "Why don't scientists trust atoms? Because they make up everything.",
    "I'm afraid for the calendar. Its days are numbered.",
    "What do you call a sleeping bull? A bulldozer.",
];

const NORWEGIAN: &[&str] = &[
    "Hva er gult og kan ikke svømme? En gravemaskin.",
    "Hva sa nullen til åtteren? Fint belte!",
    "Hva kaller du en bjørn uten tenner? En gummibjørn.",
    "Hva sa havet til stranda? Ingenting, det bare vinket.",
    "Hvorfor var matteboka lei seg? Den hadde så mange problemer.",
    "Hvilket dyr kan hoppe høyere enn et hus? Alle sammen. Hus kan ikke hoppe.",
    "Læreren spør: Hvis jeg har fem epler i den ene hånden og seks i den andre, hva har jeg da? Eleven svarer: Veldig store hender.",
    "Hva sier den ene snømannen til den andre? Lukter det ikke gulrot her?",
    "Hvorfor går ikke fisker på skolen? De er allerede i en stim.",
    "Vet du forskjellen på en god vits og en dårlig vits? Denne var den dårlige.",
];

#[derive(Deserialize)]
struct DadJoke {
    joke: String,
}

pub struct Jokes {
    agent: ureq::Agent,
    /// Only the built-in jokes: their speech can be rendered ahead of time.
    built_in_only: bool,
    recent: VecDeque<String>,
    next: usize,
}

impl Default for Jokes {
    fn default() -> Self {
        // Start somewhere different on each run.
        let next =
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as usize);
        Self {
            agent: ureq::Agent::config_builder().timeout_global(Some(TIMEOUT)).build().into(),
            built_in_only: false,
            recent: VecDeque::new(),
            next,
        }
    }
}

impl Jokes {
    pub fn built_in_only(self) -> Self {
        Self { built_in_only: true, ..self }
    }

    /// Every built-in English joke, to render ahead of time.
    pub fn english() -> &'static [&'static str] {
        ENGLISH
    }

    pub fn tell(&mut self, lang: Lang) -> String {
        let joke = match lang {
            Lang::English if self.built_in_only => self.built_in(ENGLISH),
            Lang::English => self.fetch().unwrap_or_else(|| self.built_in(ENGLISH)),
            Lang::Norwegian => self.built_in(NORWEGIAN),
        };
        self.remember(&joke);
        joke
    }

    /// A fresh joke from the service, or `None` when it fails or repeats a recent one.
    fn fetch(&self) -> Option<String> {
        let response = self
            .agent
            .get("https://icanhazdadjoke.com/")
            .header("Accept", "application/json")
            .header("User-Agent", "innestemme voice assistant (https://github.com/mortennordbye/innestemme)")
            .call();
        match response.map(|mut r| r.body_mut().read_json::<DadJoke>()) {
            Ok(Ok(DadJoke { joke })) if !self.recent.contains(&joke) => {
                Some(joke.split_whitespace().collect::<Vec<_>>().join(" "))
            }
            Ok(Ok(_)) => None,
            Ok(Err(error)) => {
                warn!(%error, "joke service returned something unexpected");
                None
            }
            Err(error) => {
                warn!(%error, "joke service unreachable");
                None
            }
        }
    }

    fn built_in(&mut self, list: &[&str]) -> String {
        for _ in 0..list.len() {
            self.next = self.next.wrapping_add(1);
            let joke = list[self.next % list.len()];
            if !self.recent.iter().any(|r| r == joke) {
                return joke.to_owned();
            }
        }
        list[self.next % list.len()].to_owned()
    }

    fn remember(&mut self, joke: &str) {
        self.recent.push_back(joke.to_owned());
        if self.recent.len() > REMEMBER {
            self.recent.pop_front();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn built_in_jokes_do_not_repeat_within_the_window() {
        let mut jokes = Jokes::default();
        let told: Vec<String> = (0..REMEMBER).map(|_| jokes.tell(Lang::Norwegian)).collect();
        let unique: std::collections::HashSet<&String> = told.iter().collect();
        assert_eq!(unique.len(), told.len());
    }
}
