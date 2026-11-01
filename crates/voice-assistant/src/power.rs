//! Electricity prices in Norway: Nord Pool spot prices per price area from hvakosterstrommen.no
//! (no key), with VAT as people pay it (none in NO4). "What does power cost now", "when is
//! electricity cheapest tonight", "hva koster strømmen", "når er strømmen billigst i morgen".

use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use jiff::civil::Date;
use jiff::{Timestamp, ToSpan, Zoned};
use serde::Deserialize;

use crate::dialog::normalize;
use crate::lang::Lang;

const URL: &str = "https://www.hvakosterstrommen.no/api/v1/prices";
/// Tomorrow's prices appear around 13:00; a missing day is asked again after this long.
const RETRY_MISSING: Duration = Duration::from_secs(900);
/// "Tonight" runs until this hour the next morning.
const NIGHT_ENDS: i8 = 7;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum When {
    /// From now to midnight.
    Today,
    /// From now to 07:00 tomorrow.
    Tonight,
    Tomorrow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerQuery {
    Now,
    /// "What's the electricity price tomorrow": tomorrow's range.
    Tomorrow,
    Cheapest(When),
    Dearest(When),
    /// "The cheapest 3 hours", "when should I run the dishwasher": the cheapest stretch of whole
    /// hours in a row. `None` is from now on, over every price known, so it crosses midnight once
    /// tomorrow's prices are out.
    Window {
        hours: u8,
        when: Option<When>,
    },
}

/// Appliances people time by the price, and how long a run of each takes.
const APPLIANCES: &[(&str, u8)] = &[
    ("dishwasher", 3),
    ("oppvaskmaskin", 3),
    ("oppvaskmaskinen", 3),
    ("oppvasken", 3),
    ("washer", 2),
    ("vaskemaskin", 2),
    ("vaskemaskinen", 2),
    ("klesvasken", 2),
    ("dryer", 2),
    ("tørketrommel", 2),
    ("tørketrommelen", 2),
];
/// Charging the car takes longer than a wash.
const CAR_HOURS: u8 = 4;
/// Longest stretch asked for: a day of prices has 24 hours.
const MOST_HOURS: u8 = 12;
const HOUR_WORDS: &[&str] = &["hours", "hour", "timer", "timene", "timers", "time", "timen"];
const NUMBER_WORDS: &[(&str, u8)] = &[
    ("a", 1),
    ("an", 1),
    ("one", 1),
    ("two", 2),
    ("three", 3),
    ("four", 4),
    ("five", 5),
    ("six", 6),
    ("seven", 7),
    ("eight", 8),
    ("nine", 9),
    ("ten", 10),
    ("eleven", 11),
    ("twelve", 12),
    ("en", 1),
    ("ett", 1),
    ("to", 2),
    ("tre", 3),
    ("fire", 4),
    ("fem", 5),
    ("seks", 6),
    ("sju", 7),
    ("syv", 7),
    ("åtte", 8),
    ("ni", 9),
    ("ti", 10),
    ("elleve", 11),
    ("tolv", 12),
];
/// Verbs for starting an appliance: "when should I run the dishwasher".
const RUN_WORDS: &[&str] = &["run", "start", "use", "put", "kjøre", "kjøra", "starte", "sette", "bruke", "skru"];

/// How many hours in a row are asked for: "3 hours", "tre timer", "an hour".
fn hours_said(words: &[String]) -> Option<u8> {
    let at = words.iter().position(|w| HOUR_WORDS.contains(&w.as_str()))?;
    let before = words.get(at.checked_sub(1)?)?.as_str();
    let hours =
        before.parse::<u8>().ok().or_else(|| NUMBER_WORDS.iter().find(|(w, _)| *w == before).map(|&(_, n)| n))?;
    (1..=MOST_HOURS).contains(&hours).then_some(hours)
}

/// The run length of an appliance the request names, if it asks when to run it.
fn appliance_hours(words: &[String]) -> Option<u8> {
    let has = |w: &str| words.iter().any(|x| x == w);
    let pair = |a: &str, b: &str| words.windows(2).any(|p| p[0] == a && p[1] == b);
    let hours = if pair("washing", "machine") || pair("tumble", "dryer") {
        2
    } else if (has("charge") || has("lade")) && (has("car") || has("bilen") || has("elbilen")) {
        CAR_HOURS
    } else {
        APPLIANCES.iter().find(|(w, _)| has(w)).map(|&(_, h)| h)?
    };
    // "When is the dishwasher done" asks about the machine, not the price.
    let cheap = words.iter().any(|w| CHEAP_WORDS.contains(&w.as_str()));
    let best = has("best") || has("beste") || has("lurt") || has("lurest");
    let ask = has("when") || has("når") || has("should") || has("bør");
    (cheap || best || (ask && words.iter().any(|w| RUN_WORDS.contains(&w.as_str())))).then_some(hours)
}

const POWER_WORDS: &[&str] = &[
    "electricity",
    "strøm",
    "strømmen",
    "strømpris",
    "strømprisen",
    "strømprisene",
    "kilowatt",
    "kilowatthour",
    "kwh",
];
const PRICE_WORDS: &[&str] =
    &["price", "prices", "cost", "costs", "cheap", "cheaper", "cheapest", "expensive", "pris", "prisen", "koster"];
const CHEAP_WORDS: &[&str] = &["cheap", "cheapest", "cheaper", "lowest", "billig", "billigst", "billigste", "lavest"];
const DEAR_WORDS: &[&str] = &["expensive", "dearest", "highest", "dyr", "dyrest", "dyreste", "høyest", "høyeste"];

pub fn parse(text: &str) -> Option<PowerQuery> {
    let words: Vec<String> = text.split_whitespace().map(normalize).filter(|w| !w.is_empty()).collect();
    let has = |w: &str| words.iter().any(|x| x == w);
    let pair = |a: &str, b: &str| words.windows(2).any(|p| p[0] == a && p[1] == b);
    let said_when = if has("tomorrow") || has("imorgen") || pair("i", "morgen") {
        Some(When::Tomorrow)
    } else if has("tonight") || has("night") || pair("i", "kveld") || pair("i", "natt") || has("natt") {
        Some(When::Tonight)
    } else if has("today") || pair("i", "dag") {
        Some(When::Today)
    } else {
        None
    };
    // "When should I run the dishwasher" names no power word; the machine says how long.
    if let Some(hours) = appliance_hours(&words) {
        return Some(PowerQuery::Window { hours: hours_said(&words).unwrap_or(hours), when: said_when });
    }
    let power = words.iter().any(|w| POWER_WORDS.contains(&w.as_str()))
        || ((has("power") || has("energy")) && words.iter().any(|w| PRICE_WORDS.contains(&w.as_str())));
    if !power {
        return None;
    }
    let when = said_when.unwrap_or(When::Today);
    let cheap = words.iter().any(|w| CHEAP_WORDS.contains(&w.as_str())) || pair("best", "time");
    if let (true, Some(hours)) = (cheap, hours_said(&words)) {
        return Some(PowerQuery::Window { hours, when: said_when });
    }
    if cheap {
        return Some(PowerQuery::Cheapest(when));
    }
    // "How expensive is power", "er strømmen dyr" ask the price; "when is it most expensive" the dearest.
    // Tonight has no price now: "how expensive is power tonight" asks for its dearest hour.
    let plain_dear = ["expensive", "dyr"];
    let superlative = has("most") || has("when") || has("når") || has("hour") || has("time") || when == When::Tonight;
    if words.iter().any(|w| DEAR_WORDS.contains(&w.as_str()) && (!plain_dear.contains(&w.as_str()) || superlative)) {
        return Some(PowerQuery::Dearest(when));
    }
    Some(if when == When::Tomorrow { PowerQuery::Tomorrow } else { PowerQuery::Now })
}

/// The price area for a place in Norway, roughly by its coordinates: NO4 north of Saltfjellet,
/// NO3 Trøndelag and Møre, NO5 the west coast around Bergen, NO2 the south and south-west, NO1
/// the east. Wrong near the borders; the `price-area` setting overrides it.
pub fn area_for(latitude: f64, longitude: f64) -> &'static str {
    if latitude >= 66.0 {
        "NO4"
    } else if latitude >= 62.0 {
        "NO3"
    } else if longitude < 7.3 && latitude >= 59.7 {
        "NO5"
    } else if longitude < 8.8 || latitude < 59.2 {
        "NO2"
    } else {
        "NO1"
    }
}

#[derive(Debug, Clone, Deserialize)]
struct Interval {
    #[serde(rename = "NOK_per_kWh")]
    nok_per_kwh: f64,
    time_start: Timestamp,
    time_end: Timestamp,
}

pub struct Power {
    area: String,
    agent: ureq::Agent,
    /// Prices by day; `None` when the day was not published yet (checked again later).
    days: Vec<(Date, Option<Vec<Interval>>, Instant)>,
}

impl Power {
    pub fn new(user_agent: &str, area: &str) -> Self {
        Self { area: area.to_uppercase(), agent: crate::geo::agent(user_agent), days: Vec::new() }
    }

    pub fn area(&self) -> &str {
        &self.area
    }

    pub fn answer(&mut self, query: PowerQuery, lang: Lang) -> Result<String> {
        let now = Zoned::now();
        let today = now.date();
        let no = lang == Lang::Norwegian;
        let vat = if self.area == "NO4" { 1.0 } else { 1.25 };
        let price = |i: &Interval| i.nok_per_kwh * vat;
        let price_of = |i: &Interval| format!("{} {}", format_price(price(i), lang), per_kwh(lang));
        let at = |i: &Interval| i.time_start.to_zoned(now.time_zone().clone()).strftime("%H:%M").to_string();
        match query {
            PowerQuery::Now => {
                let day = self.day(today)?.context("today's prices are missing")?;
                let current = day
                    .iter()
                    .find(|i| i.time_start <= now.timestamp() && now.timestamp() < i.time_end)
                    .context("no price for the current hour")?;
                let low = day.iter().map(price).fold(f64::MAX, f64::min);
                let high = day.iter().map(price).fold(f64::MIN, f64::max);
                Ok(if no {
                    format!(
                        "Strømmen koster {} nå. I dag fra {} til {}.",
                        price_of(current),
                        format_price(low, lang),
                        format_price(high, lang)
                    )
                } else {
                    // The first sentence is one of a bounded set, rendered ahead (`english_sentences`);
                    // the range is synthesized while it plays.
                    format!(
                        "Right now, power costs {}. Today it ranges from {} to {}.",
                        price_of(current),
                        format_price(low, lang),
                        format_price(high, lang)
                    )
                })
            }
            PowerQuery::Tomorrow => {
                let Some(day) = self.day(today.checked_add(1.day())?)? else {
                    return Ok(if no {
                        "Morgendagens strømpriser kommer rundt klokka ett.".into()
                    } else {
                        "Tomorrow's prices come out around one in the afternoon.".into()
                    });
                };
                let low = day.iter().map(price).fold(f64::MAX, f64::min);
                let high = day.iter().map(price).fold(f64::MIN, f64::max);
                let mean = day.iter().map(price).sum::<f64>() / day.len().max(1) as f64;
                Ok(if no {
                    format!(
                        "I morgen koster strømmen fra {} til {} {}. Snittet er {}.",
                        format_price(low, lang),
                        format_price(high, lang),
                        per_kwh(lang),
                        format_price(mean, lang)
                    )
                } else {
                    format!(
                        "Tomorrow, power costs from {} to {} {}. On average {}.",
                        format_price(low, lang),
                        format_price(high, lang),
                        per_kwh(lang),
                        format_price(mean, lang)
                    )
                })
            }
            PowerQuery::Cheapest(when) | PowerQuery::Dearest(when) => {
                let cheapest = matches!(query, PowerQuery::Cheapest(_));
                let tomorrow = today.checked_add(1.day())?;
                let mut intervals: Vec<Interval> = Vec::new();
                let (from, to) = match when {
                    When::Today => {
                        (now.timestamp(), today.checked_add(1.day())?.to_zoned(now.time_zone().clone())?.timestamp())
                    }
                    When::Tonight => (
                        now.timestamp(),
                        tomorrow.at(NIGHT_ENDS, 0, 0, 0).to_zoned(now.time_zone().clone())?.timestamp(),
                    ),
                    When::Tomorrow => (
                        tomorrow.to_zoned(now.time_zone().clone())?.timestamp(),
                        tomorrow.checked_add(1.day())?.to_zoned(now.time_zone().clone())?.timestamp(),
                    ),
                };
                if when != When::Tomorrow {
                    intervals.extend(self.day(today)?.unwrap_or_default());
                }
                if when != When::Today {
                    match self.day(tomorrow)? {
                        Some(day) => intervals.extend(day),
                        None if when == When::Tomorrow => {
                            return Ok(if no {
                                "Morgendagens strømpriser kommer rundt klokka ett.".into()
                            } else {
                                "Tomorrow's prices come out around one in the afternoon.".into()
                            });
                        }
                        None => {}
                    }
                }
                // The hour already under way still counts.
                let window: Vec<&Interval> =
                    intervals.iter().filter(|i| i.time_end > from && i.time_start < to).collect();
                let pick = if cheapest {
                    window.iter().min_by(|a, b| price(a).total_cmp(&price(b)))
                } else {
                    window.iter().max_by(|a, b| price(a).total_cmp(&price(b)))
                };
                let Some(pick) = pick else { bail!("no prices in the window") };
                let word = match (no, cheapest, when) {
                    (false, ..) => english_word(cheapest, when),
                    (true, true, When::Today) => "Billigst resten av dagen",
                    (true, true, When::Tonight) => "Billigst i natt",
                    (true, true, When::Tomorrow) => "Billigst i morgen",
                    (true, false, When::Today) => "Dyrest resten av dagen",
                    (true, false, When::Tonight) => "Dyrest i kveld og natt",
                    (true, false, When::Tomorrow) => "Dyrest i morgen",
                };
                let started = pick.time_start <= now.timestamp();
                Ok(match (no, started) {
                    (false, true) => format!("{word} is right now. It costs {}.", price_of(pick)),
                    (false, false) => format!("{word} is at {}. It costs {}.", at(pick), price_of(pick)),
                    (true, true) => format!("{word} er akkurat nå, {}.", price_of(pick)),
                    (true, false) => format!("{word} er klokka {}, {}.", at(pick), price_of(pick)),
                })
            }
            PowerQuery::Window { hours, when } => {
                let tomorrow = today.checked_add(1.day())?;
                let tz = now.time_zone().clone();
                let midnight = |date: Date| date.to_zoned(tz.clone()).map(|z| z.timestamp());
                let (from, to) = match when {
                    None => (now.timestamp(), Timestamp::MAX),
                    Some(When::Today) => (now.timestamp(), midnight(tomorrow)?),
                    Some(When::Tonight) => {
                        (now.timestamp(), tomorrow.at(NIGHT_ENDS, 0, 0, 0).to_zoned(tz.clone())?.timestamp())
                    }
                    Some(When::Tomorrow) => (midnight(tomorrow)?, midnight(tomorrow.checked_add(1.day())?)?),
                };
                let mut intervals: Vec<Interval> = Vec::new();
                if when != Some(When::Tomorrow) {
                    intervals.extend(self.day(today)?.unwrap_or_default());
                }
                let mut tomorrow_missing = false;
                if when != Some(When::Today) {
                    match self.day(tomorrow)? {
                        Some(day) => intervals.extend(day),
                        None => tomorrow_missing = true,
                    }
                }
                let published = if no {
                    "Morgendagens strømpriser kommer rundt klokka ett."
                } else {
                    "Tomorrow's prices come out around one in the afternoon."
                };
                if tomorrow_missing && when == Some(When::Tomorrow) {
                    return Ok(published.into());
                }
                let minutes = i64::from(hours) * 60;
                let Some(found) = cheapest_window(&intervals, from, to, minutes, price) else {
                    let short = if no {
                        format!("Jeg kjenner ikke prisene for {hours} timer i strekk ennå.")
                    } else {
                        format!("I don't know the prices for {hours} hours in a row yet.")
                    };
                    return Ok(if tomorrow_missing { format!("{short} {published}") } else { short });
                };
                let clock = |t: Timestamp| t.to_zoned(tz.clone()).strftime("%H:%M").to_string();
                let starts = if found.start <= now.timestamp() {
                    if no {
                        "nå".to_string()
                    } else {
                        "now".to_string()
                    }
                } else if no {
                    format!("klokka {}", clock(found.start))
                } else {
                    format!("at {}", clock(found.start))
                };
                // A start in tomorrow's day says so; the small hours read as tonight. Not when tomorrow
                // was asked for.
                let start_day = found.start.to_zoned(tz.clone());
                let next_day = start_day.date() == tomorrow && start_day.hour() >= 6 && when != Some(When::Tomorrow);
                let day = match (next_day, no) {
                    (false, _) => "",
                    (true, false) => " tomorrow",
                    (true, true) => " i morgen",
                };
                let ends = clock(found.end);
                let average = format!("{} {}", format_price(found.average, lang), per_kwh(lang));
                Ok(match (no, hours) {
                    // The window, then the price: the first sentence has a bounded shape, rendered
                    // ahead after each refresh (`DATA_QUESTIONS`), and "It costs" is kept as well.
                    (false, 1) => {
                        format!("The cheapest hour starts {starts}{day} and ends at {ends}. On average, it costs {average}.")
                    }
                    (false, n) => format!(
                        "The cheapest {n} hours start {starts}{day} and end at {ends}. On average, it costs {average}."
                    ),
                    (true, 1) => {
                        format!(
                            "Den billigste timen starter {starts}{day} og slutter klokka {ends}, i snitt {average}."
                        )
                    }
                    (true, n) => format!(
                        "De billigste {n} timene starter {starts}{day} og slutter klokka {ends}, i snitt {average}."
                    ),
                })
            }
        }
    }

    /// Fetches today's and tomorrow's prices unless kept; tomorrow's are published around 13:00.
    pub fn refresh(&mut self) -> Result<()> {
        let today = Zoned::now().date();
        self.day(today)?;
        self.day(today.tomorrow()?)?;
        Ok(())
    }

    /// Whether the prices `query` needs are kept, so its answer needs no request.
    pub fn is_ready(&self, query: PowerQuery) -> bool {
        let today = Zoned::now().date();
        let kept = |date: Date| self.days.iter().any(|(d, ..)| *d == date);
        let tomorrow = matches!(
            query,
            PowerQuery::Tomorrow
                | PowerQuery::Cheapest(When::Tonight | When::Tomorrow)
                | PowerQuery::Dearest(When::Tonight | When::Tomorrow)
                | PowerQuery::Window { when: None | Some(When::Tonight | When::Tomorrow), .. }
        );
        kept(today) && (!tomorrow || today.tomorrow().is_ok_and(kept))
    }

    /// One day's prices, cached; `None` when not published yet.
    fn day(&mut self, date: Date) -> Result<Option<Vec<Interval>>> {
        self.days.retain(|(d, prices, at)| {
            *d >= date.checked_sub(1.day()).unwrap_or(date) && (prices.is_some() || at.elapsed() < RETRY_MISSING)
        });
        if let Some((_, prices, _)) = self.days.iter().find(|(d, ..)| *d == date) {
            return Ok(prices.clone());
        }
        let url = format!("{URL}/{}/{}_{}.json", date.year(), date.strftime("%m-%d"), self.area);
        let prices = match self.agent.get(&url).call() {
            Ok(mut response) => {
                Some(response.body_mut().read_json::<Vec<Interval>>().context("parsing electricity prices")?)
            }
            Err(ureq::Error::StatusCode(404)) => None,
            Err(error) => return Err(error).context("electricity prices (hvakosterstrommen.no)"),
        };
        self.days.push((date, prices.clone(), Instant::now()));
        Ok(prices)
    }
}

/// The cheapest stretch found: from, to and its average price.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Stretch {
    start: Timestamp,
    end: Timestamp,
    average: f64,
}

/// The cheapest `minutes` of prices in a row between `from` and `to`, averaged by time (prices
/// come per hour or per quarter hour). The interval under way at `from` may start it. `None` when
/// the known prices hold no stretch that long, or have a gap in it.
fn cheapest_window(
    intervals: &[Interval],
    from: Timestamp,
    to: Timestamp,
    minutes: i64,
    price: impl Fn(&Interval) -> f64,
) -> Option<Stretch> {
    let mut sorted: Vec<&Interval> = intervals.iter().filter(|i| i.time_end > from && i.time_start < to).collect();
    sorted.sort_by_key(|i| i.time_start);
    sorted.dedup_by_key(|i| i.time_start);
    let wanted = minutes * 60;
    let mut best: Option<Stretch> = None;
    for first in 0..sorted.len() {
        let start = sorted[first].time_start;
        let (mut covered, mut cost, mut end) = (0i64, 0.0, start);
        for interval in &sorted[first..] {
            if interval.time_start != end || covered >= wanted {
                break;
            }
            let seconds = (interval.time_end.as_second() - interval.time_start.as_second()).min(wanted - covered);
            covered += seconds;
            cost += price(interval) * seconds as f64;
            end = interval.time_start + jiff::SignedDuration::from_secs(seconds);
        }
        if covered < wanted || end > to {
            continue;
        }
        let average = cost / covered as f64;
        // The earliest of equally cheap stretches.
        if best.is_none_or(|b| average < b.average - 1e-9) {
            best = Some(Stretch { start, end, average });
        }
    }
    best
}

fn english_word(cheapest: bool, when: When) -> &'static str {
    match (cheapest, when) {
        (true, When::Today) => "Cheapest for the rest of today",
        (true, When::Tonight) => "Cheapest tonight",
        (true, When::Tomorrow) => "Cheapest tomorrow",
        (false, When::Today) => "Most expensive for the rest of today",
        (false, When::Tonight) => "Most expensive tonight",
        (false, When::Tomorrow) => "Most expensive tomorrow",
    }
}

/// Every English price sentence up to 5 kroner, and when the cheapest and dearest quarter hours
/// are, to render ahead.
pub fn english_sentences() -> Vec<String> {
    let mut sentences = Vec::new();
    for cents in 1..500 {
        let price = format!("{} {}", format_price(cents as f64 / 100.0, Lang::English), per_kwh(Lang::English));
        sentences.push(format!("Right now, power costs {price}."));
        sentences.push(format!("It costs {price}."));
    }
    for cheapest in [true, false] {
        for when in [When::Today, When::Tonight, When::Tomorrow] {
            let word = english_word(cheapest, when);
            sentences.push(format!("{word} is right now."));
            sentences.extend((0..96).map(|q| format!("{word} is at {:02}:{:02}.", q / 4, q % 4 * 15)));
        }
    }
    sentences
}

/// "1.44 kroner" / "1,44 kroner", or "36 øre" under a krone.
fn format_price(nok: f64, lang: Lang) -> String {
    if nok.abs() < 1.0 {
        format!("{} øre", (nok * 100.0).round() as i64)
    } else {
        let text = format!("{nok:.2}");
        let text = if lang == Lang::Norwegian { text.replace('.', ",") } else { text };
        format!("{text} kroner")
    }
}

fn per_kwh(lang: Lang) -> &'static str {
    if lang == Lang::Norwegian {
        "per kilowattime"
    } else {
        "per kilowatt hour"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ready_when_the_days_asked_about_are_kept() {
        let mut power = Power::new("test", "NO1");
        assert!(!power.is_ready(PowerQuery::Now));
        let today = Zoned::now().date();
        power.days.push((today, Some(Vec::new()), Instant::now()));
        assert!(power.is_ready(PowerQuery::Now));
        assert!(power.is_ready(PowerQuery::Cheapest(When::Today)));
        assert!(!power.is_ready(PowerQuery::Cheapest(When::Tomorrow)));
        // Not published yet is kept too: the answer says so without a request.
        power.days.push((today.tomorrow().unwrap(), None, Instant::now()));
        assert!(power.is_ready(PowerQuery::Dearest(When::Tonight)));
    }

    #[test]
    fn questions() {
        assert_eq!(parse("what does electricity cost right now?"), Some(PowerQuery::Now));
        assert_eq!(parse("what's the power price"), Some(PowerQuery::Now));
        assert_eq!(parse("hva koster strømmen nå?"), Some(PowerQuery::Now));
        assert_eq!(parse("when is electricity cheapest tonight"), Some(PowerQuery::Cheapest(When::Tonight)));
        assert_eq!(parse("når er strømmen billigst i morgen?"), Some(PowerQuery::Cheapest(When::Tomorrow)));
        assert_eq!(parse("when is power cheapest today"), Some(PowerQuery::Cheapest(When::Today)));
        assert_eq!(parse("når er strømmen dyrest i dag"), Some(PowerQuery::Dearest(When::Today)));
        assert_eq!(parse("turn off the power to the tv"), None);
        assert_eq!(parse("what's the electricity price tomorrow"), Some(PowerQuery::Tomorrow));
        assert_eq!(parse("hva koster strømmen i morgen"), Some(PowerQuery::Tomorrow));
        assert_eq!(parse("how expensive is power"), Some(PowerQuery::Now));
        assert_eq!(parse("how expensive is power tonight"), Some(PowerQuery::Dearest(When::Tonight)));
        assert_eq!(parse("is electricity expensive now"), Some(PowerQuery::Now));
        assert_eq!(parse("when is electricity most expensive"), Some(PowerQuery::Dearest(When::Today)));
        assert_eq!(parse("what's the weather"), None);
    }

    #[test]
    fn stretches_of_hours() {
        let window = |hours, when| Some(PowerQuery::Window { hours, when });
        assert_eq!(parse("when is power cheapest for 3 hours"), window(3, None));
        assert_eq!(parse("what are the cheapest three hours of electricity tomorrow"), window(3, Some(When::Tomorrow)));
        assert_eq!(parse("når er strømmen billigst i tre timer i natt"), window(3, Some(When::Tonight)));
        assert_eq!(parse("billigste fire timer med strøm"), window(4, None));
        assert_eq!(parse("when should I run the dishwasher"), window(3, None));
        assert_eq!(parse("best time to run the washing machine tonight"), window(2, Some(When::Tonight)));
        assert_eq!(parse("når bør jeg kjøre oppvaskmaskinen"), window(3, None));
        assert_eq!(parse("when is it cheapest to charge the car"), window(4, None));
        assert_eq!(parse("when should I run the dishwasher for two hours"), window(2, None));
        // One hour and the cheapest hour stay what they were.
        assert_eq!(parse("when is power cheapest"), Some(PowerQuery::Cheapest(When::Today)));
        assert_eq!(parse("what's the cheapest hour of electricity"), Some(PowerQuery::Cheapest(When::Today)));
        // About the machine, not the price.
        assert_eq!(parse("when is the dishwasher done"), None);
        assert_eq!(parse("turn on the dishwasher"), None);
    }

    fn interval(start: Timestamp, minutes: i64, nok: f64) -> Interval {
        Interval { nok_per_kwh: nok, time_start: start, time_end: start + jiff::SignedDuration::from_mins(minutes) }
    }

    /// Hourly prices from `start`, one per hour.
    fn hours_from(start: Timestamp, prices: &[f64]) -> Vec<Interval> {
        prices
            .iter()
            .enumerate()
            .map(|(i, &p)| interval(start + jiff::SignedDuration::from_hours(i as i64), 60, p))
            .collect()
    }

    #[test]
    fn the_cheapest_stretch_is_found() {
        let start: Timestamp = "2026-11-01T18:00:00Z".parse().unwrap();
        let prices = hours_from(start, &[1.0, 0.2, 0.9, 0.3, 0.3, 0.4, 1.0]);
        let same = |i: &Interval| i.nok_per_kwh;
        // A single cheap hour does not win over three cheap ones in a row.
        let found = cheapest_window(&prices, start, Timestamp::MAX, 180, same).unwrap();
        assert_eq!(found.start, start + jiff::SignedDuration::from_hours(3));
        assert_eq!(found.end, start + jiff::SignedDuration::from_hours(6));
        assert!((found.average - 1.0 / 3.0).abs() < 1e-9);
        // Too short a stretch of prices, and a gap in them.
        assert_eq!(cheapest_window(&prices, start, Timestamp::MAX, 8 * 60, same), None);
        let mut gap = prices.clone();
        gap.remove(4);
        let after_gap = start + jiff::SignedDuration::from_hours(1);
        assert_eq!(cheapest_window(&gap, start, Timestamp::MAX, 180, same).map(|f| f.start), Some(after_gap));
        // The hour under way may start it; one before `from` may not.
        let from = start + jiff::SignedDuration::from_mins(3 * 60 + 30);
        assert_eq!(cheapest_window(&prices, from, Timestamp::MAX, 180, same).unwrap().start, found.start);
        let from = start + jiff::SignedDuration::from_hours(4);
        assert_eq!(cheapest_window(&prices, from, Timestamp::MAX, 180, same).unwrap().start, from);
        // Quarter hours are averaged by time, like hours.
        let quarters: Vec<Interval> = (0..12)
            .map(|q| interval(start + jiff::SignedDuration::from_mins(q * 15), 15, if q < 4 { 1.0 } else { 0.5 }))
            .collect();
        let found = cheapest_window(&quarters, start, Timestamp::MAX, 60, same).unwrap();
        assert_eq!(found.start, start + jiff::SignedDuration::from_hours(1));
        // Within the end of the window asked for.
        let to = start + jiff::SignedDuration::from_hours(5);
        let found = cheapest_window(&prices, start, to, 180, same).unwrap();
        assert!(found.end <= to);
    }

    #[test]
    fn stretches_cross_midnight_and_wait_for_tomorrow() {
        let now = Zoned::now();
        let tz = now.time_zone().clone();
        let today = now.date();
        let tomorrow = today.tomorrow().unwrap();
        let midnight = |d: Date| d.to_zoned(tz.clone()).unwrap().timestamp();
        let mut power = Power::new("test", "NO1");
        // Today dear all day; tomorrow cheap from 01:00 to 04:00.
        power.days.push((today, Some(hours_from(midnight(today), &[2.0; 24])), Instant::now()));
        let mut tomorrows = [2.0; 24];
        tomorrows[1..4].fill(0.4);
        power.days.push((tomorrow, Some(hours_from(midnight(tomorrow), &tomorrows)), Instant::now()));
        let window = |hours, when| PowerQuery::Window { hours, when };
        assert_eq!(
            power.answer(window(3, None), Lang::English).unwrap(),
            "The cheapest 3 hours start at 01:00 and end at 04:00. On average, it costs 50 øre per kilowatt hour."
        );
        assert_eq!(
            power.answer(window(3, Some(When::Tomorrow)), Lang::Norwegian).unwrap(),
            "De billigste 3 timene starter klokka 01:00 og slutter klokka 04:00, i snitt 50 øre per kilowattime."
        );
        // A start in tomorrow's daytime says so.
        tomorrows = [2.0; 24];
        tomorrows[13] = 0.4;
        power.days[1].1 = Some(hours_from(midnight(tomorrow), &tomorrows));
        assert_eq!(
            power.answer(window(1, None), Lang::English).unwrap(),
            "The cheapest hour starts at 13:00 tomorrow and ends at 14:00. On average, it costs 50 øre per kilowatt \
             hour."
        );
        // Tomorrow not published: tomorrow's stretch waits for it.
        power.days[1].1 = None;
        assert_eq!(
            power.answer(window(3, Some(When::Tomorrow)), Lang::English).unwrap(),
            "Tomorrow's prices come out around one in the afternoon."
        );
        assert!(power.is_ready(window(3, None)));
        for sentence in [
            "The cheapest 3 hours start at 01:00 and end at 04:00.",
            "The cheapest hour starts now and ends at 14:00.",
            "On average, it costs 50 øre per kilowatt hour.",
        ] {
            assert!(crate::speech_cache::CachedTts::cacheable(sentence), "{sentence}");
        }
    }

    #[test]
    fn areas() {
        assert_eq!(area_for(59.91, 10.75), "NO1"); // Oslo
        assert_eq!(area_for(60.39, 5.32), "NO5"); // Bergen
        assert_eq!(area_for(58.97, 5.73), "NO2"); // Stavanger
        assert_eq!(area_for(58.15, 8.0), "NO2"); // Kristiansand
        assert_eq!(area_for(63.43, 10.39), "NO3"); // Trondheim
        assert_eq!(area_for(69.65, 18.96), "NO4"); // Tromsø
        assert_eq!(area_for(60.79, 11.07), "NO1"); // Hamar
    }

    #[test]
    fn english_sentences_are_kept() {
        let ahead = english_sentences();
        assert!(ahead.contains(&"Right now, power costs 1.44 kroner per kilowatt hour.".to_string()));
        assert!(ahead.contains(&"It costs 36 øre per kilowatt hour.".to_string()));
        assert!(ahead.contains(&"Cheapest tonight is at 03:15.".to_string()));
        assert!(ahead.iter().all(|s| crate::speech_cache::CachedTts::cacheable(s)), "not all cacheable");
    }

    #[test]
    fn prices_are_spoken() {
        assert_eq!(format_price(1.444, Lang::English), "1.44 kroner");
        assert_eq!(format_price(1.444, Lang::Norwegian), "1,44 kroner");
        assert_eq!(format_price(0.357, Lang::English), "36 øre");
    }
}
