//! Where things are: the home from an address or coordinates (Entur's geocoder, Norwegian
//! addresses), and places by name (Open-Meteo's geocoder, places in Norway first).

use std::time::Duration;

use anyhow::{Context, Result};
use serde::Deserialize;

const TIMEOUT: Duration = Duration::from_secs(5);
/// Entur asks every client to name itself.
pub const ENTUR_CLIENT: &str = "innestemme";

#[derive(Debug, Clone, PartialEq)]
pub struct Location {
    pub latitude: f64,
    pub longitude: f64,
    /// What to call it in an answer: "Oslo".
    pub label: String,
}

pub fn agent(user_agent: &str) -> ureq::Agent {
    ureq::Agent::config_builder().timeout_global(Some(TIMEOUT)).user_agent(user_agent).build().into()
}

/// "59.9133, 10.7403" or "59.9133 10.7403".
pub fn parse_coordinates(text: &str) -> Option<(f64, f64)> {
    let mut parts = text.split([',', ' ']).map(str::trim).filter(|p| !p.is_empty());
    let (lat, lon) = (parts.next()?.parse().ok()?, parts.next()?.parse().ok()?);
    (parts.next().is_none() && (-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon)).then_some((lat, lon))
}

/// The home: coordinates as given, else the address looked up with Entur's geocoder. `label`
/// names it in answers; without one, the address's town.
pub fn home(agent: &ureq::Agent, address: &str, label: Option<&str>) -> Result<Option<Location>> {
    if let Some((latitude, longitude)) = parse_coordinates(address) {
        let label = label.unwrap_or("home").to_owned();
        return Ok(Some(Location { latitude, longitude, label }));
    }
    let found: Features = agent
        .get("https://api.entur.io/geocoder/v1/search")
        .header("ET-Client-Name", ENTUR_CLIENT)
        .query("text", address)
        .query("size", "1")
        .query("layers", "address,venue")
        .query("lang", "no")
        .call()
        .context("address lookup (Entur geocoder)")?
        .body_mut()
        .read_json()?;
    Ok(found.features.into_iter().next().map(|f| Location {
        longitude: f.geometry.coordinates[0],
        latitude: f.geometry.coordinates[1],
        label: label.map(str::to_owned).or(f.properties.locality).unwrap_or(f.properties.label),
    }))
}

#[derive(Deserialize)]
struct Features {
    #[serde(default)]
    features: Vec<Feature>,
}

#[derive(Deserialize)]
struct Feature {
    geometry: Geometry,
    properties: Properties,
}

#[derive(Deserialize)]
struct Geometry {
    coordinates: [f64; 2],
}

#[derive(Deserialize)]
struct Properties {
    label: String,
    locality: Option<String>,
}

#[derive(Deserialize)]
struct Geocoding {
    #[serde(default)]
    results: Vec<Place>,
}

#[derive(Deserialize)]
struct Place {
    name: String,
    latitude: f64,
    longitude: f64,
}

/// Larger Norwegian towns, asked about often enough to know without a lookup and to match a
/// misheard name against.
const TOWNS: &[(&str, f64, f64)] = &[
    ("Oslo", 59.91, 10.75),
    ("Bergen", 60.39, 5.32),
    ("Trondheim", 63.43, 10.40),
    ("Stavanger", 58.97, 5.73),
    ("Drammen", 59.74, 10.20),
    ("Fredrikstad", 59.22, 10.93),
    ("Kristiansand", 58.15, 8.00),
    ("Sandnes", 58.85, 5.74),
    ("Tromsø", 69.65, 18.96),
    ("Sarpsborg", 59.28, 11.11),
    ("Skien", 59.21, 9.61),
    ("Ålesund", 62.47, 6.15),
    ("Sandefjord", 59.13, 10.22),
    ("Haugesund", 59.41, 5.27),
    ("Tønsberg", 59.27, 10.41),
    ("Moss", 59.43, 10.66),
    ("Porsgrunn", 59.14, 9.66),
    ("Bodø", 67.28, 14.40),
    ("Arendal", 58.46, 8.77),
    ("Hamar", 60.79, 11.07),
    ("Larvik", 59.05, 10.04),
    ("Halden", 59.12, 11.39),
    ("Lillehammer", 61.12, 10.47),
    ("Molde", 62.74, 7.16),
    ("Kongsberg", 59.67, 9.65),
    ("Gjøvik", 60.80, 10.69),
    ("Harstad", 68.80, 16.54),
    ("Narvik", 68.44, 17.43),
    ("Alta", 69.97, 23.27),
    ("Kristiansund", 63.11, 7.73),
    ("Lillestrøm", 59.96, 11.05),
    ("Asker", 59.83, 10.44),
    ("Sandvika", 59.89, 10.52),
    ("Hammerfest", 70.66, 23.68),
];

/// What the transcriber has written for a town, from the engine's logs. Too far from the name for
/// the sound-alike check, which would also take common words at that distance.
const HEARD_AS: &[(&str, &[&str])] = &[("Oslo", &["us of", "all slow", "as low", "oz low", "os low", "oslow"])];

/// A place known without a lookup: the home by its name, a town, or what a town is heard as.
fn known(name: &str, home: Option<&Location>) -> Option<Location> {
    let name = name.trim().to_lowercase();
    if let Some(home) = home.filter(|h| h.label.to_lowercase() == name) {
        return Some(home.clone());
    }
    let town = HEARD_AS.iter().find(|(_, heard)| heard.contains(&name.as_str())).map(|(town, _)| *town);
    let town = town.unwrap_or(&name);
    TOWNS.iter().find(|(t, ..)| t.to_lowercase() == town.to_lowercase()).map(|&(label, latitude, longitude)| Location {
        latitude,
        longitude,
        label: label.to_owned(),
    })
}

/// The home or a town the whole name sounds like ("Bergan", "Tronheim").
fn sounds_like(name: &str, home: Option<&Location>) -> Option<Location> {
    let names = crate::names::Names::new(home.map(|h| h.label.as_str()).into_iter().chain(TOWNS.iter().map(|t| t.0)));
    let found = names.best(name)?;
    // A town in part of the name ("Moss" in "Mossel Bay") is not the place asked for.
    (found.words == (0..name.split_whitespace().count())).then(|| known(found.name, home)).flatten()
}

/// Whether a name is a town known without a lookup, or sounds like one ("Bergan").
pub fn is_known_place(name: &str) -> bool {
    offline(name, None).is_some()
}

/// A place found without the network: the home, a town, or what either sounds like.
pub fn offline(name: &str, home: Option<&Location>) -> Option<Location> {
    known(name, home).or_else(|| sounds_like(name, home))
}

/// The Norwegian place, else the home or a town it sounds like, else the best place anywhere.
/// A place abroad with exactly the name asked for beats a sound-alike: "Malta" is not Alta, nor
/// "Lerwick" Larvik.
fn pick(
    name: &str,
    norway: Vec<Place>,
    world: impl FnOnce() -> Result<Vec<Place>>,
    home: Option<&Location>,
) -> Result<Option<Location>> {
    let same = |p: &Place| p.name.to_lowercase() == name.trim().to_lowercase();
    let location = |p: Place| Location { latitude: p.latitude, longitude: p.longitude, label: p.name };
    if let Some(place) = norway.into_iter().find(same) {
        return Ok(Some(location(place)));
    }
    let world = world()?;
    if let Some(place) = world.iter().position(same) {
        return Ok(Some(location(world.into_iter().nth(place).expect("found above"))));
    }
    if let Some(alike) = sounds_like(name, home) {
        return Ok(Some(alike));
    }
    Ok(world.into_iter().next().map(location))
}

/// A place by name. The home, a larger town or a known mishearing of one needs no lookup. Then
/// a Norwegian place with that exact name wins over a bigger one abroad, so a misheard "Verden"
/// does not end up in Germany when Norway has none; then a place anywhere with that exact name;
/// then the home or a town it sounds like; then the best place anywhere.
pub fn place(agent: &ureq::Agent, name: &str, home: Option<&Location>) -> Result<Option<Location>> {
    if let Some(known) = known(name, home) {
        return Ok(Some(known));
    }
    let search = |country: Option<&str>| -> Result<Vec<Place>> {
        let mut call = agent
            .get("https://geocoding-api.open-meteo.com/v1/search")
            .query("name", name)
            .query("count", "5")
            .query("language", "en");
        if let Some(country) = country {
            call = call.query("countryCode", country);
        }
        let geo: Geocoding = call.call().context("place lookup (Open-Meteo geocoder)")?.body_mut().read_json()?;
        Ok(geo.results)
    };
    pick(name, search(Some("NO"))?, || search(None), home)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coordinates() {
        assert_eq!(parse_coordinates("59.9133, 10.7403"), Some((59.9133, 10.7403)));
        assert_eq!(parse_coordinates("59.9133 10.7403"), Some((59.9133, 10.7403)));
        assert_eq!(parse_coordinates("Karl Johans gate 22, Oslo"), None);
        assert_eq!(parse_coordinates("95, 10"), None);
    }

    fn label(found: Option<Location>) -> Option<String> {
        found.map(|l| l.label)
    }

    #[test]
    fn towns_and_their_mishearings_need_no_lookup() {
        assert_eq!(label(known("Bergen", None)).as_deref(), Some("Bergen"));
        assert_eq!(label(known("tromsø", None)).as_deref(), Some("Tromsø"));
        // "How is the weather in us of tomorrow?", "... at all slow tomorrow?"
        assert_eq!(label(known("us of", None)).as_deref(), Some("Oslo"));
        assert_eq!(label(known("all slow", None)).as_deref(), Some("Oslo"));
        assert_eq!(known("Hamburg", None), None);
        let home = Location { latitude: 59.95, longitude: 10.77, label: "Nydalen".into() };
        assert_eq!(known("nydalen", Some(&home)), Some(home));
    }

    #[test]
    fn sound_alikes_of_the_home_and_towns() {
        let home = Location { latitude: 59.95, longitude: 10.77, label: "Nydalen".into() };
        assert_eq!(label(sounds_like("bergan", None)).as_deref(), Some("Bergen"));
        assert_eq!(label(sounds_like("tronheim", None)).as_deref(), Some("Trondheim"));
        assert_eq!(label(sounds_like("stavangar", None)).as_deref(), Some("Stavanger"));
        assert_eq!(label(sounds_like("nydal'n", Some(&home))).as_deref(), Some("Nydalen"));
        assert_eq!(sounds_like("new york", None), None);
        assert_eq!(sounds_like("london", None), None);
        assert_eq!(sounds_like("mossel bay", None), None);
    }

    fn found(name: &str, latitude: f64) -> Place {
        Place { name: name.into(), latitude, longitude: 0.0 }
    }

    #[test]
    fn a_place_abroad_by_its_own_name_beats_a_sound_alike() {
        let pick =
            |name: &str, norway: Vec<Place>, world: Vec<Place>| label(pick(name, norway, || Ok(world), None).unwrap());
        assert_eq!(pick("Malta", vec![], vec![found("Malta", 35.9)]).as_deref(), Some("Malta"));
        assert_eq!(pick("Lerwick", vec![], vec![found("Lerwick", 60.2)]).as_deref(), Some("Lerwick"));
        // Misheard: nothing abroad by that name, so the town it sounds like.
        assert_eq!(pick("Bergan", vec![], vec![found("Bergamo", 45.7)]).as_deref(), Some("Bergen"));
        // A Norwegian place by the name still wins, without asking abroad.
        let verden = super::pick("Verden", vec![found("Verden", 63.0)], || panic!("not asked"), None).unwrap();
        assert_eq!(verden.map(|l| l.latitude), Some(63.0));
        assert!(is_known_place("bergen"));
        assert!(!is_known_place("kitchen"));
    }
}
