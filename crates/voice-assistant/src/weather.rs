//! Weather from Yr (MET Norway's Locationforecast): current conditions and the forecast for
//! today and tomorrow, turned into one short sentence. No key; MET asks for an identifying
//! User-Agent and for callers not to ask more often than the forecast changes.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use jiff::tz::TimeZone;
use serde::Deserialize;

use crate::geo::{self, Location};
use crate::intent::Day;
use crate::lang::Lang;

/// MET updates the forecast about hourly; answers within this reuse the last one.
const CACHE: Duration = Duration::from_secs(600);

pub struct Weather {
    agent: ureq::Agent,
    cache: HashMap<(i64, i64), (Instant, Met)>,
}

impl Weather {
    /// `user_agent` names the application and, ideally, a contact (MET's terms).
    pub fn new(user_agent: &str) -> Self {
        Self { agent: geo::agent(user_agent), cache: HashMap::new() }
    }

    /// A place by name; `Ok(None)` when there is none.
    pub fn find(&self, name: &str) -> Result<Option<Location>> {
        geo::place(&self.agent, name)
    }

    /// A sentence to speak about `place`.
    pub fn report(&mut self, place: &Location, day: Day, lang: Lang) -> Result<String> {
        // MET wants at most four decimals; two (about a kilometre) also make a good cache key.
        let key = ((place.latitude * 100.0).round() as i64, (place.longitude * 100.0).round() as i64);
        if !self.cache.get(&key).is_some_and(|(at, _)| at.elapsed() < CACHE) {
            let met: Met = self
                .agent
                .get("https://api.met.no/weatherapi/locationforecast/2.0/complete")
                .query("lat", format!("{:.2}", key.0 as f64 / 100.0))
                .query("lon", format!("{:.2}", key.1 as f64 / 100.0))
                .call()
                .context("forecast request (MET Norway)")?
                .body_mut()
                .read_json()
                .context("reading the forecast")?;
            self.cache.insert(key, (Instant::now(), met));
        }
        let met = &self.cache[&key].1;
        Ok(match forecast(met, jiff::Timestamp::now(), &TimeZone::system()) {
            Some(f) => describe(&place.label, &f, day, lang),
            None => no_forecast(&place.label, lang),
        })
    }
}

// --- MET's format ------------------------------------------------------------------------------

#[derive(Deserialize)]
struct Met {
    properties: MetProperties,
}

#[derive(Deserialize)]
struct MetProperties {
    timeseries: Vec<Step>,
}

#[derive(Deserialize)]
struct Step {
    time: jiff::Timestamp,
    data: StepData,
}

#[derive(Deserialize)]
struct StepData {
    instant: Now,
    next_1_hours: Option<Period>,
    next_6_hours: Option<Period>,
    next_12_hours: Option<Period>,
}

#[derive(Deserialize)]
struct Now {
    details: NowDetails,
}

#[derive(Deserialize)]
struct NowDetails {
    air_temperature: Option<f64>,
    wind_speed: Option<f64>,
}

#[derive(Deserialize)]
struct Period {
    summary: Summary,
    details: Option<PeriodDetails>,
}

#[derive(Deserialize)]
struct Summary {
    symbol_code: String,
}

#[derive(Deserialize)]
struct PeriodDetails {
    probability_of_precipitation: Option<f64>,
}

impl StepData {
    fn symbol(&self) -> Option<&str> {
        [&self.next_1_hours, &self.next_6_hours, &self.next_12_hours]
            .into_iter()
            .flatten()
            .map(|p| p.summary.symbol_code.as_str())
            .next()
    }

    fn rain_chance(&self) -> Option<f64> {
        [&self.next_1_hours, &self.next_6_hours]
            .into_iter()
            .flatten()
            .find_map(|p| p.details.as_ref()?.probability_of_precipitation)
    }
}

/// MET symbol codes ("lightrainshowers_day") as the WMO codes `describe` speaks. 68 stands for
/// sleet, which WMO's short list lacks.
fn symbol_to_code(symbol: &str) -> u32 {
    let base = symbol.split('_').next().unwrap_or(symbol);
    let base = base.strip_suffix("andthunder").map(|_| "thunder").unwrap_or(base);
    match base {
        "clearsky" => 0,
        "fair" => 1,
        "partlycloudy" => 2,
        "cloudy" => 3,
        "fog" => 45,
        "lightrain" => 61,
        "rain" => 63,
        "heavyrain" => 65,
        "lightrainshowers" => 80,
        "rainshowers" => 81,
        "heavyrainshowers" => 82,
        "lightsleet" | "sleet" | "heavysleet" | "lightsleetshowers" | "sleetshowers" | "heavysleetshowers" => 68,
        "lightsnow" | "lightsnowshowers" => 71,
        "snow" | "snowshowers" => 73,
        "heavysnow" | "heavysnowshowers" => 75,
        "thunder" => 95,
        _ => 3,
    }
}

/// Now, and today and tomorrow in local time, from MET's hourly steps.
fn forecast(met: &Met, now: jiff::Timestamp, tz: &TimeZone) -> Option<Forecast> {
    let steps = &met.properties.timeseries;
    // The step for this hour: the last one that has started.
    let current = steps.iter().rev().find(|s| s.time <= now).or(steps.first())?;
    let hour = jiff::SignedDuration::from_hours(1);
    let today = now.to_zoned(tz.clone()).date();
    let mut daily = Daily {
        weather_code: vec![],
        temperature_2m_max: vec![],
        temperature_2m_min: vec![],
        precipitation_probability_max: vec![],
    };
    for date in [today, today.tomorrow().ok()?] {
        let day: Vec<&Step> =
            steps.iter().filter(|s| s.time.to_zoned(tz.clone()).date() == date && s.time + hour > now).collect();
        let temps: Vec<f64> = day.iter().filter_map(|s| s.data.instant.details.air_temperature).collect();
        if temps.is_empty() {
            break;
        }
        // The day's weather: the afternoon's six hours, else the morning's, else the first step.
        let at = |h: i8| day.iter().find(|s| s.time.to_zoned(tz.clone()).hour() == h);
        let symbol = at(12)
            .or_else(|| at(6))
            .or(day.first())
            .and_then(|s| s.data.next_6_hours.as_ref().map(|p| p.summary.symbol_code.as_str()).or(s.data.symbol()));
        daily.weather_code.push(symbol.map_or(3, symbol_to_code));
        daily.temperature_2m_max.push(temps.iter().copied().fold(f64::MIN, f64::max));
        daily.temperature_2m_min.push(temps.iter().copied().fold(f64::MAX, f64::min));
        daily.precipitation_probability_max.push(day.iter().filter_map(|s| s.data.rain_chance()).reduce(f64::max));
    }
    Some(Forecast {
        current: Current {
            temperature_2m: current.data.instant.details.air_temperature?,
            weather_code: current.data.symbol().map_or(3, symbol_to_code),
            wind_speed_10m: current.data.instant.details.wind_speed.unwrap_or(0.0),
        },
        daily,
    })
}

// --- What is said ------------------------------------------------------------------------------

struct Current {
    temperature_2m: f64,
    weather_code: u32,
    wind_speed_10m: f64,
}

struct Daily {
    weather_code: Vec<u32>,
    temperature_2m_max: Vec<f64>,
    temperature_2m_min: Vec<f64>,
    precipitation_probability_max: Vec<Option<f64>>,
}

struct Forecast {
    current: Current,
    daily: Daily,
}

fn no_forecast(name: &str, lang: Lang) -> String {
    match lang {
        Lang::English => format!("I got no forecast for {name}."),
        Lang::Norwegian => format!("Jeg fikk ingen værmelding for {name}."),
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
        return no_forecast(name, lang);
    };
    // The condition already says rain or snow ("I morgen i Bergen: regn, ..."): no rain clause on top.
    let shown = if day == Day::Today { f.current.weather_code } else { code };
    let rain = match wet(shown) {
        true => 0.0,
        false => d.precipitation_probability_max.get(i).copied().flatten().unwrap_or(0.0),
    };
    let windy = day == Day::Today && f.current.wind_speed_10m >= WINDY;
    // Late in the day the high is behind us: "a high of 10" when it is 10 now says nothing.
    let high_ahead = high.round() > f.current.temperature_2m.round();
    let (high, low) = (number(high), number(low));
    match lang {
        Lang::English => {
            let rain = match (rain, day == Day::Tomorrow || high_ahead) {
                (p, true) if p >= 60.0 => " and rain is likely",
                (p, true) if p >= 30.0 => " and a chance of rain",
                (p, false) if p >= 60.0 => ", and rain is likely",
                (p, false) if p >= 30.0 => ", with a chance of rain",
                _ => "",
            };
            let wind = if windy { " It's windy." } else { "" };
            let with_high = if high_ahead { format!(", with a high of {high}") } else { String::new() };
            match day {
                Day::Today => format!(
                    "It's {} and {} in {name}{with_high}{rain}.{wind}",
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
            let with_high = if high_ahead { format!(", med opptil {high} grader i dag") } else { String::new() };
            match day {
                Day::Today => format!(
                    "Det er {} og {} i {name} nå{with_high}{rain}.{wind}",
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
    matches!(code, 51..=68 | 71..=77 | 80..=86 | 95..=99)
}

fn condition(code: u32, lang: Lang) -> &'static str {
    let (en, no) = match code {
        0 => ("clear", "klart"),
        1 => ("mostly clear", "stort sett klart"),
        2 => ("partly cloudy", "delvis skyet"),
        3 => ("overcast", "overskyet"),
        45 | 48 => ("foggy", "tåke"),
        51..=57 => ("drizzle", "yr"),
        68 => ("sleet", "sludd"),
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

    fn sample() -> Forecast {
        Forecast {
            current: Current { temperature_2m: 11.6, weather_code: 2, wind_speed_10m: 3.4 },
            daily: Daily {
                weather_code: vec![61, 3],
                temperature_2m_max: vec![14.2, 12.0],
                temperature_2m_min: vec![-0.4, -3.2],
                precipitation_probability_max: vec![Some(40.0), None],
            },
        }
    }

    #[test]
    fn describes_today_and_tomorrow() {
        let f = sample();
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
        let mut f = sample();
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
        // In the evening the high is behind: not mentioned.
        f.daily.temperature_2m_max[0] = 11.8;
        f.daily.precipitation_probability_max[0] = Some(70.0);
        assert_eq!(
            describe("Bergen", &f, Day::Today, Lang::English),
            "It's 12 degrees and partly cloudy in Bergen, and rain is likely."
        );
        assert_eq!(
            describe("Bergen", &f, Day::Today, Lang::Norwegian),
            "Det er 12 grader og delvis skyet i Bergen nå, og det blir trolig regn."
        );
    }

    /// Two days of hourly steps in MET's shape, starting at 18:00 UTC.
    fn met(start: &str) -> Met {
        let start: jiff::Timestamp = start.parse().unwrap();
        let steps: Vec<String> = (0..48)
            .map(|h| {
                let time = start + jiff::SignedDuration::from_hours(h);
                // Colder at night, rain in the second afternoon.
                let temp = 10.0 + (h % 24) as f64 / 4.0;
                let symbol = if (40..46).contains(&h) { "rain" } else { "partlycloudy_day" };
                format!(
                    r#"{{"time":"{time}","data":{{"instant":{{"details":{{"air_temperature":{temp},"wind_speed":3.0}}}},
                    "next_1_hours":{{"summary":{{"symbol_code":"cloudy"}},"details":{{"probability_of_precipitation":{p}}}}},
                    "next_6_hours":{{"summary":{{"symbol_code":"{symbol}"}},"details":{{}}}}}}}}"#,
                    p = if h > 30 { 70.0 } else { 5.0 }
                )
            })
            .collect();
        serde_json::from_str(&format!(r#"{{"properties":{{"timeseries":[{}]}}}}"#, steps.join(","))).unwrap()
    }

    #[test]
    fn reads_met_steps_by_local_day() {
        let tz = TimeZone::get("Europe/Oslo").unwrap();
        let met = met("2026-09-24T18:00:00Z");
        // 20:30 in Oslo: the step from 18:00 UTC (20:00 local) is the current hour.
        let f = forecast(&met, "2026-09-24T18:30:00Z".parse().unwrap(), &tz).unwrap();
        assert_eq!(f.current.temperature_2m, 10.0);
        assert_eq!(f.current.weather_code, 3, "the next hour's symbol");
        // Tomorrow (25 September, local) runs from 22:00 UTC on the 24th: steps 4 to 27.
        assert_eq!(f.daily.temperature_2m_min[1], 10.0);
        assert_eq!(f.daily.temperature_2m_max[1], 15.75);
        // Its weather is the six hours from local noon (10:00 UTC, step 16).
        assert_eq!(f.daily.weather_code[1], 2);
        assert_eq!(f.daily.precipitation_probability_max[1], Some(5.0));
        assert_eq!(symbol_to_code("heavyrainshowersandthunder_day"), 95);
        assert_eq!(symbol_to_code("lightsleet"), 68);
        assert_eq!(
            describe("Oslo", &f, Day::Tomorrow, Lang::English),
            "Tomorrow in Oslo: partly cloudy, 10 to 16 degrees."
        );
    }
}
