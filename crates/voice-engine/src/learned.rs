//! Corrections learned from repeats: a request that was not understood, said again soon after in
//! words that were ("Code rent." then "Code red."), pairs what was heard with what was meant. Once
//! the same pair has come up `LEARN_AFTER` times, hearing those words again means the request,
//! without a code change. Only words that are not understood are ever rewritten, so a correction
//! never changes a request that works. Kept as JSON next to the unhandled log; the web page lists
//! them and can forget one.

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tracing::{info, warn};
use voice_assistant::dialog::normalize;

/// A repeat counts when it comes this soon after the request that was not understood.
const REPEAT_WINDOW: Duration = Duration::from_secs(20);
/// Times the same pair must come up before it is used.
pub const LEARN_AFTER: u32 = 2;
/// Longer speech is the room or the TV, not a request said wrong.
const MAX_WORDS: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pair {
    /// What was heard, as normalised words: the key.
    pub heard: String,
    /// What was heard, as transcribed, for the page.
    pub said: String,
    /// The request it meant, as transcribed.
    pub meant: String,
    pub count: u32,
    /// When it last came up, local time.
    pub last: String,
}

#[derive(Default)]
struct State {
    pairs: Vec<Pair>,
    /// The latest request that was not understood, and when.
    failed: Option<(String, Instant)>,
}

pub struct Learned {
    path: Option<PathBuf>,
    state: Mutex<State>,
}

impl Learned {
    /// Kept in `path`, read back from it if it exists; in memory only without one.
    pub fn new(path: Option<PathBuf>) -> Self {
        let pairs = path
            .as_ref()
            .and_then(|path| std::fs::read_to_string(path).ok())
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        Self { path, state: Mutex::new(State { pairs, failed: None }) }
    }

    /// `text` was said to the assistant and not understood.
    pub fn failed(&self, text: &str) {
        let words = words(text);
        if words.is_empty() || words.len() > MAX_WORDS {
            return;
        }
        self.lock().failed = Some((text.trim().to_owned(), Instant::now()));
    }

    /// `text` was understood: if it repeats a request that was not, just before and in like words,
    /// the pair counts once more. Returns the pair when it has just been learned.
    pub fn understood(&self, text: &str) -> Option<Pair> {
        let mut state = self.lock();
        let (said, at) = state.failed.take()?;
        if at.elapsed() > REPEAT_WINDOW || !alike(&words(&said), &words(text)) {
            return None;
        }
        let heard = words(&said).join(" ");
        let last = jiff::Zoned::now().strftime("%Y-%m-%d %H:%M").to_string();
        let meant = text.trim().to_owned();
        let index = match state.pairs.iter().position(|p| p.heard == heard && words(&p.meant) == words(&meant)) {
            Some(i) => i,
            None => {
                state.pairs.push(Pair { heard, said: said.clone(), meant, count: 0, last: String::new() });
                state.pairs.len() - 1
            }
        };
        let pair = &mut state.pairs[index];
        pair.count += 1;
        pair.last = last;
        let pair = pair.clone();
        info!(heard = pair.said, meant = pair.meant, count = pair.count, "repeat after a miss");
        self.save(&state.pairs);
        (pair.count == LEARN_AFTER).then_some(pair)
    }

    /// The request learned for `text`, if its words have come up as a miss often enough.
    pub fn meant(&self, text: &str) -> Option<String> {
        let heard = words(text).join(" ");
        let state = self.lock();
        // The most frequent, if the same words were once meant otherwise.
        state
            .pairs
            .iter()
            .filter(|p| p.heard == heard && p.count >= LEARN_AFTER)
            .max_by_key(|p| p.count)
            .map(|p| p.meant.clone())
    }

    /// Every pair, the most frequent first.
    pub fn list(&self) -> Vec<Pair> {
        let mut pairs = self.lock().pairs.clone();
        pairs.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| b.last.cmp(&a.last)));
        pairs
    }

    /// Drops every pair for these heard words; whether there was one.
    pub fn forget(&self, heard: &str) -> bool {
        let heard = words(heard).join(" ");
        let mut state = self.lock();
        let before = state.pairs.len();
        state.pairs.retain(|p| p.heard != heard);
        let forgot = state.pairs.len() < before;
        if forgot {
            info!(heard, "learned correction forgotten");
            self.save(&state.pairs);
        }
        forgot
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn save(&self, pairs: &[Pair]) {
        let Some(path) = &self.path else { return };
        // Written aside and renamed, so a crash never leaves half a file.
        let partial = path.with_extension("part");
        let result = serde_json::to_vec_pretty(pairs)
            .map_err(std::io::Error::other)
            .and_then(|bytes| std::fs::write(&partial, bytes))
            .and_then(|()| std::fs::rename(&partial, path));
        if let Err(error) = result {
            warn!(%error, path = %path.display(), "could not keep the learned corrections");
        }
    }
}

fn words(text: &str) -> Vec<String> {
    text.split_whitespace().map(normalize).filter(|w| !w.is_empty()).collect()
}

/// Whether a repeat is the same request said again: at least half the words of the longer one are
/// in both ("code rent" / "code red", "who am i on the shopping list" / "what's on the shopping
/// list"). Something else asked after a miss shares few words with it.
fn alike(failed: &[String], meant: &[String]) -> bool {
    if failed == meant {
        return false;
    }
    let shared = failed.iter().filter(|w| meant.contains(w)).count();
    shared > 0 && shared * 2 >= failed.len().max(meant.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn learns_a_pair_said_twice() {
        let learned = Learned::new(None);
        learned.failed("Code rent.");
        assert_eq!(learned.understood("Code red."), None);
        assert_eq!(learned.meant("code rent"), None);
        learned.failed("Code rent!");
        let pair = learned.understood("Code red.").unwrap();
        assert_eq!((pair.heard.as_str(), pair.count), ("code rent", 2));
        assert_eq!(learned.meant("Code rent."), Some("Code red.".to_owned()));
        assert!(learned.forget("Code rent."));
        assert_eq!(learned.meant("Code rent."), None);
    }

    #[test]
    fn ignores_what_is_not_a_repeat() {
        let learned = Learned::new(None);
        for _ in 0..3 {
            // The TV, then a real request.
            learned.failed("I'm not a fan of the music video.");
            learned.understood("Turn on the music.");
            // A request after a long chat is not a repeat of it.
            learned.failed("I was able to fly a trumpet even with my terrible addition of rules.");
            learned.understood("Turn off the lights.");
        }
        assert!(learned.list().is_empty());
        // Only the request right after the miss counts.
        learned.failed("Increase the sum.");
        learned.understood("What time is it?");
        assert_eq!(learned.understood("Increase the sound."), None);
        assert!(learned.list().is_empty());
    }

    #[test]
    fn real_mishearings_are_alike() {
        let pairs = [
            ("Who am I on the shopping list?", "What's on the shopping list?"),
            ("Increase the sum.", "Increase the sound."),
            ("The bowling at 60%.", "The volume at 60%."),
            ("Stand out.", "Stand down."),
            ("Set a towel for 10 minutes.", "Set a timer for 10 minutes."),
        ];
        for (heard, meant) in pairs {
            assert!(alike(&words(heard), &words(meant)), "{heard}");
        }
        assert!(!alike(&words("Batrop cup."), &words("Turn on the kitchen.")));
    }

    #[test]
    fn kept_across_restarts() {
        let path = std::env::temp_dir().join(format!("learned-test-{}.json", std::process::id()));
        let learned = Learned::new(Some(path.clone()));
        for _ in 0..2 {
            learned.failed("Stand out.");
            learned.understood("Stand down.");
        }
        let again = Learned::new(Some(path.clone()));
        assert_eq!(again.meant("stand out"), Some("Stand down.".to_owned()));
        let _ = std::fs::remove_file(path);
    }
}
