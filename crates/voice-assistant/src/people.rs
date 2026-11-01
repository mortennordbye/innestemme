//! Where someone is, from Home Assistant's people: home, away, or the zone they are in. Names are
//! matched by sound, since the transcriber guesses their spelling ("Jon" for John).

use crate::distance;
use crate::geo::{self, Location};
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

/// Zone words that mean the same place, in either language: "er Ola på jobb" when Home
/// Assistant's zone is "Work".
const ZONE_SYNONYMS: &[&[&str]] = &[
    &["work", "jobb", "jobben", "arbeid"],
    &["office", "kontor", "kontoret"],
    &["school", "skole", "skolen"],
    &["gym", "trening", "treningssenter", "treningssenteret", "treningen"],
    &["kindergarten", "nursery", "barnehage", "barnehagen"],
    &["university", "uni", "universitet", "universitetet"],
];

/// Whether a zone as said is the zone a person is in: "work" and "Work", "the office" and "Office",
/// "jobb" and "Work".
fn in_zone(person: &Person, zone: &str) -> bool {
    let said = zone.trim_start_matches("the ").trim().to_lowercase();
    let state = person.state.to_lowercase();
    let group = |word: &str| ZONE_SYNONYMS.iter().position(|g| g.contains(&word));
    if group(&said).is_some() && group(&said) == group(&state) {
        return true;
    }
    names::similarity(&names::sound_key(&person.state), &names::sound_key(&said)) >= 0.75
}

/// The answer to "where is Ingrid", "is Ingrid at work" (`zone`) or "when will Ingrid be home"
/// (`arriving`). Home Assistant has no arrival times, so that says where they are. A name that is
/// no one's but a town ("where is Bergen") gets its distance from `home`.
pub fn where_answer(
    people: &[Person],
    name: &str,
    zone: Option<&str>,
    arriving: bool,
    home: Option<&Location>,
    lang: Lang,
) -> String {
    let no = lang == Lang::Norwegian;
    let Some(person) = find(people, name) else {
        // Only a town by its exact name: the geocoder finds a village for almost any name ("Kari"),
        // and a sound-alike would make "Malta" or "Alma" Alta.
        if let Some(place) = (zone.is_none() && !arriving).then(|| geo::known(name, home)).flatten() {
            return match home {
                Some(home) if home.label == place.label && no => format!("{} er hjemme.", place.label),
                Some(home) if home.label == place.label => format!("{} is home.", place.label),
                // The towns known offline are all Norwegian.
                Some(home) if no => {
                    format!("{} er en by i Norge. {}", place.label, distance::answer(home, &place, lang))
                }
                Some(home) => format!("{} is a town in Norway. {}", place.label, distance::answer(home, &place, lang)),
                None if no => format!("{} er en by i Norge.", place.label),
                None => format!("{} is a town in Norway.", place.label),
            };
        }
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
    let unknown = ["unknown", "unavailable"].contains(&person.state.as_str());
    match zone {
        // Neither yes nor no when the phone does not say.
        Some(_) if unknown => whereabouts(person, lang),
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
        where_answer(&people(), name, zone, arriving, None, Lang::English)
    }

    #[test]
    fn says_where_someone_is() {
        assert_eq!(say("torsten", None, false), "Torsten is at work.");
        // Misheard, as the transcriber writes it.
        assert_eq!(say("torston", None, false), "Torsten is at work.");
        assert_eq!(say("ingrid", None, false), "Ingrid is home.");
        assert_eq!(say("ola", None, false), "Ola is away.");
        assert_eq!(say("kari", None, false), "I don't track Kari.");
        assert_eq!(where_answer(&people(), "torsten", None, false, None, Lang::Norwegian), "Torsten er på work.");
    }

    #[test]
    fn answers_whether_someone_is_in_a_zone() {
        assert_eq!(say("torsten", Some("work"), false), "Yes, Torsten is at work.");
        assert_eq!(say("ingrid", Some("work"), false), "No, Ingrid is home.");
        assert_eq!(say("ola", Some("the office"), false), "No, Ola is away.");
        // Norwegian words for an English zone, and the reverse.
        assert_eq!(
            where_answer(&people(), "torsten", Some("jobb"), false, None, Lang::Norwegian),
            "Ja, Torsten er på work."
        );
        // Work and office are kept apart: a zone called Office is not Work.
        assert_eq!(say("torsten", Some("the office"), false), "No, Torsten is at work.");
        let jobb = vec![Person { name: "Kari".into(), state: "Jobb".into() }];
        assert_eq!(where_answer(&jobb, "kari", Some("work"), false, None, Lang::English), "Yes, Kari is at jobb.");
        // Neither yes nor no when Home Assistant cannot tell.
        let lost = vec![Person { name: "Kari".into(), state: "unknown".into() }];
        assert_eq!(
            where_answer(&lost, "kari", Some("work"), false, None, Lang::English),
            "I can't tell where Kari is."
        );
    }

    #[test]
    fn has_no_arrival_times() {
        assert_eq!(say("torsten", None, true), "Torsten is at work. I don't have an arrival time.");
        assert_eq!(say("ingrid", None, true), "Ingrid is already home.");
    }

    #[test]
    fn a_town_is_not_a_person() {
        let home = Location { latitude: 59.95, longitude: 10.77, label: "Nydalen".into() };
        let answer = where_answer(&people(), "bergen", None, false, Some(&home), Lang::English);
        assert!(answer.starts_with("Bergen is a town in Norway. From Nydalen to Bergen is about "), "{answer}");
        assert_eq!(say("bergen", None, false), "Bergen is a town in Norway.");
        // Someone unknown is still no one tracked; nothing is looked up for them.
        assert_eq!(say("kari", None, false), "I don't track Kari.");
        // A name with a zone, no one tracked.
        assert_eq!(say("kari", Some("work"), false), "I don't track Kari.");
        // Only a town by its exact name: not Alta.
        assert_eq!(say("malta", None, false), "I don't track Malta.");
        assert_eq!(say("alma", None, false), "I don't track Alma.");
        assert_eq!(say("tromso", None, false), "Tromsø is a town in Norway.");
    }
}
