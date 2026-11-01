//! Weather from Yr (MET Norway's Locationforecast): current conditions, tonight, when rain starts
//! or stops, and the forecast for each day it reaches (about nine days, hourly for the first two
//! and a half, then in six-hour steps), turned into a few short sentences. No key; MET asks for an
//! identifying User-Agent and for callers not to ask more often than the forecast changes.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use jiff::civil::{Date, DateTime, Weekday};
use jiff::tz::TimeZone;
use serde::Deserialize;

use crate::geo::{self, Location};
use crate::intent::{Day, Focus};
use crate::lang::Lang;

/// MET updates the forecast about hourly; answers within this reuse the last one.
const CACHE: Duration = Duration::from_secs(600);
/// When MET cannot be reached (a DNS outage in the cluster lasted over an hour), a forecast this
/// old still answers: the hours ahead change little.
const STALE_OK: Duration = Duration::from_secs(6 * 3600);

/// Places looked up by name kept at most; past this the kept ones are dropped.
const MAX_PLACES: usize = 256;

pub struct Weather {
    agent: ureq::Agent,
    cache: HashMap<(i64, i64), (Instant, Met)>,
    /// Places by name as asked, found or not: a place asked again needs no lookup.
    places: HashMap<String, Option<Location>>,
}

impl Weather {
    /// `user_agent` names the application and, ideally, a contact (MET's terms).
    pub fn new(user_agent: &str) -> Self {
        Self { agent: geo::agent(user_agent), cache: HashMap::new(), places: HashMap::new() }
    }

    /// A place by name, or the home or a town it sounds like; `Ok(None)` when there is none.
    pub fn find(&mut self, name: &str, home: Option<&Location>) -> Result<Option<Location>> {
        let key = name.trim().to_lowercase();
        if let Some(found) = self.places.get(&key) {
            return Ok(found.clone());
        }
        let found = geo::place(&self.agent, &key, home)?;
        if self.places.len() >= MAX_PLACES {
            self.places.clear();
        }
        self.places.insert(key, found.clone());
        Ok(found)
    }

    /// A sentence to speak about `place`.
    pub fn report(&mut self, place: &Location, day: Day, focus: Focus, lang: Lang) -> Result<String> {
        let met = self.met(place)?;
        Ok(match forecast(met, jiff::Timestamp::now(), &TimeZone::system()) {
            Some(f) => describe(&place.label, &f, day, focus, lang),
            None => no_forecast(&place.label, lang),
        })
    }

    /// Now and the next hours at `place`, for the TV screen: `None` without a forecast.
    pub fn screen(&mut self, place: &Location) -> Result<Option<serde_json::Value>> {
        let met = self.met(place)?;
        Ok(screen(met, jiff::Timestamp::now(), &TimeZone::system()).map(|mut value| {
            value["place"] = place.label.clone().into();
            value
        }))
    }

    /// Fetches `place`'s forecast once the kept one is half as old as it may get, so a question about
    /// it is always answered from memory.
    pub fn refresh(&mut self, place: &Location) -> Result<()> {
        let key = key(place);
        if !self.cache.get(&key).is_some_and(|(at, _)| at.elapsed() < CACHE / 2) {
            // The kept forecast stays until a new one has arrived.
            let met = self.fetch(key)?;
            self.cache.insert(key, (Instant::now(), met));
        }
        Ok(())
    }

    /// Whether a forecast for `place` is kept, so its answer needs no request.
    pub fn is_fresh(&self, place: &Location) -> bool {
        self.cache.get(&key(place)).is_some_and(|(at, _)| at.elapsed() < CACHE)
    }

    fn met(&mut self, place: &Location) -> Result<&Met> {
        let key = key(place);
        let age = self.cache.get(&key).map(|(at, _)| at.elapsed());
        if !age.is_some_and(|age| age < CACHE) {
            match self.fetch(key) {
                Ok(met) => {
                    self.cache.insert(key, (Instant::now(), met));
                }
                Err(error) if age.is_some_and(|age| age < STALE_OK) => {
                    tracing::warn!(error = format!("{error:#}"), ?age, "answering from an older forecast");
                }
                Err(error) => return Err(error),
            }
        }
        Ok(&self.cache[&key].1)
    }

    fn fetch(&self, key: (i64, i64)) -> Result<Met> {
        self.agent
            .get("https://api.met.no/weatherapi/locationforecast/2.0/complete")
            .query("lat", format!("{:.2}", key.0 as f64 / 100.0))
            .query("lon", format!("{:.2}", key.1 as f64 / 100.0))
            .call()
            .context("forecast request (MET Norway)")?
            .body_mut()
            .read_json()
            .context("reading the forecast")
    }
}

/// MET wants at most four decimals; two (about a kilometre) also make a good cache key.
fn key(place: &Location) -> (i64, i64) {
    ((place.latitude * 100.0).round() as i64, (place.longitude * 100.0).round() as i64)
}

/// Hours shown on the TV after the current one.
const SCREEN_HOURS: usize = 8;

/// The current step and the next hours: temperature, WMO code and rain in millimetres.
fn screen(met: &Met, now: jiff::Timestamp, tz: &TimeZone) -> Option<serde_json::Value> {
    let steps = &met.properties.timeseries;
    let current = steps.iter().rev().find(|s| s.time <= now).or(steps.first())?;
    let code = current.data.symbol().map_or(3, symbol_to_code);
    let hours: Vec<serde_json::Value> = steps
        .iter()
        .filter(|s| s.time > now)
        .filter(|s| s.data.next_1_hours.is_some())
        .take(SCREEN_HOURS)
        .map(|s| {
            let rain = s.data.next_1_hours.as_ref().and_then(|p| p.details.as_ref()?.precipitation_amount);
            serde_json::json!({
                "hour": s.time.to_zoned(tz.clone()).hour(),
                "temp": s.data.instant.details.air_temperature.map(f64::round),
                "code": s.data.symbol().map_or(3, symbol_to_code),
                "rain": rain.unwrap_or(0.0),
            })
        })
        .collect();
    Some(serde_json::json!({
        "temp": current.data.instant.details.air_temperature?.round(),
        "code": code,
        "sky": condition(code, Lang::English),
        "wind": current.data.instant.details.wind_speed.unwrap_or(0.0).round(),
        "hours": hours,
    }))
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
    precipitation_amount: Option<f64>,
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

/// The evening starts at this hour, and the night ends at this one.
const EVENING: i8 = 18;
const NIGHT_ENDS: i8 = 6;
/// "Tonight" asked before this hour is the night it is now.
const NIGHT_ASKED_UNTIL: i8 = 4;
/// A rain question about today looks at least this many hours ahead, so late in the evening it
/// still covers the night.
const RAIN_HOURS: i64 = 6;

/// Now, each local day from today as far as the forecast reaches, and the hours ahead, from
/// MET's steps.
fn forecast(met: &Met, now: jiff::Timestamp, tz: &TimeZone) -> Option<Forecast> {
    let steps = &met.properties.timeseries;
    // The step for this hour: the last one that has started.
    let current = steps.iter().rev().find(|s| s.time <= now).or(steps.first())?;
    let hour = jiff::SignedDuration::from_hours(1);
    let local = |s: &Step| s.time.to_zoned(tz.clone()).datetime();
    let ahead: Vec<&Step> = steps.iter().filter(|s| s.time + hour > now).collect();
    let mut days = Vec::new();
    let mut date = now.to_zoned(tz.clone()).date();
    loop {
        let day: Vec<&Step> = ahead.iter().copied().filter(|s| local(s).date() == date).collect();
        let temps: Vec<f64> = day.iter().filter_map(|s| s.data.instant.details.air_temperature).collect();
        if temps.is_empty() {
            break;
        }
        // The day's weather: the six hours from the step nearest noon. After the first two and a
        // half days MET only has steps at 00, 06, 12 and 18 UTC.
        let symbol = day
            .iter()
            .min_by_key(|s| (local(s).hour() - 12).abs())
            .and_then(|s| s.data.next_6_hours.as_ref().map(|p| p.summary.symbol_code.as_str()).or(s.data.symbol()));
        days.push(DayForecast {
            date,
            code: symbol.map_or(3, symbol_to_code),
            high: temps.iter().copied().fold(f64::MIN, f64::max),
            low: temps.iter().copied().fold(f64::MAX, f64::min),
            rain: day.iter().filter_map(|s| s.data.rain_chance()).reduce(f64::max),
            wind: day.iter().filter_map(|s| s.data.instant.details.wind_speed).fold(0.0, f64::max),
        });
        let Ok(next) = date.tomorrow() else { break };
        date = next;
    }
    let hours = ahead
        .iter()
        .filter_map(|s| {
            let next = s.data.next_1_hours.as_ref()?;
            Some(Hour {
                time: local(s),
                code: symbol_to_code(&next.summary.symbol_code),
                period: s.data.next_6_hours.as_ref().map(|p| symbol_to_code(&p.summary.symbol_code)),
                temp: s.data.instant.details.air_temperature,
                rain: s.data.rain_chance(),
            })
        })
        .collect();
    Some(Forecast {
        now: now.to_zoned(tz.clone()).datetime(),
        current: Current {
            temperature_2m: current.data.instant.details.air_temperature?,
            weather_code: current.data.symbol().map_or(3, symbol_to_code),
            wind_speed_10m: current.data.instant.details.wind_speed.unwrap_or(0.0),
        },
        days,
        hours,
    })
}

// --- What is said ------------------------------------------------------------------------------

struct Current {
    temperature_2m: f64,
    weather_code: u32,
    wind_speed_10m: f64,
}

struct DayForecast {
    date: Date,
    /// The WMO code for the day's weather.
    code: u32,
    high: f64,
    low: f64,
    /// The highest chance of rain in percent.
    rain: Option<f64>,
    /// The strongest wind in m/s.
    wind: f64,
}

/// An hour of MET's hourly steps.
struct Hour {
    /// Local time the hour starts.
    time: DateTime,
    /// The hour's weather as a WMO code.
    code: u32,
    /// The six hours from this one.
    period: Option<u32>,
    temp: Option<f64>,
    rain: Option<f64>,
}

struct Forecast {
    /// Local time it was read at.
    now: DateTime,
    current: Current,
    /// Today first, then each following day the forecast reaches.
    days: Vec<DayForecast>,
    /// The hours from the current one on, as far as MET gives them by the hour.
    hours: Vec<Hour>,
}

fn no_forecast(name: &str, lang: Lang) -> String {
    match lang {
        Lang::English => format!("I got no forecast for {name}."),
        Lang::Norwegian => format!("Jeg fikk ingen værmelding for {name}."),
    }
}

/// Wind worth mentioning, in m/s (a "fresh breeze" and up).
const WINDY: f64 = 8.0;
/// Below this the air is close to still (Beaufort's "light air").
const CALM: f64 = 1.5;

/// A few short sentences, the way a person would answer: now and the high for today, the range
/// for another day, the evening's low tonight. Rain only when it is at least somewhat likely, wind
/// only when it is windy, unless the question is about rain or wind.
fn describe(name: &str, f: &Forecast, day: Day, focus: Focus, lang: Lang) -> String {
    match (focus, day) {
        (Focus::Rain, Day::Today | Day::Tonight) => return rain(name, f, day, lang),
        (Focus::Wind, Day::Today | Day::Tonight) => return wind_now(name, f, lang),
        (_, Day::Tonight) => return tonight(name, f, lang),
        _ => {}
    }
    let mut said: Vec<String> = Vec::new();
    for i in offsets(day, f.now.date()) {
        let sentence = match f.days.get(i) {
            Some(_) if i == 0 && focus == Focus::Wind => wind_now(name, f, lang),
            Some(d) if i == 0 => today(name, f, d, lang),
            Some(d) if focus == Focus::Wind => format!("{} {}", later_day(name, d, i, lang), wind_up_to(d.wind, lang)),
            Some(d) => later_day(name, d, i, lang),
            None if f.days.is_empty() => no_forecast(name, lang),
            None => beyond(f.now.date(), i, lang),
        };
        if !said.contains(&sentence) {
            said.push(sentence);
        }
    }
    said.join(" ")
}

/// Which days from today `day` means: the next such weekday (today when it is one), the coming
/// weekend or what is left of it.
fn offsets(day: Day, today: Date) -> Vec<usize> {
    let ahead = |weekday: Weekday| today.weekday().until(weekday) as usize;
    match day {
        Day::Today | Day::Tonight => vec![0],
        Day::Tomorrow => vec![1],
        Day::AfterTomorrow => vec![2],
        Day::Weekday(weekday) => vec![ahead(weekday)],
        Day::Weekend if today.weekday() == Weekday::Sunday => vec![0],
        Day::Weekend => vec![ahead(Weekday::Saturday), ahead(Weekday::Saturday) + 1],
        Day::NextWeekday(weekday) => vec![match ahead(weekday) {
            0 => 7,
            n => n,
        }],
        // On a Saturday or Sunday, "next weekend" is the one after this.
        Day::NextWeekend => {
            let saturday = match today.weekday() {
                Weekday::Saturday => 7,
                Weekday::Sunday => 6,
                _ => ahead(Weekday::Saturday),
            };
            vec![saturday, saturday + 1]
        }
    }
}

fn today(name: &str, f: &Forecast, d: &DayForecast, lang: Lang) -> String {
    let code = f.current.weather_code;
    // The condition already says rain or snow: no rain clause on top.
    let rain = if wet(code) { 0.0 } else { d.rain.unwrap_or(0.0) };
    let windy = f.current.wind_speed_10m >= WINDY;
    // Late in the day the high is behind us: "a high of 10" when it is 10 now says nothing.
    let high_ahead = d.high.round() > f.current.temperature_2m.round();
    match lang {
        // A sentence for the number, one for the weather: the weather sentences are few and fixed
        // and the number ones bounded, so both are rendered ahead (`english_sentences`).
        Lang::English => {
            let high =
                if high_ahead { format!(" The high today is {}.", degrees(d.high, lang)) } else { String::new() };
            let wind = if windy { " It's windy." } else { "" };
            format!(
                "{} {}{}.{high}{wind}",
                current(name, f.current.temperature_2m),
                capitalize(condition(code, lang)),
                rain_clause(rain),
            )
        }
        Lang::Norwegian => {
            let with_high =
                if high_ahead { format!(", med opptil {} grader i dag", number(d.high)) } else { String::new() };
            let wind = if windy { " Det blåser godt." } else { "" };
            format!(
                "Det er {} og {} i {name} nå{with_high}{}.{wind}",
                degrees(f.current.temperature_2m, lang),
                condition(code, lang),
                rain_clause_no(rain),
            )
        }
    }
}

/// Tomorrow or a later day: "Saturday in Oslo: overcast. 2 to 8 degrees."
fn later_day(name: &str, d: &DayForecast, i: usize, lang: Lang) -> String {
    let rain = if wet(d.code) { 0.0 } else { d.rain.unwrap_or(0.0) };
    let label = day_label(d.date, i, lang);
    match lang {
        Lang::English => {
            format!("{label} in {name}: {}{}. {}", condition(d.code, lang), rain_clause(rain), range(d.low, d.high))
        }
        Lang::Norwegian => format!(
            "{label} i {name}: {}, {} til {} grader{}.",
            condition(d.code, lang),
            number(d.low),
            number(d.high),
            rain_clause_no(rain)
        ),
    }
}

/// "Tomorrow", else the weekday.
fn day_label(date: Date, i: usize, lang: Lang) -> String {
    match (i, lang) {
        (1, Lang::English) => "Tomorrow".into(),
        (1, Lang::Norwegian) => "I morgen".into(),
        _ => capitalize(weekday_name(date.weekday(), lang)),
    }
}

fn weekday_name(day: Weekday, lang: Lang) -> &'static str {
    let (en, no) = match day {
        Weekday::Monday => ("Monday", "mandag"),
        Weekday::Tuesday => ("Tuesday", "tirsdag"),
        Weekday::Wednesday => ("Wednesday", "onsdag"),
        Weekday::Thursday => ("Thursday", "torsdag"),
        Weekday::Friday => ("Friday", "fredag"),
        Weekday::Saturday => ("Saturday", "lørdag"),
        Weekday::Sunday => ("Sunday", "søndag"),
    };
    match lang {
        Lang::English => en,
        Lang::Norwegian => no,
    }
}

/// A day past the end of the forecast.
fn beyond(today: Date, i: usize, lang: Lang) -> String {
    let date = today.checked_add(jiff::Span::new().days(i as i64)).unwrap_or(today);
    match lang {
        Lang::English => beyond_sentence(date.weekday()),
        Lang::Norwegian => format!("Værmeldingen rekker ikke til {} ennå.", weekday_name(date.weekday(), lang)),
    }
}

/// The hours that overlap `from` to `to`.
fn hours_between(f: &Forecast, from: DateTime, to: DateTime) -> Vec<&Hour> {
    let hour = jiff::SignedDuration::from_hours(1);
    f.hours.iter().filter(|h| h.time.saturating_add(hour) > from && h.time < to).collect()
}

/// From now, or from the evening when it is still day, until the night ends. After
/// [`NIGHT_ASKED_UNTIL`] the night is nearly over: "tonight" then means the coming evening.
fn night(now: DateTime) -> (DateTime, DateTime) {
    let date = now.date();
    if now.hour() < NIGHT_ASKED_UNTIL {
        return (now, date.at(NIGHT_ENDS, 0, 0, 0));
    }
    let morning = date.tomorrow().unwrap_or(date).at(NIGHT_ENDS, 0, 0, 0);
    (now.max(date.at(EVENING, 0, 0, 0)), morning)
}

/// From now to midnight, and at least [`RAIN_HOURS`].
fn rest_of_today(now: DateTime) -> (DateTime, DateTime) {
    let midnight = now.date().tomorrow().unwrap_or(now.date()).at(0, 0, 0, 0);
    (now, midnight.max(now.saturating_add(jiff::SignedDuration::from_hours(RAIN_HOURS))))
}

/// "Tonight in Oslo: clear. Down to 2 degrees."
fn tonight(name: &str, f: &Forecast, lang: Lang) -> String {
    let (from, to) = night(f.now);
    let hours = hours_between(f, from, to);
    let (Some(first), Some(low)) = (hours.first(), hours.iter().filter_map(|h| h.temp).reduce(f64::min)) else {
        return no_forecast(name, lang);
    };
    let code = first.period.unwrap_or(first.code);
    let rain = if wet(code) { 0.0 } else { hours.iter().filter_map(|h| h.rain).reduce(f64::max).unwrap_or(0.0) };
    match lang {
        Lang::English => format!(
            "Tonight in {name}: {}{}. Down to {}.",
            condition(code, lang),
            rain_clause(rain),
            degrees(low, lang)
        ),
        Lang::Norwegian => format!(
            "I kveld og i natt i {name}: {}, ned mot {}{}.",
            condition(code, lang),
            degrees(low, lang),
            rain_clause_no(rain)
        ),
    }
}

#[derive(Clone, Copy)]
enum Kind {
    Rain,
    Snow,
    Sleet,
}

impl Kind {
    fn of(code: u32) -> Self {
        match code {
            68 => Kind::Sleet,
            71..=77 | 85 | 86 => Kind::Snow,
            _ => Kind::Rain,
        }
    }

    /// "rain", "raining" and the Norwegian noun.
    fn words(self) -> (&'static str, &'static str, &'static str) {
        match self {
            Kind::Rain => ("Rain", "raining", "regn"),
            Kind::Snow => ("Snow", "snowing", "snø"),
            Kind::Sleet => ("Sleet", "sleeting", "sludd"),
        }
    }
}

/// When rain starts or stops, from the hourly forecast: the temperature now, then whether it is
/// raining and until when, or when it starts. The times are sentences of their own.
fn rain(name: &str, f: &Forecast, day: Day, lang: Lang) -> String {
    let (from, to) = if day == Day::Tonight { night(f.now) } else { rest_of_today(f.now) };
    let later: Vec<&Hour> = hours_between(f, from, to).into_iter().filter(|h| h.time > f.now).collect();
    let at = |h: &Hour| h.time.strftime("%H:%M").to_string();
    let no = lang == Lang::Norwegian;
    let night = day == Day::Tonight;
    let mut said = vec![match lang {
        Lang::English => current(name, f.current.temperature_2m),
        Lang::Norwegian => format!("Det er {} i {name}.", degrees(f.current.temperature_2m, lang)),
    }];
    // "Will it rain tonight" asked in the afternoon: the rain now says nothing about the night,
    // and when it stops is not when the night's window opens.
    if wet(f.current.weather_code) && from <= f.now {
        let (_, ing, _) = Kind::of(f.current.weather_code).words();
        said.push(match (no, Kind::of(f.current.weather_code)) {
            (false, _) => format!("It's {ing} now."),
            (true, Kind::Rain) => "Det regner nå.".into(),
            (true, Kind::Snow) => "Det snør nå.".into(),
            (true, Kind::Sleet) => "Det sludder nå.".into(),
        });
        said.push(match (later.iter().find(|h| !wet(h.code)), no, night) {
            (Some(h), false, _) => format!("It should stop around {}.", at(h)),
            (Some(h), true, _) => format!("Det gir seg trolig rundt {}.", at(h)),
            (None, false, false) => format!("It should keep {ing} all day."),
            (None, false, true) => format!("It should keep {ing} all night."),
            (None, true, false) => "Det ser ut til å fortsette hele dagen.".into(),
            (None, true, true) => "Det ser ut til å fortsette hele natten.".into(),
        });
    } else {
        match later.iter().position(|h| wet(h.code)) {
            None => said.push(
                match (no, night) {
                    (false, false) => "It should stay dry for the rest of the day.",
                    (false, true) => "It should stay dry tonight.",
                    (true, false) => "Det ser ut til å holde seg tørt resten av dagen.",
                    (true, true) => "Det ser ut til å holde seg tørt i kveld og i natt.",
                }
                .into(),
            ),
            Some(start) => {
                let (noun, _, norsk) = Kind::of(later[start].code).words();
                let end = later[start..].iter().find(|h| !wet(h.code));
                said.push(match (end, no) {
                    (Some(end), false) => {
                        format!("{noun} is expected from around {} to {}.", at(later[start]), at(end))
                    }
                    (None, false) => format!("{noun} is expected from around {}.", at(later[start])),
                    (Some(end), true) => format!("Det ventes {norsk} fra rundt {} til {}.", at(later[start]), at(end)),
                    (None, true) => format!("Det ventes {norsk} fra rundt {}.", at(later[start])),
                });
            }
        }
    }
    said.join(" ")
}

/// The wind now, calm or not: "In Oslo, the wind is 3 metres per second."
fn wind_now(name: &str, f: &Forecast, lang: Lang) -> String {
    let wind = f.current.wind_speed_10m;
    let speed = wind.round() as i64;
    match lang {
        Lang::English => {
            let unit = if speed == 1 { "metre" } else { "metres" };
            let remark = match wind {
                w if w >= WINDY => " It's windy.",
                w if w < CALM => " It's calm.",
                _ => "",
            };
            format!("In {name}, the wind is {speed} {unit} per second.{remark}")
        }
        Lang::Norwegian => {
            let remark = match wind {
                w if w >= WINDY => " Det blåser godt.",
                w if w < CALM => " Det er nesten vindstille.",
                _ => "",
            };
            format!("Vinden i {name} er {speed} meter per sekund nå.{remark}")
        }
    }
}

/// A day's strongest wind, after its forecast.
fn wind_up_to(wind: f64, lang: Lang) -> String {
    let speed = wind.round() as i64;
    match lang {
        Lang::English => {
            format!("The wind gets up to {speed} {} per second.", if speed == 1 { "metre" } else { "metres" })
        }
        Lang::Norwegian => format!("Vinden blir opptil {speed} meter per sekund."),
    }
}

/// The rain clause after the condition, by the chance of rain in percent.
fn rain_clause(chance: f64) -> &'static str {
    match chance {
        p if p >= 60.0 => ", and rain is likely",
        p if p >= 30.0 => ", with a chance of rain",
        _ => "",
    }
}

fn rain_clause_no(chance: f64) -> &'static str {
    match chance {
        p if p >= 60.0 => ", og det blir trolig regn",
        p if p >= 30.0 => ", og litt sjanse for regn",
        _ => "",
    }
}

/// "It's 4 degrees in Oslo."
fn current(name: &str, t: f64) -> String {
    format!("It's {} in {name}.", degrees(t, Lang::English))
}

/// "Minus 3 to 12 degrees."
fn range(low: f64, high: f64) -> String {
    capitalize(&format!("{} to {} degrees.", number(low), number(high)))
}

fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map_or_else(String::new, |c| c.to_uppercase().chain(chars).collect())
}

/// Every English weather sentence for a place with temperatures in a usual range, to render ahead:
/// the weather for today, tonight and the days ahead, when rain starts and stops, and the numbers.
pub fn english_sentences(name: &str) -> Vec<String> {
    let codes = [0, 1, 2, 3, 45, 51, 68, 61, 63, 65, 66, 71, 73, 75, 77, 95, 96, 4];
    let rains = [0.0, 30.0, 60.0];
    let weekdays = [
        Weekday::Monday,
        Weekday::Tuesday,
        Weekday::Wednesday,
        Weekday::Thursday,
        Weekday::Friday,
        Weekday::Saturday,
        Weekday::Sunday,
    ];
    let labels: Vec<&str> =
        ["Tomorrow", "Tonight"].into_iter().chain(weekdays.map(|d| weekday_name(d, Lang::English))).collect();
    let mut sentences = Vec::new();
    for code in codes {
        let condition = condition(code, Lang::English);
        for rain in rains {
            let rain = if wet(code) { "" } else { rain_clause(rain) };
            sentences.push(format!("{}{rain}.", capitalize(condition)));
            sentences.extend(labels.iter().map(|label| format!("{label} in {name}: {condition}{rain}.")));
        }
    }
    sentences.extend(["It's windy.", "It's calm."].map(String::from));
    sentences.extend(weekdays.map(beyond_sentence));
    for t in -25..=35 {
        sentences.push(current(name, t as f64));
        sentences.push(format!("The high today is {}.", degrees(t as f64, Lang::English)));
        sentences.push(format!("Down to {}.", degrees(t as f64, Lang::English)));
    }
    // Another day's low and high: a day's spread is seldom more than fifteen degrees.
    for low in -20..=25 {
        sentences.extend((low..=low + 15).map(|high| range(low as f64, high as f64)));
    }
    for speed in 0..=30 {
        let unit = if speed == 1 { "metre" } else { "metres" };
        sentences.push(format!("In {name}, the wind is {speed} {unit} per second."));
        sentences.push(format!("The wind gets up to {speed} {unit} per second."));
    }
    sentences.extend(
        [
            "It should stay dry for the rest of the day.",
            "It should stay dry tonight.",
            "It's raining now.",
            "It's snowing now.",
            "It should keep raining all day.",
            "It should keep raining all night.",
        ]
        .map(String::from),
    );
    for hour in 0..24 {
        sentences.push(format!("It should stop around {hour:02}:00."));
        sentences.push(format!("Rain is expected from around {hour:02}:00."));
        sentences.push(format!("Snow is expected from around {hour:02}:00."));
        // Showers seldom last longer than this; longer ones are kept once said.
        for length in 1..=6 {
            sentences.push(format!("Rain is expected from around {hour:02}:00 to {:02}:00.", (hour + length) % 24));
        }
    }
    sentences.sort();
    sentences.dedup();
    sentences
}

fn beyond_sentence(day: Weekday) -> String {
    format!("The forecast doesn't reach {} yet.", weekday_name(day, Lang::English))
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
    use jiff::civil::Weekday::*;

    fn day(date: &str, code: u32, high: f64, low: f64, rain: Option<f64>) -> DayForecast {
        DayForecast { date: date.parse().unwrap(), code, high, low, rain, wind: 6.4 }
    }

    /// Thursday 24 September 2026 at 14:30, and the days to Sunday.
    fn sample() -> Forecast {
        Forecast {
            now: "2026-09-24T14:30".parse().unwrap(),
            current: Current { temperature_2m: 11.6, weather_code: 2, wind_speed_10m: 3.4 },
            days: vec![
                day("2026-09-24", 61, 14.2, -0.4, Some(40.0)),
                day("2026-09-25", 3, 12.0, -3.2, None),
                day("2026-09-26", 0, 9.0, 2.0, Some(10.0)),
                day("2026-09-27", 63, 7.4, 3.0, Some(90.0)),
            ],
            hours: hours(&[]),
        }
    }

    /// Hourly steps from 14:00 on the sample's day to 06:00 the next, wet in the hours given.
    fn hours(wet: &[i8]) -> Vec<Hour> {
        let start: DateTime = "2026-09-24T14:00".parse().unwrap();
        (0..16)
            .map(|h| {
                let time = start.saturating_add(jiff::SignedDuration::from_hours(h));
                let rainy = wet.contains(&time.hour());
                Hour {
                    time,
                    code: if rainy { 63 } else { 1 },
                    period: None,
                    temp: Some(12.0 - h as f64 / 2.0),
                    rain: Some(if rainy { 80.0 } else { 10.0 }),
                }
            })
            .collect()
    }

    fn say(f: &Forecast, day: Day) -> String {
        describe("Oslo", f, day, Focus::General, Lang::English)
    }

    #[test]
    fn describes_today_and_tomorrow() {
        let f = sample();
        assert_eq!(
            say(&f, Day::Today),
            "It's 12 degrees in Oslo. Partly cloudy, with a chance of rain. The high today is 14 degrees."
        );
        assert_eq!(say(&f, Day::Tomorrow), "Tomorrow in Oslo: overcast. Minus 3 to 12 degrees.");
        assert_eq!(
            describe("Oslo", &f, Day::Today, Focus::General, Lang::Norwegian),
            "Det er 12 grader og delvis skyet i Oslo nå, med opptil 14 grader i dag, og litt sjanse for regn."
        );
        assert_eq!(
            describe("Oslo", &f, Day::Tomorrow, Focus::General, Lang::Norwegian),
            "I morgen i Oslo: overskyet, minus 3 til 12 grader."
        );
    }

    #[test]
    fn describes_weekdays_and_the_weekend() {
        let f = sample();
        assert_eq!(say(&f, Day::Weekday(Saturday)), "Saturday in Oslo: clear. 2 to 9 degrees.");
        assert_eq!(say(&f, Day::AfterTomorrow), "Saturday in Oslo: clear. 2 to 9 degrees.");
        assert_eq!(say(&f, Day::Weekday(Friday)), say(&f, Day::Tomorrow));
        // Today's weekday is today.
        assert_eq!(say(&f, Day::Weekday(Thursday)), say(&f, Day::Today));
        assert_eq!(
            say(&f, Day::Weekend),
            "Saturday in Oslo: clear. 2 to 9 degrees. Sunday in Oslo: rain. 3 to 7 degrees."
        );
        assert_eq!(
            describe("Oslo", &f, Day::Weekday(Sunday), Focus::General, Lang::Norwegian),
            "Søndag i Oslo: regn, 3 til 7 grader."
        );
        // Past the end of the forecast.
        assert_eq!(say(&f, Day::Weekday(Wednesday)), "The forecast doesn't reach Wednesday yet.");
        assert_eq!(
            describe("Oslo", &f, Day::Weekday(Wednesday), Focus::General, Lang::Norwegian),
            "Værmeldingen rekker ikke til onsdag ennå."
        );
    }

    #[test]
    fn weekend_days_from_each_weekday() {
        let date = |d: &str| -> Date { d.parse().unwrap() };
        // Thursday, Saturday and Sunday.
        assert_eq!(offsets(Day::Weekend, date("2026-09-24")), [2, 3]);
        assert_eq!(offsets(Day::Weekend, date("2026-09-26")), [0, 1]);
        assert_eq!(offsets(Day::Weekend, date("2026-09-27")), [0]);
        assert_eq!(offsets(Day::Weekday(Monday), date("2026-09-27")), [1]);
        // "Next Monday" asked on a Monday is a week ahead; on a Sunday, tomorrow.
        assert_eq!(offsets(Day::NextWeekday(Monday), date("2026-09-28")), [7]);
        assert_eq!(offsets(Day::NextWeekday(Monday), date("2026-09-27")), [1]);
        // "Next weekend" on a Thursday is the coming one, on a Saturday or Sunday the one after.
        assert_eq!(offsets(Day::NextWeekend, date("2026-09-24")), [2, 3]);
        assert_eq!(offsets(Day::NextWeekend, date("2026-09-26")), [7, 8]);
        assert_eq!(offsets(Day::NextWeekend, date("2026-09-27")), [6, 7]);
    }

    #[test]
    fn describes_tonight() {
        let mut f = sample();
        // From 18:00 to 06:00: the low is 4.5 degrees, at 05:00.
        assert_eq!(say(&f, Day::Tonight), "Tonight in Oslo: mostly clear. Down to 5 degrees.");
        f.hours = hours(&[20, 21]);
        assert_eq!(say(&f, Day::Tonight), "Tonight in Oslo: mostly clear, and rain is likely. Down to 5 degrees.");
        assert_eq!(
            describe("Oslo", &f, Day::Tonight, Focus::General, Lang::Norwegian),
            "I kveld og i natt i Oslo: stort sett klart, ned mot 5 grader, og det blir trolig regn."
        );
        // After midnight, the rest of the night.
        f.now = "2026-09-25T03:10".parse().unwrap();
        assert_eq!(say(&f, Day::Tonight), "Tonight in Oslo: mostly clear. Down to 5 degrees.");
        // At half past five the night is over: tonight is the coming evening, past these hours.
        f.now = "2026-09-25T05:30".parse().unwrap();
        assert_eq!(night(f.now).0, "2026-09-25T18:00".parse::<DateTime>().unwrap());
    }

    #[test]
    fn rain_tonight_asked_while_it_rains_in_the_day() {
        let mut f = sample();
        // Raining at 14:30, dry from 15:00, rain again at 21:00.
        f.current.weather_code = 63;
        f.hours = hours(&[14, 21]);
        assert_eq!(rain(&f, Day::Tonight), "It's 12 degrees in Oslo. Rain is expected from around 21:00 to 22:00.");
        f.hours = hours(&[14]);
        assert_eq!(rain(&f, Day::Tonight), "It's 12 degrees in Oslo. It should stay dry tonight.");
    }

    fn rain(f: &Forecast, day: Day) -> String {
        describe("Oslo", f, day, Focus::Rain, Lang::English)
    }

    #[test]
    fn says_when_rain_starts_and_stops() {
        let mut f = sample();
        assert_eq!(rain(&f, Day::Today), "It's 12 degrees in Oslo. It should stay dry for the rest of the day.");
        f.hours = hours(&[16, 17, 18]);
        assert_eq!(rain(&f, Day::Today), "It's 12 degrees in Oslo. Rain is expected from around 16:00 to 19:00.");
        assert_eq!(
            describe("Oslo", &f, Day::Today, Focus::Rain, Lang::Norwegian),
            "Det er 12 grader i Oslo. Det ventes regn fra rundt 16:00 til 19:00."
        );
        // Raining now: when it stops.
        f.now = "2026-09-24T16:30".parse().unwrap();
        f.current.weather_code = 63;
        assert_eq!(rain(&f, Day::Today), "It's 12 degrees in Oslo. It's raining now. It should stop around 19:00.");
        f.hours = hours(&(14..24).collect::<Vec<_>>());
        assert_eq!(rain(&f, Day::Today), "It's 12 degrees in Oslo. It's raining now. It should keep raining all day.");
        // Rain that starts and goes on past the hours looked at.
        f.current.weather_code = 1;
        f.hours = hours(&(20..24).chain(0..6).collect::<Vec<_>>());
        assert_eq!(rain(&f, Day::Today), "It's 12 degrees in Oslo. Rain is expected from around 20:00.");
        assert_eq!(rain(&f, Day::Tonight), "It's 12 degrees in Oslo. Rain is expected from around 20:00.");
        // Snow by its name.
        for h in &mut f.hours {
            if h.code == 63 {
                h.code = 73;
            }
        }
        assert_eq!(rain(&f, Day::Today), "It's 12 degrees in Oslo. Snow is expected from around 20:00.");
        // Rain on another day: that day's forecast, which says the chance of rain.
        assert_eq!(rain(&f, Day::Tomorrow), say(&f, Day::Tomorrow));
    }

    #[test]
    fn says_the_wind_even_when_calm() {
        let mut f = sample();
        let wind = |f: &Forecast, day: Day| describe("Oslo", f, day, Focus::Wind, Lang::English);
        assert_eq!(wind(&f, Day::Today), "In Oslo, the wind is 3 metres per second.");
        f.current.wind_speed_10m = 0.8;
        assert_eq!(wind(&f, Day::Today), "In Oslo, the wind is 1 metre per second. It's calm.");
        f.current.wind_speed_10m = 12.2;
        assert_eq!(wind(&f, Day::Today), "In Oslo, the wind is 12 metres per second. It's windy.");
        assert_eq!(
            describe("Oslo", &f, Day::Today, Focus::Wind, Lang::Norwegian),
            "Vinden i Oslo er 12 meter per sekund nå. Det blåser godt."
        );
        assert_eq!(
            wind(&f, Day::Tomorrow),
            "Tomorrow in Oslo: overcast. Minus 3 to 12 degrees. The wind gets up to 6 metres per second."
        );
    }

    #[test]
    fn english_sentences_are_rendered_ahead() {
        let ahead = english_sentences("Oslo");
        let mut f = sample();
        let days = [
            Day::Today,
            Day::Tonight,
            Day::Tomorrow,
            Day::AfterTomorrow,
            Day::Weekend,
            Day::Weekday(Saturday),
            Day::Weekday(Wednesday),
        ];
        for (rain, wind, wet) in
            [(Some(10.0), 2.0, vec![]), (Some(40.0), 11.0, vec![16, 17]), (Some(90.0), 0.4, vec![22])]
        {
            for d in &mut f.days {
                d.rain = rain;
            }
            f.current.wind_speed_10m = wind;
            f.hours = hours(&wet);
            for day in days {
                for focus in [Focus::General, Focus::Rain, Focus::Wind] {
                    let said = describe("Oslo", &f, day, focus, Lang::English);
                    for sentence in said.split_inclusive(". ").map(str::trim) {
                        assert!(ahead.iter().any(|a| a == sentence), "{sentence}");
                        assert!(crate::speech_cache::CachedTts::cacheable(sentence), "{sentence}");
                    }
                }
            }
        }
    }

    #[test]
    fn mentions_wind_only_when_windy_and_rain_only_when_likely() {
        let mut f = sample();
        f.current.wind_speed_10m = 11.0;
        f.days[0].rain = Some(80.0);
        let bergen = |f: &Forecast, day: Day, lang: Lang| describe("Bergen", f, day, Focus::General, lang);
        assert_eq!(
            bergen(&f, Day::Today, Lang::English),
            "It's 12 degrees in Bergen. Partly cloudy, and rain is likely. The high today is 14 degrees. It's windy."
        );
        // Rain as the condition is not repeated as a chance of rain.
        f.days[1].code = 63;
        f.days[1].rain = Some(90.0);
        assert_eq!(bergen(&f, Day::Tomorrow, Lang::Norwegian), "I morgen i Bergen: regn, minus 3 til 12 grader.");
        f.days[0].rain = Some(10.0);
        f.current.wind_speed_10m = 2.0;
        assert_eq!(
            bergen(&f, Day::Today, Lang::English),
            "It's 12 degrees in Bergen. Partly cloudy. The high today is 14 degrees."
        );
        // In the evening the high is behind: not mentioned.
        f.days[0].high = 11.8;
        f.days[0].rain = Some(70.0);
        assert_eq!(
            bergen(&f, Day::Today, Lang::English),
            "It's 12 degrees in Bergen. Partly cloudy, and rain is likely."
        );
        assert_eq!(
            bergen(&f, Day::Today, Lang::Norwegian),
            "Det er 12 grader og delvis skyet i Bergen nå, og det blir trolig regn."
        );
    }

    /// Two days of hourly steps in MET's shape, starting at 18:00 UTC, then six-hour steps for a
    /// week as MET gives them past the first days.
    fn met(start: &str) -> Met {
        let start: jiff::Timestamp = start.parse().unwrap();
        let step = |h: i64, hourly: bool| {
            let time = start + jiff::SignedDuration::from_hours(h);
            // Colder at night, rain in the second afternoon.
            let temp = 10.0 + (h % 24) as f64 / 4.0;
            let symbol = if (40..46).contains(&h) { "rain" } else { "partlycloudy_day" };
            let next_hour = if hourly {
                format!(
                    r#""next_1_hours":{{"summary":{{"symbol_code":"cloudy"}},"details":{{"probability_of_precipitation":{p}}}}},"#,
                    p = if h > 30 { 70.0 } else { 5.0 }
                )
            } else {
                String::new()
            };
            format!(
                r#"{{"time":"{time}","data":{{"instant":{{"details":{{"air_temperature":{temp},"wind_speed":3.0}}}},
                {next_hour}"next_6_hours":{{"summary":{{"symbol_code":"{symbol}"}},"details":{{}}}}}}}}"#
            )
        };
        let steps: Vec<String> =
            (0..48).map(|h| step(h, true)).chain((0..28).map(|i| step(54 + i * 6, false))).collect();
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
        assert_eq!(f.days[1].low, 10.0);
        assert_eq!(f.days[1].high, 15.75);
        // Its weather is the six hours from local noon (10:00 UTC, step 16).
        assert_eq!(f.days[1].code, 2);
        assert_eq!(f.days[1].rain, Some(5.0));
        // Six-hour steps reach nine days on; the hours stop where the hourly steps do.
        assert_eq!(f.days.len(), 10);
        assert_eq!(f.days[9].date, "2026-10-03".parse().unwrap());
        assert_eq!(f.hours.len(), 48);
        assert_eq!(symbol_to_code("heavyrainshowersandthunder_day"), 95);
        assert_eq!(symbol_to_code("lightsleet"), 68);
        assert_eq!(
            describe("Oslo", &f, Day::Tomorrow, Focus::General, Lang::English),
            "Tomorrow in Oslo: partly cloudy. 10 to 16 degrees."
        );
    }

    #[test]
    fn screen_shows_the_next_hours() {
        let tz = TimeZone::get("Europe/Oslo").unwrap();
        let met = met("2026-09-24T18:00:00Z");
        let value = screen(&met, "2026-09-24T18:30:00Z".parse().unwrap(), &tz).unwrap();
        assert_eq!(value["temp"], 10.0);
        assert_eq!(value["sky"], "overcast");
        let hours = value["hours"].as_array().unwrap();
        assert_eq!(hours.len(), SCREEN_HOURS);
        // 19:00 UTC is 21:00 in Oslo.
        assert_eq!(hours[0]["hour"], 21);
        assert_eq!(hours[0]["rain"], 0.0);
    }
}
