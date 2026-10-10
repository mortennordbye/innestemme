//! "How far is it from Oslo to Shanghai?": the distance as the crow flies between two places, or
//! from home. Places are found like the weather's; a country counts from its middle.

use crate::geo::Location;
use crate::lang::Lang;

const EARTH_RADIUS_KM: f64 = 6371.0;

/// `from` is `None` for "from home".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Query {
    pub from: Option<String>,
    pub to: String,
}

/// The places in a distance question, if `text` is one.
pub fn parse(text: &str) -> Option<Query> {
    let clean: String =
        text.chars().map(|c| if c.is_alphanumeric() || c == ' ' || c == '-' || c == '\'' { c } else { ' ' }).collect();
    let lower = clean.to_lowercase();
    let words: Vec<&str> = lower.split_whitespace().collect();
    let original: Vec<&str> = clean.split_whitespace().collect();
    let joined = words.join(" ");
    let asks = [
        "how far",
        "how long is it from",
        "how many kilometres",
        "how many kilometers",
        "how many miles",
        "distance",
        "hvor langt",
        "avstand",
    ];
    if !asks.iter().any(|a| joined.contains(a)) {
        return None;
    }
    let at = |w: &[&str]| words.iter().position(|x| w.contains(x));
    let span = |from: usize, to: usize| -> Option<String> {
        let place: Vec<&str> =
            original.get(from..to)?.iter().copied().filter(|w| !FILLER.contains(&w.to_lowercase().as_str())).collect();
        (!place.is_empty()).then(|| place.join(" "))
    };
    // "from X to Y", "fra X til Y"
    if let (Some(f), Some(t)) = (at(&["from", "fra"]), at(&["to", "til"])) {
        if f < t {
            return Some(Query { from: span(f + 1, t), to: span(t + 1, words.len())? });
        }
        // "how far is Bergen from Oslo"
        return Some(Query { from: span(f + 1, words.len()), to: span(at(&["is", "er"])? + 1, f)? });
    }
    // "how far is X from here", "how far away is Tokyo", "hvor langt er det til Bergen"
    if let Some(f) = at(&["from", "fra"]) {
        return Some(Query { from: span(f + 1, words.len()), to: span(at(&["is", "er"])? + 1, f)? });
    }
    if let Some(t) = at(&["to", "til"]) {
        return Some(Query { from: None, to: span(t + 1, words.len())? });
    }
    let is = at(&["is", "er"])?;
    Some(Query { from: None, to: span(is + 1, words.len())? })
}

/// Words around the places that are not part of them.
const FILLER: &[&str] = &[
    "it",
    "the",
    "away",
    "here",
    "home",
    "us",
    "me",
    "det",
    "hit",
    "hjemme",
    "herfra",
    "unna",
    "is",
    "er",
    "in",
    "km",
    "kilometres",
    "kilometers",
    "miles",
];

/// Great-circle distance in kilometres.
pub fn kilometres(a: &Location, b: &Location) -> f64 {
    let (la1, la2) = (a.latitude.to_radians(), b.latitude.to_radians());
    let dla = la2 - la1;
    let dlo = (b.longitude - a.longitude).to_radians();
    let h = (dla / 2.0).sin().powi(2) + la1.cos() * la2.cos() * (dlo / 2.0).sin().powi(2);
    2.0 * EARTH_RADIUS_KM * h.sqrt().asin()
}

/// "From Oslo to Shanghai is about 7,900 kilometres as the crow flies."
pub fn answer(from: &Location, to: &Location, lang: Lang) -> String {
    let km = rounded(kilometres(from, to));
    if lang == Lang::Norwegian {
        let number = group(km, ' ');
        format!("Fra {} til {} er det omtrent {number} kilometer i luftlinje.", from.label, to.label)
    } else {
        let number = group(km, ',');
        format!("From {} to {} is about {number} kilometres as the crow flies.", from.label, to.label)
    }
}

/// Two significant figures above 100 km, whole kilometres below.
fn rounded(km: f64) -> u64 {
    match km {
        k if k >= 10_000.0 => (k / 1000.0).round() as u64 * 1000,
        k if k >= 1000.0 => (k / 100.0).round() as u64 * 100,
        k if k >= 100.0 => (k / 10.0).round() as u64 * 10,
        k => k.round().max(1.0) as u64,
    }
}

fn group(n: u64, separator: char) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(separator);
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(from: Option<&str>, to: &str) -> Option<Query> {
        Some(Query { from: from.map(str::to_owned), to: to.to_owned() })
    }

    #[test]
    fn finds_the_places() {
        assert_eq!(parse("How long is it from Oslo to Shanghai?"), q(Some("Oslo"), "Shanghai"));
        assert_eq!(parse("how far is it from Bergen to New York"), q(Some("Bergen"), "New York"));
        assert_eq!(parse("How far is Tromsø from Oslo?"), q(Some("Oslo"), "Tromsø"));
        assert_eq!(parse("how far away is Tokyo"), q(None, "Tokyo"));
        assert_eq!(parse("what's the distance to Paris"), q(None, "Paris"));
        assert_eq!(parse("Hvor langt er det fra Oslo til Bergen?"), q(Some("Oslo"), "Bergen"));
        assert_eq!(parse("hvor langt er det til Trondheim"), q(None, "Trondheim"));
        assert_eq!(parse("turn on the lights"), None);
    }

    #[test]
    fn speaks_a_round_distance() {
        let oslo = Location { latitude: 59.91, longitude: 10.75, label: "Oslo".into() };
        let shanghai = Location { latitude: 31.22, longitude: 121.46, label: "Shanghai".into() };
        let bergen = Location { latitude: 60.39, longitude: 5.32, label: "Bergen".into() };
        assert_eq!(
            answer(&oslo, &shanghai, Lang::English),
            "From Oslo to Shanghai is about 8,100 kilometres as the crow flies."
        );
        assert_eq!(
            answer(&oslo, &bergen, Lang::Norwegian),
            "Fra Oslo til Bergen er det omtrent 310 kilometer i luftlinje."
        );
        assert_eq!(group(12_000, ','), "12,000");
    }
}
