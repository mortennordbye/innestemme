//! Finds the known name a misheard phrase meant: "round lamb" is the Round lamp, "kakma de faka"
//! is Kakkmaddafakka, "my trending playlist" is Treninggg. Names come from the home (lights,
//! rooms) and the music library (artists, playlists); a transcriber only guesses their spelling.
//!
//! Both sides are reduced to a rough sound key (Norwegian letters folded, doubled letters and
//! spelling variants merged, spaces dropped), and a name matches a run of words in the phrase when
//! the edit distance between the keys is small for their length.

/// A match needs at least this similarity (1 - distance / longer key length).
const MIN_SCORE: f32 = 0.74;
/// Short keys need a closer match: one letter off in "kos" is a quarter of it.
const SHORT_KEY: usize = 4;

#[derive(Debug, Clone, PartialEq)]
pub struct Match<'a> {
    pub name: &'a str,
    pub score: f32,
    /// The words of the phrase the name replaced, as a range of word indices.
    pub words: std::ops::Range<usize>,
}

#[derive(Debug, Clone, Default)]
pub struct Names {
    entries: Vec<(String, String)>,
}

impl Names {
    pub fn new<S: AsRef<str>>(names: impl IntoIterator<Item = S>) -> Self {
        let entries = names
            .into_iter()
            .map(|n| n.as_ref().trim().to_owned())
            .filter(|n| !n.is_empty())
            .map(|n| {
                let key = sound_key(&n);
                (n, key)
            })
            .filter(|(_, key)| key.len() >= 2)
            .collect();
        Self { entries }
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The best name for any run of up to four words in `phrase`, if it is close enough.
    pub fn best(&self, phrase: &str) -> Option<Match<'_>> {
        let words: Vec<&str> = phrase.split_whitespace().collect();
        let keys: Vec<String> = words.iter().map(|w| sound_key(w)).collect();
        let mut best: Option<Match> = None;
        for (name, key) in &self.entries {
            for start in 0..keys.len() {
                let mut joined = String::new();
                for (end, key_part) in keys.iter().enumerate().skip(start).take(4) {
                    joined.push_str(key_part);
                    if joined.is_empty() {
                        continue;
                    }
                    // Windows much longer than the name only add distance.
                    if joined.len() > key.len() * 2 + 2 {
                        break;
                    }
                    let score = similarity(key, &joined);
                    let needed = if key.len() <= SHORT_KEY { 0.75 } else { MIN_SCORE };
                    if score >= needed && best.as_ref().is_none_or(|b| better(score, b, key)) {
                        best = Some(Match { name, score, words: start..end + 1 });
                    }
                }
            }
        }
        best
    }
}

/// Higher score wins; on a tie, the longer name (it explains more of the phrase).
fn better(score: f32, current: &Match, key: &str) -> bool {
    score > current.score + 1e-6 || ((score - current.score).abs() <= 1e-6 && key.len() > sound_key(current.name).len())
}

/// 1 for equal keys, 0 for nothing in common.
pub fn similarity(a: &str, b: &str) -> f32 {
    let longest = a.chars().count().max(b.chars().count()).max(1);
    1.0 - levenshtein(a, b) as f32 / longest as f32
}

fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for i in 1..=a.len() {
        let mut prev = row[0];
        row[0] = i;
        for j in 1..=b.len() {
            let cur = row[j];
            row[j] = (row[j] + 1).min(row[j - 1] + 1).min(prev + usize::from(a[i - 1] != b[j - 1]));
            prev = cur;
        }
    }
    row[b.len()]
}

/// A rough sound key: lowercase letters only, Norwegian and accented letters folded, common
/// spelling variants merged, repeated letters collapsed.
pub fn sound_key(text: &str) -> String {
    let mut s = String::new();
    for c in text.to_lowercase().chars() {
        match c {
            'a'..='z' => s.push(c),
            '0'..='9' => s.push(c),
            'æ' => s.push_str("ae"),
            'ø' | 'ö' | 'ó' | 'ò' | 'ô' => s.push('o'),
            'å' | 'á' | 'à' | 'â' | 'ä' => s.push('a'),
            'é' | 'è' | 'ê' | 'ë' => s.push('e'),
            'í' | 'ì' | 'î' | 'ï' => s.push('i'),
            'ú' | 'ù' | 'û' | 'ü' => s.push('u'),
            'ñ' => s.push('n'),
            _ => {}
        }
    }
    for (from, to) in [
        ("ph", "f"),
        ("ck", "k"),
        ("qu", "kv"),
        ("x", "ks"),
        ("c", "k"),
        ("q", "k"),
        ("z", "s"),
        ("y", "i"),
        ("w", "v"),
        ("th", "t"),
        ("ou", "u"),
        ("ee", "i"),
        ("oo", "u"),
        ("gh", "g"),
    ] {
        s = s.replace(from, to);
    }
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if !out.ends_with(c) {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names() -> Names {
        Names::new([
            "Fresh lamp",
            "Round lamp",
            "Gang lamp",
            "Sink",
            "Kitchen",
            "Kakkmaddafakka",
            "Bjørn Eidsvåg",
            "Dimmu Borgir",
            "Mike Oldfield",
            "Treninggg",
            "Kos",
            "Rene bangers",
            "The Neighbourhood",
            "X Ambassadors",
            "Sergey Lazarev",
            "Mild Orange",
        ])
    }

    fn best(phrase: &str) -> Option<String> {
        names().best(phrase).map(|m| m.name.to_owned())
    }

    #[test]
    fn misheard_names_are_found() {
        assert_eq!(best("turn on the round lamb").as_deref(), Some("Round lamp"));
        assert_eq!(best("turn on the sync light").as_deref(), Some("Sink"));
        assert_eq!(best("play Kakma de Faka").as_deref(), Some("Kakkmaddafakka"));
        assert_eq!(best("play Demu Borgir").as_deref(), Some("Dimmu Borgir"));
        assert_eq!(best("play my gold field").as_deref(), Some("Mike Oldfield"));
        assert_eq!(best("play my trending playlist").as_deref(), Some("Treninggg"));
        assert_eq!(best("play my Renee Banger's playlist").as_deref(), Some("Rene bangers"));
        assert_eq!(best("play the neighborhood").as_deref(), Some("The Neighbourhood"));
        assert_eq!(best("play exam ambassadors").as_deref(), Some("X Ambassadors"));
        assert_eq!(best("play Björn Eidsvog").as_deref(), Some("Bjørn Eidsvåg"));
    }

    #[test]
    fn ordinary_words_do_not_match() {
        assert_eq!(best("what's the weather in Bergen tomorrow"), None);
        assert_eq!(best("tell me a joke"), None);
        assert_eq!(best("turn off the lights"), None);
    }

    #[test]
    fn the_match_says_which_words_it_covers() {
        let names = names();
        let m = names.best("play my gold field please").unwrap();
        assert_eq!(m.words, 1..4);
    }
}
