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
    ureq::AgentBuilder::new().timeout(TIMEOUT).user_agent(user_agent).build()
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
        .set("ET-Client-Name", ENTUR_CLIENT)
        .query("text", address)
        .query("size", "1")
        .query("layers", "address,venue")
        .query("lang", "no")
        .call()
        .context("address lookup (Entur geocoder)")?
        .into_json()?;
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

/// A place by name. A Norwegian place with that exact name wins over a bigger one abroad, so a
/// misheard "Verden" does not end up in Germany when Norway has none, and "Bergen" is Bergen.
pub fn place(agent: &ureq::Agent, name: &str) -> Result<Option<Location>> {
    let search = |country: Option<&str>| -> Result<Vec<Place>> {
        let mut call = agent
            .get("https://geocoding-api.open-meteo.com/v1/search")
            .query("name", name)
            .query("count", "5")
            .query("language", "en");
        if let Some(country) = country {
            call = call.query("countryCode", country);
        }
        let geo: Geocoding = call.call().context("place lookup (Open-Meteo geocoder)")?.into_json()?;
        Ok(geo.results)
    };
    let same = |p: &Place| p.name.to_lowercase() == name.trim().to_lowercase();
    let norway = search(Some("NO"))?;
    let chosen = match norway.into_iter().find(same) {
        Some(place) => Some(place),
        None => search(None)?.into_iter().next(),
    };
    Ok(chosen.map(|p| Location { latitude: p.latitude, longitude: p.longitude, label: p.name }))
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
}
