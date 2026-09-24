//! Live weather from Open-Meteo: geocode the place name, then fetch current conditions and the
//! daily forecast. No API key needed.

use std::time::Duration;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::intent::Day;
use crate::lang::Lang;

const TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Deserialize)]
struct Place {
    name: String,
    latitude: f64,
    longitude: f64,
}

#[derive(Deserialize)]
struct Geocoding {
    #[serde(default)]
    results: Vec<Place>,
}

#[derive(Deserialize)]
struct Current {
    temperature_2m: f64,
    weather_code: u32,
    wind_speed_10m: f64,
}

#[derive(Deserialize)]
struct Daily {
    weather_code: Vec<u32>,
    temperature_2m_max: Vec<f64>,
    temperature_2m_min: Vec<f64>,
    precipitation_probability_max: Vec<Option<f64>>,
}

#[derive(Deserialize)]
struct Forecast {
    current: Current,
    daily: Daily,
}

pub struct Weather {
    agent: ureq::Agent,
}

impl Default for Weather {
    fn default() -> Self {
        Self { agent: ureq::AgentBuilder::new().timeout(TIMEOUT).build() }
    }
}

impl Weather {
    /// A sentence to speak. `Ok(None)` when the place is not found.
    pub fn report(&self, place: &str, day: Day, lang: Lang) -> Result<Option<String>> {
        let geo: Geocoding = self
            .agent
            .get("https://geocoding-api.open-meteo.com/v1/search")
            .query("name", place)
            .query("count", "1")
            .query("language", "en")
            .call()
            .context("geocoding request")?
            .into_json()?;
        let Some(place) = geo.results.into_iter().next() else {
            return Ok(None);
        };
        let forecast: Forecast = self
            .agent
            .get("https://api.open-meteo.com/v1/forecast")
            .query("latitude", &place.latitude.to_string())
            .query("longitude", &place.longitude.to_string())
            .query("current", "temperature_2m,weather_code,wind_speed_10m")
            .query("daily", "weather_code,temperature_2m_max,temperature_2m_min,precipitation_probability_max")
            .query("wind_speed_unit", "ms")
            .query("timezone", "auto")
            .query("forecast_days", "2")
            .call()
            .context("forecast request")?
            .into_json()?;
        Ok(Some(describe(&place.name, &forecast, day, lang)))
    }
}

/// Wind worth mentioning, in m/s (a "fresh breeze" and up).
const WINDY: f64 = 8.0;

/// One or two short sentences, the way a person would answer: now and the high for today, the
/// range for tomorrow. Rain only when it is at least somewhat likely, wind only when it is windy.
fn describe(name: &str, f: &Forecast, day: Day, lang: Lang) -> String {
    let i = match day {
        Day::Today => 0,
        Day::Tomorrow => 1,
    };
    let d = &f.daily;
    let (Some(&code), Some(&high), Some(&low)) =
        (d.weather_code.get(i), d.temperature_2m_max.get(i), d.temperature_2m_min.get(i))
    else {
        return match lang {
            Lang::English => format!("I got no forecast for {name}."),
            Lang::Norwegian => format!("Jeg fikk ingen værmelding for {name}."),
        };
    };
    // The condition already says rain or snow ("I morgen i Bergen: regn, ..."): no rain clause on top.
    let shown = if day == Day::Today { f.current.weather_code } else { code };
    let rain = match wet(shown) {
        true => 0.0,
        false => d.precipitation_probability_max.get(i).copied().flatten().unwrap_or(0.0),
    };
    let windy = day == Day::Today && f.current.wind_speed_10m >= WINDY;
    let (high, low) = (number(high), number(low));
    match lang {
        Lang::English => {
            let rain = match rain {
                p if p >= 60.0 => " and rain is likely",
                p if p >= 30.0 => " and a chance of rain",
                _ => "",
            };
            let wind = if windy { " It's windy." } else { "" };
            match day {
                Day::Today => format!(
                    "It's {} and {} in {name}, with a high of {high}{rain}.{wind}",
                    degrees(f.current.temperature_2m, lang),
                    condition(f.current.weather_code, lang),
                ),
                Day::Tomorrow => {
                    format!("Tomorrow in {name}: {}, {low} to {high} degrees{rain}.", condition(code, lang))
                }
            }
        }
        Lang::Norwegian => {
            let rain = match rain {
                p if p >= 60.0 => ", og det blir trolig regn",
                p if p >= 30.0 => ", og litt sjanse for regn",
                _ => "",
            };
            let wind = if windy { " Det blåser godt." } else { "" };
            match day {
                Day::Today => format!(
                    "Det er {} og {} i {name} nå, med opptil {high} grader i dag{rain}.{wind}",
                    degrees(f.current.temperature_2m, lang),
                    condition(f.current.weather_code, lang),
                ),
                Day::Tomorrow => {
                    format!("I morgen i {name}: {}, {low} til {high} grader{rain}.", condition(code, lang))
                }
            }
        }
    }
}

/// A rounded temperature without the unit, "minus 3" for negatives.
fn number(t: f64) -> String {
    let t = t.round();
    if t < 0.0 {
        format!("minus {}", -t)
    } else {
        format!("{}", t.abs())
    }
}

fn degrees(t: f64, lang: Lang) -> String {
    let t = t.round();
    // `-0` prints as "-0".
    let t = if t == 0.0 { 0.0 } else { t };
    let (minus, unit) = match lang {
        Lang::English => ("minus", if t.abs() == 1.0 { "degree" } else { "degrees" }),
        Lang::Norwegian => ("minus", if t.abs() == 1.0 { "grad" } else { "grader" }),
    };
    if t < 0.0 {
        format!("{minus} {} {unit}", -t)
    } else {
        format!("{t} {unit}")
    }
}

/// WMO weather interpretation codes.
/// Drizzle, rain, snow, showers and thunderstorms.
fn wet(code: u32) -> bool {
    matches!(code, 51..=67 | 71..=77 | 80..=86 | 95..=99)
}

fn condition(code: u32, lang: Lang) -> &'static str {
    let (en, no) = match code {
        0 => ("clear", "klart"),
        1 => ("mostly clear", "stort sett klart"),
        2 => ("partly cloudy", "delvis skyet"),
        3 => ("overcast", "overskyet"),
        45 | 48 => ("foggy", "tåke"),
        51..=57 => ("drizzle", "yr"),
        61 | 80 => ("light rain", "lett regn"),
        63 | 81 => ("rain", "regn"),
        65 | 82 => ("heavy rain", "kraftig regn"),
        66 | 67 => ("freezing rain", "underkjølt regn"),
        71 | 85 => ("light snow", "lett snø"),
        73 => ("snow", "snø"),
        75 | 86 => ("heavy snow", "kraftig snøfall"),
        77 => ("snow grains", "kornsnø"),
        95 => ("thunderstorms", "tordenvær"),
        96 | 99 => ("thunderstorms with hail", "tordenvær med hagl"),
        _ => ("mixed weather", "skiftende vær"),
    };
    match lang {
        Lang::English => en,
        Lang::Norwegian => no,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
        "current": {"temperature_2m": 11.6, "weather_code": 2, "wind_speed_10m": 3.4},
        "daily": {"weather_code": [61, 3], "temperature_2m_max": [14.2, 12.0],
                  "temperature_2m_min": [-0.4, -3.2], "precipitation_probability_max": [40, null]}
    }"#;

    #[test]
    fn describes_today_and_tomorrow() {
        let f: Forecast = serde_json::from_str(SAMPLE).unwrap();
        assert_eq!(
            describe("Oslo", &f, Day::Today, Lang::English),
            "It's 12 degrees and partly cloudy in Oslo, with a high of 14 and a chance of rain."
        );
        assert_eq!(
            describe("Oslo", &f, Day::Tomorrow, Lang::English),
            "Tomorrow in Oslo: overcast, minus 3 to 12 degrees."
        );
        assert_eq!(
            describe("Oslo", &f, Day::Today, Lang::Norwegian),
            "Det er 12 grader og delvis skyet i Oslo nå, med opptil 14 grader i dag, og litt sjanse for regn."
        );
        assert_eq!(
            describe("Oslo", &f, Day::Tomorrow, Lang::Norwegian),
            "I morgen i Oslo: overskyet, minus 3 til 12 grader."
        );
    }

    #[test]
    fn mentions_wind_only_when_windy_and_rain_only_when_likely() {
        let mut f: Forecast = serde_json::from_str(SAMPLE).unwrap();
        f.current.wind_speed_10m = 11.0;
        f.daily.precipitation_probability_max[0] = Some(80.0);
        assert_eq!(
            describe("Bergen", &f, Day::Today, Lang::English),
            "It's 12 degrees and partly cloudy in Bergen, with a high of 14 and rain is likely. It's windy."
        );
        // Rain as the condition is not repeated as a chance of rain.
        f.daily.weather_code[1] = 63;
        f.daily.precipitation_probability_max[1] = Some(90.0);
        assert_eq!(
            describe("Bergen", &f, Day::Tomorrow, Lang::Norwegian),
            "I morgen i Bergen: regn, minus 3 til 12 grader."
        );
        f.daily.precipitation_probability_max[0] = Some(10.0);
        f.current.wind_speed_10m = 2.0;
        assert_eq!(
            describe("Bergen", &f, Day::Today, Lang::English),
            "It's 12 degrees and partly cloudy in Bergen, with a high of 14."
        );
    }
}
