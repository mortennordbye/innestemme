//! Public transport departures from Entur (every operator in Norway, real time where available):
//! "when's the next bus", "neste trikk til sentrum". Without a destination: the departures from
//! the stops nearest the home (or the ones the settings name) that can still be reached on foot.
//! With one: a trip from home, which also finds lines that only pass through it and says when to
//! leave. The trip query follows ruter-cli's.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::json;

use crate::dialog::normalize;
use crate::geo::{Location, ENTUR_CLIENT};
use crate::lang::Lang;

const JOURNEY_PLANNER: &str = "https://api.entur.io/journey-planner/v3/graphql";
/// Stops within this walk of the home count as "nearby".
const NEARBY_METRES: u32 = 700;
const NEARBY_STOPS: usize = 4;
/// Walking pace for "can I still make it": about 4.8 km/h.
const WALK_METRES_PER_MINUTE: f64 = 80.0;
/// Longest walk to or from a stop in a trip.
const MAX_WALK_MINUTES: u32 = 15;
/// Departures looked at per stop, and how far ahead.
const DEPARTURES: u32 = 40;
/// For one line, or one named stop that may be a busy hub.
const MORE_DEPARTURES: u32 = 120;
const WINDOW_SECONDS: u32 = 3 * 3600;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    #[default]
    Any,
    Bus,
    Tram,
    Metro,
    Rail,
    Ferry,
}

impl Mode {
    /// Entur's `transportMode` values.
    fn matches(self, mode: &str) -> bool {
        match self {
            Mode::Any => true,
            Mode::Bus => mode == "bus" || mode == "coach",
            Mode::Tram => mode == "tram",
            Mode::Metro => mode == "metro",
            Mode::Rail => mode == "rail",
            Mode::Ferry => mode == "water",
        }
    }

    fn word(self, lang: Lang) -> &'static str {
        match (self, lang) {
            (Mode::Any, Lang::English) => "departure",
            (Mode::Bus, Lang::English) => "bus",
            (Mode::Tram, Lang::English) => "tram",
            (Mode::Metro, Lang::English) => "metro",
            (Mode::Rail, Lang::English) => "train",
            (Mode::Ferry, Lang::English) => "boat",
            (Mode::Any, Lang::Norwegian) => "avgang",
            (Mode::Bus, Lang::Norwegian) => "buss",
            (Mode::Tram, Lang::Norwegian) => "trikk",
            (Mode::Metro, Lang::Norwegian) => "t-bane",
            (Mode::Rail, Lang::Norwegian) => "tog",
            (Mode::Ferry, Lang::Norwegian) => "båt",
        }
    }
}

/// "The next bus (to Sentrum)", "the next 31", "next train from Nationaltheatret".
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TransitQuery {
    pub mode: Mode,
    /// Matched against the sign on the vehicle ("Jernbanetorget").
    pub destination: Option<String>,
    /// The line's public code, as on the sign: "31", "n12", "re10".
    pub line: Option<String>,
    /// A stop named to leave from instead of the ones near home.
    pub from: Option<String>,
}

const MODES: &[(&str, Mode)] = &[
    ("bus", Mode::Bus),
    ("buses", Mode::Bus),
    ("buss", Mode::Bus),
    ("bussen", Mode::Bus),
    ("busser", Mode::Bus),
    ("tram", Mode::Tram),
    ("trams", Mode::Tram),
    ("streetcar", Mode::Tram),
    ("trikk", Mode::Tram),
    ("trikken", Mode::Tram),
    // Whisper's usual spelling of "trikk"; only counts next to "neste", "når går" and the like.
    ("trykk", Mode::Tram),
    ("trykken", Mode::Tram),
    ("metro", Mode::Metro),
    ("subway", Mode::Metro),
    ("underground", Mode::Metro),
    ("tbane", Mode::Metro),
    ("tbanen", Mode::Metro),
    ("train", Mode::Rail),
    ("trains", Mode::Rail),
    ("tog", Mode::Rail),
    ("toget", Mode::Rail),
    ("ferry", Mode::Ferry),
    ("boat", Mode::Ferry),
    ("ferje", Mode::Ferry),
    ("ferja", Mode::Ferry),
    ("ferga", Mode::Ferry),
    ("båt", Mode::Ferry),
    ("båten", Mode::Ferry),
    ("båtbussen", Mode::Ferry),
];
const DEPARTURE_WORDS: &[&str] = &["departure", "departures", "avgang", "avgangen", "avganger", "avgangene"];
const WHEN_WORDS: &[&str] = &[
    "next", "when", "when's", "leave", "leaves", "leaving", "depart", "departs", "go", "goes", "come", "comes",
    "coming", "neste", "når", "går", "gå", "kommer", "drar",
];
const TOWARDS: &[&str] = &["to", "towards", "into", "for", "til", "mot"];
/// Dropped from the end of a destination: "to the city centre please".
const DESTINATION_FILLER: &[&str] = &[
    "please", "takk", "the", "now", "nå", "today", "i", "dag", "leave", "leaves", "go", "goes", "depart", "departs",
    "come", "comes", "går", "kommer", "drar",
];

const FROM: &[&str] = &["from", "fra"];
/// "From home" is the default, not a stop.
const HOME_WORDS: &[&str] = &["home", "here", "hjemme", "hjem", "hjemmefra", "her"];
/// Words just before a line number: "the next 31", "bus 31", "linje 37".
const LINE_LEAD: &[&str] = &["next", "neste", "the", "line", "linje", "linjen", "number", "nummer", "route", "rute"];
/// Words after a number that make it a count or a time, not a line: "the next 3 days".
const NOT_A_LINE: &[&str] = &[
    "minute", "minutes", "minutt", "minutter", "second", "seconds", "sekunder", "hour", "hours", "time", "timer",
    "timene", "day", "days", "dag", "dager", "dagene", "week", "weeks", "uke", "uker", "song", "songs", "sang",
    "sanger", "track", "tracks", "times", "percent", "prosent", "degrees", "grader", "o'clock", "am", "pm", "of",
];
/// A request about music, where "skip the next 2" is not a line.
const MUSIC_WORDS: &[&str] = &["skip", "song", "songs", "track", "play", "spill", "sang", "låt", "hopp"];

/// A line's public code as the transcriber writes it: "31", "n12", "re10", "31e".
fn line_code(word: &str) -> bool {
    let letters = word.chars().take_while(char::is_ascii_alphabetic).count();
    let digits = word[letters..].chars().take_while(char::is_ascii_digit).count();
    let rest = &word[letters + digits..];
    letters <= 2 && (1..=3).contains(&digits) && rest.len() <= 1 && rest.chars().all(|c| c.is_ascii_alphabetic())
}

fn is_mode(word: &str) -> bool {
    MODES.iter().any(|(m, _)| *m == word)
}

/// "When's the next bus", "next tram to Majorstuen", "når går neste buss til sentrum", "when is
/// the next 31 bus", "next train from Nationaltheatret", "når går neste 37".
pub fn parse(text: &str) -> Option<TransitQuery> {
    // "t-bane" loses its hyphen in `normalize`; "t bane" is two words.
    let joined = text.to_lowercase().replace("t-bane", "tbane").replace("t bane", "tbane");
    let mut words: Vec<String> = Vec::new();
    for word in joined.split_whitespace().map(normalize).filter(|w| !w.is_empty()) {
        // "37-buss" loses its hyphen too: the line, then the mode.
        let digits = word.chars().take_while(char::is_ascii_digit).count();
        if digits > 0 && is_mode(&word[digits..]) {
            words.push(word[..digits].to_owned());
            words.push(word[digits..].to_owned());
        } else {
            words.push(word);
        }
    }
    let mode = words.iter().find_map(|w| MODES.iter().find(|(m, _)| m == w).map(|&(_, mode)| mode));
    let departures = words.iter().any(|w| DEPARTURE_WORDS.contains(&w.as_str()));
    let music = mode.is_none() && words.iter().any(|w| MUSIC_WORDS.contains(&w.as_str()));
    let line_at = (0..words.len()).find(|&i| {
        let after = words.get(i + 1).map(String::as_str);
        let before = i.checked_sub(1).map(|b| words[b].as_str());
        !music
            && line_code(&words[i])
            && !after.is_some_and(|a| NOT_A_LINE.contains(&a))
            && (after.is_some_and(is_mode) || before.is_some_and(|b| LINE_LEAD.contains(&b) || is_mode(b)))
    });
    if mode.is_none() && !departures && line_at.is_none() {
        return None;
    }
    // A question about the next one, not "I took the bus to work".
    if !departures && !words.iter().any(|w| WHEN_WORDS.contains(&w.as_str())) {
        return None;
    }
    // The words after `start` up to one of `end`, with filler dropped at both ends.
    let phrase = |start: usize, end: &[&str]| -> Option<String> {
        let mut found: Vec<&str> =
            words.get(start..)?.iter().map(String::as_str).take_while(|w| !end.contains(w)).collect();
        while found.first().is_some_and(|w| *w == "the") {
            found.remove(0);
        }
        while found.last().is_some_and(|w| DESTINATION_FILLER.contains(w)) {
            found.pop();
        }
        (!found.is_empty()).then(|| found.join(" "))
    };
    let mode_at = words.iter().position(|w| is_mode(w) || DEPARTURE_WORDS.contains(&w.as_str())).or(line_at);
    let destination = mode_at.and_then(|at| {
        let to = words[at..].iter().position(|w| TOWARDS.contains(&w.as_str()))? + at;
        phrase(to + 1, FROM)
    });
    let from = words
        .iter()
        .position(|w| FROM.contains(&w.as_str()))
        .and_then(|at| phrase(at + 1, TOWARDS))
        .filter(|stop| !HOME_WORDS.contains(&stop.as_str()));
    Some(TransitQuery { mode: mode.unwrap_or(Mode::Any), destination, line: line_at.map(|at| words[at].clone()), from })
}

/// Places a request says for the town centre, and what the signs say instead.
fn centre(destination: &str) -> bool {
    matches!(
        destination,
        "town" | "the city" | "city" | "city centre" | "city center" | "downtown" | "byen" | "sentrum"
    )
}

pub struct Transit {
    agent: ureq::Agent,
    /// Stop place ids ("NSR:StopPlace:58404") or names from the settings; nearest to home if empty.
    configured: Vec<String>,
    home: Option<Location>,
    stops: Vec<NearStop>,
}

#[derive(Debug, Clone)]
struct NearStop {
    id: String,
    name: String,
    /// Minutes on foot from home; 0 when not known.
    walk: i64,
}

#[derive(Deserialize)]
struct Call {
    #[serde(rename = "expectedDepartureTime")]
    expected: jiff::Timestamp,
    #[serde(default)]
    cancellation: bool,
    #[serde(rename = "destinationDisplay")]
    destination: Option<Display>,
    #[serde(rename = "serviceJourney")]
    journey: Journey,
}

#[derive(Deserialize)]
struct Display {
    #[serde(rename = "frontText")]
    front_text: Option<String>,
}

#[derive(Deserialize)]
struct Journey {
    line: Line,
}

#[derive(Deserialize)]
struct Line {
    #[serde(rename = "publicCode")]
    public_code: Option<String>,
    #[serde(rename = "transportMode")]
    mode: Option<String>,
}

#[derive(Deserialize)]
struct Stop {
    id: String,
    name: String,
    #[serde(rename = "estimatedCalls", default)]
    calls: Vec<Call>,
    /// Minutes on foot from home: departures sooner than this cannot be caught.
    #[serde(skip)]
    walk: i64,
}

#[derive(Deserialize)]
struct Pattern {
    #[serde(rename = "expectedStartTime")]
    start: jiff::Timestamp,
    #[serde(default)]
    legs: Vec<TripLeg>,
}

#[derive(Deserialize)]
struct TripLeg {
    mode: String,
    #[serde(rename = "expectedStartTime")]
    start: jiff::Timestamp,
    #[serde(rename = "fromPlace")]
    from: Named,
    line: Option<Line>,
}

#[derive(Deserialize)]
struct Named {
    name: String,
}

/// One departure as it is spoken.
#[derive(Debug, Clone, PartialEq)]
struct Departure {
    stop: String,
    line: String,
    mode: String,
    towards: String,
    minutes: i64,
    clock: String,
}

impl Transit {
    pub fn new(user_agent: &str, home: Option<Location>, configured: Vec<String>) -> Self {
        Self { agent: crate::geo::agent(user_agent), configured, home, stops: Vec::new() }
    }

    pub fn is_configured(&self) -> bool {
        self.home.is_some() || !self.configured.is_empty()
    }

    fn graphql(&self, query: &str) -> Result<serde_json::Value> {
        let mut answer: serde_json::Value = self
            .agent
            .post(JOURNEY_PLANNER)
            .header("ET-Client-Name", ENTUR_CLIENT)
            .send_json(json!({ "query": query }))
            .context("Entur request")?
            .body_mut()
            .read_json()?;
        if let Some(errors) = answer.get("errors") {
            bail!("Entur answered with errors: {errors}");
        }
        Ok(answer["data"].take())
    }

    /// The stops to look at, resolved once: configured ids or names, else the nearest to home.
    fn stops(&mut self) -> Result<&[NearStop]> {
        if self.stops.is_empty() {
            self.stops = if !self.configured.is_empty() {
                self.configured
                    .clone()
                    .iter()
                    .map(|s| self.stop_by_name(s))
                    .collect::<Result<Vec<_>>>()?
                    .into_iter()
                    .flatten()
                    .collect()
            } else if let Some(home) = &self.home {
                let data = self.graphql(&format!(
                    "{{ nearest(latitude: {}, longitude: {}, maximumDistance: {NEARBY_METRES}, maximumResults: {NEARBY_STOPS}, \
                     filterByPlaceTypes: [stopPlace], multiModalMode: parent) {{ edges {{ node {{ distance place {{ ... on StopPlace {{ id name }} }} }} }} }} }}",
                    home.latitude, home.longitude
                ))?;
                data["nearest"]["edges"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|e| {
                        let place = &e["node"]["place"];
                        let walk =
                            (e["node"]["distance"].as_f64().unwrap_or(0.0) / WALK_METRES_PER_MINUTE).ceil() as i64;
                        Some(NearStop {
                            id: place["id"].as_str()?.to_owned(),
                            name: place["name"].as_str()?.to_owned(),
                            walk,
                        })
                    })
                    .collect()
            } else {
                bail!("no home address or stops configured");
            };
            let names: Vec<String> = self.stops.iter().map(|s| format!("{} ({} min walk)", s.name, s.walk)).collect();
            tracing::info!(stops = ?names, "transit stops");
        }
        Ok(&self.stops)
    }

    /// "NSR:StopPlace:58404" as is; a name through Entur's geocoder.
    fn stop_by_name(&self, stop: &str) -> Result<Option<NearStop>> {
        if stop.starts_with("NSR:") {
            return Ok(Some(NearStop { id: stop.to_owned(), name: stop.to_owned(), walk: 0 }));
        }
        Ok(self.find_stop(stop)?.map(|(id, name, ..)| NearStop { id, name, walk: 0 }))
    }

    /// A stop by (possibly misheard) name, nearest the home first: id, name, coordinates.
    fn find_stop(&self, name: &str) -> Result<Option<(String, String, f64, f64)>> {
        let mut call = self
            .agent
            .get("https://api.entur.io/geocoder/v1/autocomplete")
            .header("ET-Client-Name", ENTUR_CLIENT)
            .query("text", name)
            .query("layers", "venue")
            .query("size", "1")
            .query("lang", "no");
        if let Some(home) = &self.home {
            call = call
                .query("focus.point.lat", home.latitude.to_string())
                .query("focus.point.lon", home.longitude.to_string());
        }
        let found: serde_json::Value = call.call().context("stop lookup (Entur geocoder)")?.body_mut().read_json()?;
        let feature = &found["features"][0];
        let props = &feature["properties"];
        let [lon, lat] = [&feature["geometry"]["coordinates"][0], &feature["geometry"]["coordinates"][1]];
        Ok(match (props["id"].as_str(), lat.as_f64(), lon.as_f64()) {
            (Some(id), Some(lat), Some(lon)) => {
                Some((id.to_owned(), props["name"].as_str().unwrap_or(name).to_owned(), lat, lon))
            }
            _ => None,
        })
    }

    /// "Next tram to Majorstuen": trips from home, like ruter-cli's.
    fn trip(&self, destination: &str, mode: Mode, lang: Lang) -> Result<String> {
        let home = self.home.as_ref().context("a trip needs the home address")?;
        // "Into town" is the home town's centre.
        let wanted = if centre(destination) { home.label.as_str() } else { destination };
        let Some((_, name, lat, lon)) = self.find_stop(wanted)? else {
            return Ok(match lang {
                Lang::English => format!("I couldn't find a stop called {destination}."),
                Lang::Norwegian => format!("Jeg fant ingen holdeplass som heter {destination}."),
            });
        };
        let modes = match mode {
            Mode::Any => "bus coach tram metro rail water",
            Mode::Bus => "bus coach",
            Mode::Tram => "tram",
            Mode::Metro => "metro",
            Mode::Rail => "rail",
            Mode::Ferry => "water",
        };
        let modes: Vec<String> = modes.split(' ').map(|m| format!("{{transportMode: {m}}}")).collect();
        let data = self.graphql(&format!(
            "{{ trip(from: {{coordinates: {{latitude: {}, longitude: {}}}}}, to: {{coordinates: {{latitude: {lat}, longitude: {lon}}}}}, \
             numTripPatterns: 3, maxAccessEgressDurationForMode: [{{streetMode: foot, duration: \"PT{MAX_WALK_MINUTES}M\"}}], \
             modes: {{accessMode: foot, egressMode: foot, transportModes: [{}]}}) {{ tripPatterns {{ expectedStartTime \
             legs {{ mode expectedStartTime fromPlace {{ name }} line {{ publicCode transportMode }} }} }} }} }}",
            home.latitude,
            home.longitude,
            modes.join(", ")
        ))?;
        let patterns: Vec<Pattern> = serde_json::from_value(data["trip"]["tripPatterns"].clone())?;
        Ok(trip_answer(&patterns, &name, mode, jiff::Timestamp::now(), lang))
    }

    /// The spoken answer.
    pub fn next(&mut self, query: &TransitQuery, lang: Lang) -> Result<String> {
        if let Some(from) = &query.from {
            return self.departures_at_stop(from, query, lang);
        }
        // A line named with a destination is that line's departures towards it, not a trip that may
        // take another line.
        if let (Some(destination), Some(_), None) = (&query.destination, &self.home, &query.line) {
            return self.trip(destination, query.mode, lang);
        }
        // One line among a busy stop's departures needs more of them.
        let count = if query.line.is_some() { MORE_DEPARTURES } else { DEPARTURES };
        let Some(found) = self.calls(count)? else {
            return Ok(match lang {
                Lang::English => "I found no stops near home.".into(),
                Lang::Norwegian => "Jeg fant ingen holdeplasser i nærheten.".into(),
            });
        };
        Ok(answer(&found, query, jiff::Timestamp::now(), lang))
    }

    /// "Next train from Nationaltheatret": the departures from a stop found by name.
    fn departures_at_stop(&self, name: &str, query: &TransitQuery, lang: Lang) -> Result<String> {
        let Some((id, found_name, ..)) = self.find_stop(name)? else {
            return Ok(match lang {
                Lang::English => format!("I couldn't find a stop called {name}."),
                Lang::Norwegian => format!("Jeg fant ingen holdeplass som heter {name}."),
            });
        };
        let stop = NearStop { id, name: found_name, walk: 0 };
        let found = self.departures_from(std::slice::from_ref(&stop), MORE_DEPARTURES)?;
        Ok(answer(&found, query, jiff::Timestamp::now(), lang))
    }

    /// The nearest stop's next departures, for the TV screen: `None` without stops or departures.
    pub fn board(&mut self) -> Result<Option<serde_json::Value>> {
        let Some(found) = self.calls(DEPARTURES)? else { return Ok(None) };
        Ok(board(&found, jiff::Timestamp::now()))
    }

    /// Departures from the stops, nearest first, each with its walk; `None` without stops.
    fn calls(&mut self, count: u32) -> Result<Option<Vec<Stop>>> {
        let stops = self.stops()?.to_vec();
        if stops.is_empty() {
            return Ok(None);
        }
        self.departures_from(&stops, count).map(Some)
    }

    /// The next `count` departures from each stop, in the order given, each with its walk.
    fn departures_from(&self, stops: &[NearStop], count: u32) -> Result<Vec<Stop>> {
        let ids: Vec<String> = stops.iter().map(|s| format!("{:?}", s.id)).collect();
        let data = self.graphql(&format!(
            "{{ stopPlaces(ids: [{}]) {{ id name estimatedCalls(numberOfDepartures: {count}, timeRange: {WINDOW_SECONDS}) {{ \
             expectedDepartureTime cancellation destinationDisplay {{ frontText }} serviceJourney {{ line {{ publicCode transportMode }} }} }} }} }}",
            ids.join(", ")
        ))?;
        let found: Vec<Option<Stop>> = serde_json::from_value(data["stopPlaces"].clone())?;
        let mut found: Vec<Stop> = found.into_iter().flatten().collect();
        for stop in &mut found {
            stop.walk = stops.iter().find(|s| s.id == stop.id).map_or(0, |s| s.walk);
        }
        found.sort_by_key(|stop| stops.iter().position(|s| s.id == stop.id));
        Ok(found)
    }
}

/// Rows on the TV's departure board.
const BOARD_ROWS: usize = 6;

fn board(stops: &[Stop], now: jiff::Timestamp) -> Option<serde_json::Value> {
    let any = TransitQuery::default();
    let found = departures(stops, &any, now);
    let first = found.first()?;
    let rows: Vec<serde_json::Value> = found
        .iter()
        .take(BOARD_ROWS)
        .map(|d| json!({ "line": d.line, "mode": d.mode, "towards": d.towards, "minutes": d.minutes }))
        .collect();
    Some(json!({ "stop": first.stop, "rows": rows }))
}

fn departures(stops: &[Stop], query: &TransitQuery, now: jiff::Timestamp) -> Vec<Departure> {
    let tz = jiff::tz::TimeZone::system();
    // The nearest stop that has any matching departure; its next ones.
    for stop in stops {
        let mut found: Vec<Departure> = stop
            .calls
            .iter()
            .filter(|c| !c.cancellation && query.mode.matches(c.journey.line.mode.as_deref().unwrap_or("")))
            .filter(|c| match &query.line {
                None => true,
                Some(line) => c.journey.line.public_code.as_deref().is_some_and(|code| code.eq_ignore_ascii_case(line)),
            })
            .filter(|c| match &query.destination {
                None => true,
                Some(d) if centre(d) => true,
                Some(d) => {
                    let sign =
                        c.destination.as_ref().and_then(|x| x.front_text.as_deref()).unwrap_or("").to_lowercase();
                    sign.contains(d.as_str()) || d.split_whitespace().all(|w| sign.contains(w))
                }
            })
            .map(|c| Departure {
                stop: stop.name.clone(),
                line: c.journey.line.public_code.clone().unwrap_or_default(),
                mode: c.journey.line.mode.clone().unwrap_or_default(),
                towards: c.destination.as_ref().and_then(|d| d.front_text.clone()).unwrap_or_default(),
                minutes: ((c.expected.as_second() - now.as_second()) as f64 / 60.0).round() as i64,
                clock: c.expected.to_zoned(tz.clone()).strftime("%H:%M").to_string(),
            })
            .filter(|d| d.minutes >= stop.walk)
            .collect();
        if !found.is_empty() {
            found.sort_by_key(|d| d.minutes);
            return found;
        }
    }
    Vec::new()
}

fn when(d: &Departure, lang: Lang) -> String {
    when_at(d.minutes, &d.clock, lang)
}

fn when_at(minutes: i64, clock: &str, lang: Lang) -> String {
    match (lang, minutes) {
        (Lang::English, 0) => "now".into(),
        (Lang::English, 1) => "in 1 minute".into(),
        (Lang::English, m) if m < 60 => format!("in {m} minutes"),
        (Lang::English, _) => format!("at {clock}"),
        (Lang::Norwegian, 0) => "nå".into(),
        (Lang::Norwegian, 1) => "om 1 minutt".into(),
        (Lang::Norwegian, m) if m < 60 => format!("om {m} minutter"),
        (Lang::Norwegian, _) => format!("klokka {clock}"),
    }
}

/// "The next bus is the 31 to Snarøya, from Wessels plass. It leaves in 4 minutes. The one after
/// leaves in 12 minutes."
fn answer(stops: &[Stop], query: &TransitQuery, now: jiff::Timestamp, lang: Lang) -> String {
    let mut found = departures(stops, query, now);
    // The sign may name the end of the line, not the stop asked for: a line's departures then do.
    if found.is_empty() && query.line.is_some() && query.destination.is_some() {
        found = departures(stops, &TransitQuery { destination: None, ..query.clone() }, now);
    }
    let what = query.mode.word(lang);
    let Some(first) = found.first() else {
        // "bus 31", "line 31" ("linje 31").
        let what = match (&query.line, query.mode, lang) {
            (None, ..) => what.to_owned(),
            (Some(line), Mode::Any, Lang::English) => format!("line {}", line.to_uppercase()),
            (Some(line), Mode::Any, Lang::Norwegian) => format!("linje {}", line.to_uppercase()),
            (Some(line), ..) => format!("{what} {}", line.to_uppercase()),
        };
        let stop = query.from.as_ref().and_then(|_| stops.first()).map(|s| s.name.as_str());
        return match (lang, &query.destination, stop) {
            (Lang::English, Some(d), None) => format!("I found no {what} towards {d} in the next three hours."),
            (Lang::English, Some(d), Some(s)) => {
                format!("I found no {what} from {s} towards {d} in the next three hours.")
            }
            (Lang::English, None, s) => {
                format!("I found no {what} from {} in the next three hours.", s.unwrap_or("the nearby stops"))
            }
            (Lang::Norwegian, Some(d), None) => format!("Jeg fant ingen {what} mot {d} de neste tre timene."),
            (Lang::Norwegian, Some(d), Some(s)) => {
                format!("Jeg fant ingen {what} fra {s} mot {d} de neste tre timene.")
            }
            (Lang::Norwegian, None, s) => {
                format!("Jeg fant ingen {what} fra {} de neste tre timene.", s.unwrap_or("holdeplassene i nærheten"))
            }
        };
    };
    // "Any" says what kind it is: "bus 31".
    let line = |d: &Departure| match query.mode {
        Mode::Any => {
            let kind = MODES.iter().find(|(_, m)| m.matches(&d.mode)).map_or(Mode::Any, |&(_, m)| m);
            format!("{} {}", kind.word(lang), d.line)
        }
        _ if lang == Lang::English => format!("the {}", d.line),
        _ => d.line.clone(),
    };
    match lang {
        // The line in a sentence of its own, then when: the times are few, so they are rendered
        // ahead (`english_sentences`), and the line sentence is kept once said.
        Lang::English => {
            let then = found.get(1).map(|second| format!(" The one after leaves {}.", when(second, lang)));
            format!(
                "The next {what} is {} to {}, from {}. It leaves {}.{}",
                line(first),
                first.towards,
                first.stop,
                when(first, lang),
                then.unwrap_or_default()
            )
        }
        Lang::Norwegian => {
            let then = found.get(1).map(|second| format!(", og den neste {}", when(second, lang)));
            format!(
                "Neste {what}, {} mot {}, går fra {} {}{}.",
                line(first),
                first.towards,
                first.stop,
                when(first, lang),
                then.unwrap_or_default()
            )
        }
    }
}

/// Every English sentence saying when a departure leaves within the hour, to render ahead.
pub fn english_sentences() -> Vec<String> {
    (0..60)
        .flat_map(|m| {
            let when = when_at(m, "", Lang::English);
            [format!("It leaves {when}."), format!("The one after leaves {when}.")]
        })
        .collect()
}

/// "Tram 11 to Majorstuen leaves Wessels plass in 6 minutes; head out in 3 minutes. The one after
/// leaves in 14 minutes."
fn trip_answer(patterns: &[Pattern], destination: &str, mode: Mode, now: jiff::Timestamp, lang: Lang) -> String {
    let tz = jiff::tz::TimeZone::system();
    let minutes = |t: jiff::Timestamp| ((t.as_second() - now.as_second()) as f64 / 60.0).floor() as i64;
    let clock = |t: jiff::Timestamp| t.to_zoned(tz.clone()).strftime("%H:%M").to_string();
    let boarding: Vec<(&Pattern, &TripLeg)> =
        patterns.iter().filter_map(|p| Some((p, p.legs.iter().find(|l| l.mode != "foot")?))).collect();
    let Some(&(pattern, leg)) = boarding.first() else {
        let what = mode.word(lang);
        return match lang {
            Lang::English => format!("I found no {what} to {destination} in the next few hours."),
            Lang::Norwegian => format!("Jeg fant ingen {what} til {destination} de neste timene."),
        };
    };
    let kind = MODES.iter().find(|(_, m)| m.matches(&leg.mode)).map_or(Mode::Any, |&(_, m)| m).word(lang);
    let kind = capitalize(kind);
    let line = leg.line.as_ref().and_then(|l| l.public_code.clone()).unwrap_or_default();
    let changes = pattern.legs.iter().filter(|l| l.mode != "foot").count().saturating_sub(1);
    let leaves = when_at(minutes(leg.start), &clock(leg.start), lang);
    let set_off = when_at(minutes(pattern.start).max(0), &clock(pattern.start), lang);
    let next = boarding.get(1).map(|(_, l)| when_at(minutes(l.start), &clock(l.start), lang));
    match lang {
        Lang::English => {
            let change = match changes {
                0 => String::new(),
                1 => ", with one change".into(),
                n => format!(", with {n} changes"),
            };
            let next = next.map(|n| format!(" The one after leaves {n}.")).unwrap_or_default();
            format!(
                "{kind} {line} to {destination} leaves {} {leaves}{change}; head out {set_off}.{next}",
                leg.from.name
            )
        }
        Lang::Norwegian => {
            let change = match changes {
                0 => String::new(),
                1 => ", med ett bytte".into(),
                n => format!(", med {n} bytter"),
            };
            let next = next.map(|n| format!(" Den neste går {n}.")).unwrap_or_default();
            format!(
                "{kind} {line} til {destination} går fra {} {leaves}{change}; gå hjemmefra {set_off}.{next}",
                leg.from.name
            )
        }
    }
}

fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map(|c| c.to_uppercase().chain(chars).collect()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transit_questions() {
        let q = |mode, dest: Option<&str>| {
            Some(TransitQuery { mode, destination: dest.map(str::to_owned), ..Default::default() })
        };
        assert_eq!(parse("When's the next bus?"), q(Mode::Bus, None));
        assert_eq!(parse("when does the next tram to Majorstuen leave"), q(Mode::Tram, Some("majorstuen")));
        assert_eq!(parse("next train to Lillestrøm please"), q(Mode::Rail, Some("lillestrøm")));
        assert_eq!(parse("Når går neste buss?"), q(Mode::Bus, None));
        assert_eq!(parse("når går t-banen til sentrum"), q(Mode::Metro, Some("sentrum")));
        assert_eq!(parse("neste trikk mot Jar"), q(Mode::Tram, Some("jar")));
        assert_eq!(parse("når går neste trykk."), q(Mode::Tram, None));
        assert_eq!(parse("what are the next departures"), q(Mode::Any, None));
        assert_eq!(parse("I took the bus to work"), None);
        assert_eq!(parse("what's the weather"), None);
    }

    #[test]
    fn lines_and_stops() {
        let q = |mode, line: Option<&str>, from: Option<&str>, dest: Option<&str>| {
            Some(TransitQuery {
                mode,
                line: line.map(str::to_owned),
                from: from.map(str::to_owned),
                destination: dest.map(str::to_owned),
            })
        };
        assert_eq!(parse("when is the next 31 bus"), q(Mode::Bus, Some("31"), None, None));
        assert_eq!(parse("when does bus 31 leave"), q(Mode::Bus, Some("31"), None, None));
        assert_eq!(parse("når går neste 37?"), q(Mode::Any, Some("37"), None, None));
        assert_eq!(parse("når går neste 37-buss"), q(Mode::Bus, Some("37"), None, None));
        assert_eq!(parse("next N12 to Bekkestua"), q(Mode::Any, Some("n12"), None, Some("bekkestua")));
        assert_eq!(parse("next train from Nationaltheatret"), q(Mode::Rail, None, Some("nationaltheatret"), None));
        assert_eq!(
            parse("when's the next train from Nationaltheatret to Lillestrøm"),
            q(Mode::Rail, None, Some("nationaltheatret"), Some("lillestrøm"))
        );
        assert_eq!(
            parse("next train to Lillestrøm from Oslo S"),
            q(Mode::Rail, None, Some("oslo s"), Some("lillestrøm"))
        );
        assert_eq!(parse("neste buss fra Jernbanetorget"), q(Mode::Bus, None, Some("jernbanetorget"), None));
        assert_eq!(parse("next bus from home"), q(Mode::Bus, None, None, None));
        // Numbers that are counts, times or songs.
        assert_eq!(parse("what's the weather the next 3 days"), None);
        assert_eq!(parse("skip the next 2"), None);
        assert_eq!(parse("when is the next 5 minutes"), None);
        assert_eq!(parse("I took the 31 to work"), None);
    }

    #[test]
    fn a_line_or_a_stop_is_answered() {
        let now: jiff::Timestamp = "2026-09-24T18:00:00Z".parse().unwrap();
        let stops = [stop(
            "Wessels plass",
            &[("31", "bus", "Snarøya", 4), ("20", "bus", "Skøyen", 6), ("31", "bus", "Tonsenhagen", 9)],
        )];
        let line = TransitQuery { line: Some("31".into()), ..Default::default() };
        assert_eq!(
            answer(&stops, &line, now, Lang::English),
            "The next departure is bus 31 to Snarøya, from Wessels plass. It leaves in 4 minutes. The one after \
             leaves in 9 minutes."
        );
        // The sign names the end of the line: the line's departures still answer.
        let towards = TransitQuery { line: Some("20".into()), destination: Some("majorstuen".into()), ..line.clone() };
        assert!(answer(&stops, &towards, now, Lang::English).contains("bus 20 to Skøyen"));
        let missing = TransitQuery { line: Some("37".into()), ..Default::default() };
        assert_eq!(
            answer(&stops, &missing, now, Lang::English),
            "I found no line 37 from the nearby stops in the next three hours."
        );
        let bus = TransitQuery { mode: Mode::Bus, line: Some("37".into()), ..Default::default() };
        assert_eq!(
            answer(&stops, &bus, now, Lang::Norwegian),
            "Jeg fant ingen buss 37 fra holdeplassene i nærheten de neste tre timene."
        );
        let named = TransitQuery { mode: Mode::Rail, from: Some("wessels plass".into()), ..Default::default() };
        assert_eq!(
            answer(&stops, &named, now, Lang::English),
            "I found no train from Wessels plass in the next three hours."
        );
    }

    fn stop(name: &str, calls: &[(&str, &str, &str, i64)]) -> Stop {
        let now: jiff::Timestamp = "2026-09-24T18:00:00Z".parse().unwrap();
        Stop {
            id: name.into(),
            name: name.into(),
            walk: 0,
            calls: calls
                .iter()
                .map(|&(line, mode, sign, minutes)| Call {
                    expected: now + jiff::SignedDuration::from_mins(minutes),
                    cancellation: false,
                    destination: Some(Display { front_text: Some(sign.into()) }),
                    journey: Journey { line: Line { public_code: Some(line.into()), mode: Some(mode.into()) } },
                })
                .collect(),
        }
    }

    #[test]
    fn board_lists_the_nearest_stop() {
        let now: jiff::Timestamp = "2026-09-24T18:00:00Z".parse().unwrap();
        let stops = [stop("Wessels plass", &[("31", "bus", "Tonsenhagen", 7), ("31", "bus", "Snarøya", 4)])];
        let value = board(&stops, now).unwrap();
        assert_eq!(value["stop"], "Wessels plass");
        assert_eq!(value["rows"][0], json!({ "line": "31", "mode": "bus", "towards": "Snarøya", "minutes": 4 }));
        assert!(board(&[], now).is_none());
    }

    #[test]
    fn answers_from_the_nearest_stop_that_has_one() {
        let now: jiff::Timestamp = "2026-09-24T18:00:00Z".parse().unwrap();
        let stops = [
            stop(
                "Wessels plass",
                &[("31", "bus", "Snarøya", 4), ("31", "bus", "Tonsenhagen", 7), ("31", "bus", "Snarøya", 12)],
            ),
            stop("Stortinget", &[("5", "metro", "Sognsvann", 2), ("11", "tram", "Kjelsås", 3)]),
        ];
        let bus = TransitQuery { mode: Mode::Bus, destination: None, ..Default::default() };
        assert_eq!(
            answer(&stops, &bus, now, Lang::English),
            "The next bus is the 31 to Snarøya, from Wessels plass. It leaves in 4 minutes. The one after leaves in 7 \
             minutes."
        );
        let to = TransitQuery { mode: Mode::Bus, destination: Some("snarøya".into()), ..Default::default() };
        assert_eq!(
            answer(&stops, &to, now, Lang::Norwegian),
            "Neste buss, 31 mot Snarøya, går fra Wessels plass om 4 minutter, og den neste om 12 minutter."
        );
        let metro = TransitQuery { mode: Mode::Metro, destination: None, ..Default::default() };
        assert_eq!(
            answer(&stops, &metro, now, Lang::English),
            "The next metro is the 5 to Sognsvann, from Stortinget. It leaves in 2 minutes."
        );
        for sentence in answer(&stops, &bus, now, Lang::English).split_inclusive('.').map(str::trim) {
            assert!(crate::speech_cache::CachedTts::cacheable(sentence), "{sentence}");
        }
        let ahead = english_sentences();
        assert!(ahead.contains(&"It leaves in 4 minutes.".to_string()));
        assert!(ahead.contains(&"The one after leaves in 7 minutes.".to_string()));
        let any = TransitQuery { mode: Mode::Any, destination: None, ..Default::default() };
        assert!(answer(&stops, &any, now, Lang::English)
            .starts_with("The next departure is bus 31 to Snarøya, from Wessels plass."));
        // Four minutes' walk away: the bus in 4 minutes is still catchable, the metro in 2 is not.
        let mut far = stops;
        far[1].walk = 4;
        let metro_far = answer(&far, &metro, now, Lang::English);
        assert_eq!(metro_far, "I found no metro from the nearby stops in the next three hours.");
        let stops = far;
        let ferry = TransitQuery { mode: Mode::Ferry, destination: None, ..Default::default() };
        assert_eq!(
            answer(&stops, &ferry, now, Lang::English),
            "I found no boat from the nearby stops in the next three hours."
        );
    }

    #[test]
    fn trips_say_when_to_leave() {
        let now: jiff::Timestamp = "2026-09-24T18:00:00Z".parse().unwrap();
        let at = |m: i64| (now + jiff::SignedDuration::from_mins(m)).to_string();
        let json = format!(
            r#"[{{"expectedStartTime":"{}","legs":[{{"mode":"foot","expectedStartTime":"{}","fromPlace":{{"name":"home"}},"line":null}},
                {{"mode":"tram","expectedStartTime":"{}","fromPlace":{{"name":"Wessels plass"}},"line":{{"publicCode":"11","transportMode":"tram"}}}}]}},
               {{"expectedStartTime":"{}","legs":[{{"mode":"tram","expectedStartTime":"{}","fromPlace":{{"name":"Wessels plass"}},"line":{{"publicCode":"19","transportMode":"tram"}}}},
                {{"mode":"metro","expectedStartTime":"{}","fromPlace":{{"name":"Stortinget"}},"line":{{"publicCode":"5","transportMode":"metro"}}}}]}}]"#,
            at(3),
            at(3),
            at(6),
            at(11),
            at(14),
            at(20)
        );
        let patterns: Vec<Pattern> = serde_json::from_str(&json).unwrap();
        assert_eq!(
            trip_answer(&patterns, "Majorstuen", Mode::Tram, now, Lang::English),
            "Tram 11 to Majorstuen leaves Wessels plass in 6 minutes; head out in 3 minutes. The one after leaves in 14 minutes."
        );
        assert_eq!(
            trip_answer(&patterns[1..], "Majorstuen", Mode::Any, now, Lang::Norwegian),
            "Trikk 19 til Majorstuen går fra Wessels plass om 14 minutter, med ett bytte; gå hjemmefra om 11 minutter."
        );
        assert_eq!(
            trip_answer(&[], "Majorstuen", Mode::Bus, now, Lang::English),
            "I found no bus to Majorstuen in the next few hours."
        );
    }
}
