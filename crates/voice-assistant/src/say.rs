//! Small pieces of spoken wording that many skills share.

use jiff::tz::TimeZone;
use jiff::Timestamp;

use crate::lang::Lang;

/// The first letter in upper case, the rest as given: "the kitchen timer" -> "The kitchen timer".
pub fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map_or_else(String::new, |c| c.to_uppercase().chain(chars).collect())
}

/// "milk", "milk and eggs", "milk, eggs and bread"; "melk og egg" in Norwegian.
pub fn join(items: &[String], lang: Lang) -> String {
    let and = if lang == Lang::Norwegian { "og" } else { "and" };
    match items {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} {and} {last}", rest.join(", ")),
    }
}

/// The clock time of a moment in a time zone, as "07:05".
pub fn hh_mm(at: Timestamp, tz: &TimeZone) -> String {
    at.to_zoned(tz.clone()).strftime("%H:%M").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn items(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn capitalizes_the_first_letter() {
        assert_eq!(capitalize(""), "");
        assert_eq!(capitalize("ørret"), "Ørret");
        assert_eq!(capitalize("the kitchen timer"), "The kitchen timer");
    }

    #[test]
    fn joins_a_list() {
        assert_eq!(join(&[], Lang::English), "");
        assert_eq!(join(&items(&["milk"]), Lang::English), "milk");
        assert_eq!(join(&items(&["milk", "eggs", "bread"]), Lang::English), "milk, eggs and bread");
        assert_eq!(join(&items(&["melk", "egg"]), Lang::Norwegian), "melk og egg");
    }

    #[test]
    fn clock_time() {
        let at: Timestamp = "2026-01-02T06:05:00Z".parse().unwrap();
        assert_eq!(hh_mm(at, &TimeZone::UTC), "06:05");
    }
}
