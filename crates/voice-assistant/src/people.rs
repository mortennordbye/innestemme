//! Where someone is, from Home Assistant's people: home, away, or the zone they are in. Names are
//! matched by sound, since the transcriber guesses their spelling ("Jon" for John).

use crate::ha::Person;
use crate::lang::Lang;
use crate::names::{self, Names};

fn first_name(person: &Person) -> &str {
    person.name.split_whitespace().next().unwrap_or(&person.name)
}

/// The person a spoken name means.
pub fn find<'a>(people: &'a [Person], name: &str) -> Option<&'a Person> {
    let names = Names::new(people.iter().map(first_name));
    let found = names.best(name)?;
    people.iter().find(|p| first_name(p) == found.name)
}

/// "Ingrid is at work." Zone names are Home Assistant's own.
fn whereabouts(person: &Person, lang: Lang) -> String {
    let who = first_name(person);
    let zone = person.state.to_lowercase();
    match (lang == Lang::Norwegian, person.state.as_str()) {
        (false, "home") => format!("{who} is home."),
        (true, "home") => format!("{who} er hjemme."),
        (false, "not_home") => format!("{who} is away."),
        (true, "not_home") => format!("{who} er borte."),
        (false, "unknown" | "unavailable") => format!("I can't tell where {who} is."),
        (true, "unknown" | "unavailable") => format!("Jeg vet ikke hvor {who} er."),
        (false, _) => format!("{who} is at {zone}."),
        (true, _) => format!("{who} er på {zone}."),
    }
}

/// Whether a zone as said is the zone a person is in: "work" and "Work", "the office" and "Office".
fn in_zone(person: &Person, zone: &str) -> bool {
    let said = names::sound_key(zone.trim_start_matches("the "));
    names::similarity(&names::sound_key(&person.state), &said) >= 0.75
}

/// The answer to "where is Ingrid", "is Ingrid at work" (`zone`) or "when will Ingrid be home"
/// (`arriving`). Home Assistant has no arrival times, so that says where they are.
pub fn where_answer(people: &[Person], name: &str, zone: Option<&str>, arriving: bool, lang: Lang) -> String {
    let no = lang == Lang::Norwegian;
    let Some(person) = find(people, name) else {
        // The parser lowercases: "kari" is said as Kari.
        let name: Vec<String> = name
            .split_whitespace()
            .map(|w| {
                let mut chars = w.chars();
                chars.next().map(|c| c.to_uppercase().chain(chars).collect()).unwrap_or_default()
            })
            .collect();
        let name = name.join(" ");
        return if no { format!("Jeg vet ikke hvor {name} er.") } else { format!("I don't track {name}.") };
    };
    let who = first_name(person);
    let home = person.state == "home";
    if arriving {
        return match (home, no) {
            (true, false) => format!("{who} is already home."),
            (true, true) => format!("{who} er allerede hjemme."),
            // The fixed sentence on its own, so it is rendered once.
            (false, false) => format!("{} I don't have an arrival time.", whereabouts(person, lang)),
            (false, true) => format!("{} Jeg har ingen ankomsttid.", whereabouts(person, lang)),
        };
    }
    match zone {
        Some(zone) if in_zone(person, zone) => {
            format!("{} {}", if no { "Ja," } else { "Yes," }, whereabouts(person, lang))
        }
        Some(_) => format!("{} {}", if no { "Nei," } else { "No," }, whereabouts(person, lang)),
        None => whereabouts(person, lang),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn people() -> Vec<Person> {
        let person = |name: &str, state: &str| Person { name: name.into(), state: state.into() };
        vec![person("Torsten Berg", "Work"), person("Ingrid Berg", "home"), person("Ola", "not_home")]
    }

    fn say(name: &str, zone: Option<&str>, arriving: bool) -> String {
        where_answer(&people(), name, zone, arriving, Lang::English)
    }

    #[test]
    fn says_where_someone_is() {
        assert_eq!(say("torsten", None, false), "Torsten is at work.");
        // Misheard, as the transcriber writes it.
        assert_eq!(say("torston", None, false), "Torsten is at work.");
        assert_eq!(say("ingrid", None, false), "Ingrid is home.");
        assert_eq!(say("ola", None, false), "Ola is away.");
        assert_eq!(say("kari", None, false), "I don't track Kari.");
        assert_eq!(where_answer(&people(), "torsten", None, false, Lang::Norwegian), "Torsten er på work.");
    }

    #[test]
    fn answers_whether_someone_is_in_a_zone() {
        assert_eq!(say("torsten", Some("work"), false), "Yes, Torsten is at work.");
        assert_eq!(say("ingrid", Some("work"), false), "No, Ingrid is home.");
        assert_eq!(say("ola", Some("the office"), false), "No, Ola is away.");
    }

    #[test]
    fn has_no_arrival_times() {
        assert_eq!(say("torsten", None, true), "Torsten is at work. I don't have an arrival time.");
        assert_eq!(say("ingrid", None, true), "Ingrid is already home.");
    }
}
