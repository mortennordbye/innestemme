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
    let power = words.iter().any(|w| POWER_WORDS.contains(&w.as_str()))
        || ((has("power") || has("energy")) && words.iter().any(|w| PRICE_WORDS.contains(&w.as_str())));
    if !power {
        return None;
    }
    let pair = |a: &str, b: &str| words.windows(2).any(|p| p[0] == a && p[1] == b);
    let when = if has("tomorrow") || has("imorgen") || pair("i", "morgen") {
        When::Tomorrow
    } else if has("tonight") || has("night") || pair("i", "kveld") || pair("i", "natt") || has("natt") {
        When::Tonight
    } else {
        When::Today
    };
    if words.iter().any(|w| CHEAP_WORDS.contains(&w.as_str())) || pair("best", "time") {
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
