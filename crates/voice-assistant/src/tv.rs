//! The TV through its Home Assistant media player: on and off, volume, mute, pause, opening an app
//! ("put Netflix on the TV"), a title from NRK or Plex ("put on Exit on NRK", "play Forrest Gump
//! on Plex"), and the assistant's own screen on it ("show the weather on the TV").
//! The TV is the `tv` setting, else the media player that is a TV by its device class or name and
//! not a Music Assistant player; one with the TV device class (LG webOS) before one that only casts.

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde_json::json;
use tracing::{info, warn};

use crate::ha::{volume_level, HomeAssistant};
use crate::lang::Lang;
use crate::names::Names;

/// Fixed English answers, rendered ahead of time.
pub const ENGLISH_ANSWERS: &[&str] = &[
    "The TV is off.",
    "The TV is on.",
    "The TV is already on.",
    "Okay, the TV is on.",
    "The TV is already off.",
    "Okay, the TV is off.",
    "Okay, the TV is muted.",
    "Okay, the TV sound is back on.",
    "Okay, paused.",
    "Okay.",
    "Okay, I'm on the TV.",
    "Okay, the weather is on the TV.",
    "Okay, the timers are on the TV.",
    "Okay, the music is on the TV.",
    "Okay, the departures are on the TV.",
    "There are no timers running.",
    "Nothing is playing.",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TvCommand {
    Power(bool),
    /// "Is the TV on?", "did I leave the TV on?"
    Status,
    Louder,
    Quieter,
    /// Volume in percent.
    Volume(u8),
    Mute(bool),
    Pause,
    Resume,
    /// An app or input by name, as said.
    App(String),
    /// A series or programme from NRK, as said: "put on Exit on NRK".
    Nrk(String),
    /// A film or show from the Plex server, as said: "play Forrest Gump on Plex".
    Plex(String),
    /// The assistant's screen on the TV, with this on it.
    Show(Screen),
    /// Back to the assistant's face: "close that".
    Hide,
}

/// What the assistant's screen on the TV shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    /// The face, clock and weather.
    Jarvis,
    Weather,
    Timers,
    Music,
    Departures,
}

/// Words for each screen, longest phrase first where one contains another.
const SCREENS: &[(&str, Screen)] = &[
    ("what's playing", Screen::Music),
    ("weather", Screen::Weather),
    ("forecast", Screen::Weather),
    ("været", Screen::Weather),
    ("timer", Screen::Timers),
    ("timers", Screen::Timers),
    ("tidtaker", Screen::Timers),
    ("tidtakeren", Screen::Timers),
    ("tidtakerne", Screen::Timers),
    ("music", Screen::Music),
    ("song", Screen::Music),
    ("musikken", Screen::Music),
    ("sangen", Screen::Music),
    ("departures", Screen::Departures),
    ("bus", Screen::Departures),
    ("buses", Screen::Departures),
    ("bus times", Screen::Departures),
    ("tram", Screen::Departures),
    ("avganger", Screen::Departures),
    ("avgangene", Screen::Departures),
    ("bussen", Screen::Departures),
    ("yourself", Screen::Jarvis),
    ("your face", Screen::Jarvis),
    ("dashboard", Screen::Jarvis),
    ("dashbord", Screen::Jarvis),
    ("dashbordet", Screen::Jarvis),
    ("deg", Screen::Jarvis),
    ("deg selv", Screen::Jarvis),
];
const SHOW_WORDS: &[&str] = &["show", "put", "display", "bring", "vis", "sett"];
const HIDE_WORDS: &[&str] = &["hide", "close", "clear", "dismiss", "skjul", "lukk", "fjern"];
/// Where the assistant's screen is, besides the TV words.
const SCREEN_WORDS: &[&str] = &["screen", "skjermen", "dashboard", "dashbord", "dashbordet"];

const TV_WORDS: &[&str] =
    &["tv", "tvs", "tv's", "television", "telly", "tellie", "fjernsyn", "fjernsynet", "tven", "teve", "teven"];
/// Apps people name without "TV": "put on Netflix". Longest first, so "nrk tv" wins over "nrk".
const APPS: &[&str] = &[
    "disney plus",
    "prime video",
    "amazon prime",
    "tv 2 play",
    "apple tv",
    "nrk tv",
    "netflix",
    "youtube",
    "plex",
    "nrk",
    "disney",
    "hbo",
    "max",
    "viaplay",
    "twitch",
];
const OPEN_WORDS: &[&str] =
    &["open", "launch", "start", "put", "switch", "go", "show", "watch", "play", "åpne", "sett", "bytt", "vis"];
/// Words of an app request that are not the app's name.
const APP_FILLER: &[&str] = &[
    "open",
    "launch",
    "start",
    "put",
    "switch",
    "go",
    "show",
    "watch",
    "play",
    "on",
    "to",
    "the",
    "tv",
    "television",
    "telly",
    "app",
    "please",
    "some",
    "åpne",
    "sett",
    "bytt",
    "vis",
    "på",
    "til",
    "tven",
    "appen",
];
const POLITE: &[&str] = &["can", "could", "would", "you", "please", "just", "okay", "ok", "kan", "du"];

/// Words before a title: "can you put on", "I want to watch", "sett på".
const TITLE_START: &[&str] =
    &["i", "want", "to", "i'd", "like", "let's", "lets", "jeg", "vil", "se", "på", "on", "some"];

/// Words before a show's name that only say which episode: Plex plays the next one anyway.
const NEXT_EPISODE: &[&str] = &["the", "next", "new", "latest", "episode", "of", "neste", "episode", "av"];

/// "Put on Exit on NRK", "play Skam on NRK TV", "I want to watch Exit from NRK", "sett på Exit på NRK";
/// `service` is "nrk" or "plex".
fn title_on(words: &[String], service: &str) -> Option<String> {
    let at = words.iter().position(|w| w == service)?;
    if at < 2 || !["on", "from", "in", "på", "fra"].contains(&words[at - 1].as_str()) {
        return None;
    }
    let before = &words[..at - 1];
    let start = before.iter().position(|w| !POLITE.contains(&w.as_str()))?;
    let first = before[start].as_str();
    if !OPEN_WORDS.contains(&first) && !TITLE_START.contains(&first) && !["next", "neste"].contains(&first) {
        return None;
    }
    let title: Vec<&str> = before[start..]
        .iter()
        .map(String::as_str)
        .skip_while(|w| OPEN_WORDS.contains(w) || TITLE_START.contains(w) || POLITE.contains(w))
        .collect();
    // "The next episode of The Office": the show.
    let title = match title.iter().position(|w| *w == "episode") {
        Some(at) if title[..=at].iter().all(|w| NEXT_EPISODE.contains(w)) && title.len() > at + 2 => {
            title[at + 2..].to_vec()
        }
        _ => title,
    };
    (!title.is_empty() && !title.iter().all(|w| APP_FILLER.contains(w))).then(|| title.join(" "))
}

pub fn parse(words: &[String]) -> Option<TvCommand> {
    if let Some(title) = title_on(words, "nrk") {
        return Some(TvCommand::Nrk(title));
    }
    if let Some(title) = title_on(words, "plex") {
        return Some(TvCommand::Plex(title));
    }
    let has = |w: &str| words.iter().any(|x| x == w);
    let any = |list: &[&str]| words.iter().any(|w| list.contains(&w.as_str()));
    let tv = any(TV_WORDS);
    let text = format!(" {} ", words.join(" "));
    if let Some(command) = screen(words, tv || any(SCREEN_WORDS), &text) {
        return Some(command);
    }
    let app = APPS.iter().find(|a| text.contains(&format!(" {a} "))).copied();
    if !tv && app.is_none() {
        return None;
    }
    let first = words.iter().map(String::as_str).find(|w| !POLITE.contains(w)).unwrap_or_default();
    // "Is the TV on?", "did I leave the TV on?" ask whether it is on; "what's on TV tonight" is
    // for someone else.
    // "Can you check if the TV is on", "so is the TV on" ask too, after their lead-ins.
    let question = crate::intent::question_word(words);
    let first = if ["if", "whether", "om", "is", "are", "did", "er"].contains(&question) { "is" } else { first };
    if ["is", "are", "did", "have", "has", "do", "does", "what", "what's", "er", "står", "har", "hva"].contains(&first)
    {
        let power = has("on") || has("off") || has("på") || has("av") || has("left");
        return (tv && power && !["what", "what's", "hva"].contains(&first)).then_some(TvCommand::Status);
    }
    if has("unmute") {
        return Some(TvCommand::Mute(false));
    }
    if has("mute") || (has("demp") && has("lyden")) {
        return Some(TvCommand::Mute(true));
    }
    if OPEN_WORDS.contains(&first) {
        if let Some(app) = app {
            return Some(TvCommand::App(app.into()));
        }
        // "Open the browser on the TV", "switch the TV to HDMI 1".
        let rest: Vec<&str> =
            words.iter().map(String::as_str).filter(|w| !APP_FILLER.contains(w) && !POLITE.contains(w)).collect();
        let switching = rest.iter().all(|w| ["on", "off", "av", "back", "up", "down", "opp", "ned"].contains(w));
        if tv && first != "play" && !rest.is_empty() && !switching {
            return Some(TvCommand::App(rest.join(" ")));
        }
    }
    if !tv {
        return None;
    }
    let volume = has("volume") || has("volumet") || has("lyden");
    let number = words.iter().find_map(|w| w.trim_end_matches('%').parse::<u8>().ok());
    if let Some(n) = number.filter(|_| volume || has("to") || has("til")) {
        return Some(TvCommand::Volume(n.min(100)));
    }
    if has("louder") || has("høyere") || has("up") || has("opp") {
        return Some(TvCommand::Louder);
    }
    if ["quieter", "softer", "lower", "down", "lavere", "ned"].iter().any(|w| has(w)) {
        return Some(TvCommand::Quieter);
    }
    if has("pause") {
        return Some(TvCommand::Pause);
    }
    if has("resume") || has("continue") || has("unpause") || has("fortsett") || first == "play" {
        return Some(TvCommand::Resume);
    }
    // "Off" first: in Norwegian "på" is also the preposition.
    if has("off") || has("av") {
        return Some(TvCommand::Power(false));
    }
    if has("on") || has("på") {
        return Some(TvCommand::Power(true));
    }
    None
}

/// "Show the weather on the TV", "put yourself on the screen", "close that", "vis været på TV-en".
fn screen(words: &[String], on_screen: bool, text: &str) -> Option<TvCommand> {
    let first = words.iter().map(String::as_str).find(|w| !POLITE.contains(w)).unwrap_or_default();
    let rest: Vec<&str> = words.iter().map(String::as_str).filter(|w| !POLITE.contains(w)).collect();
    // "Close that" alone: the screen is the thing most likely open. "Close the blinds" is not it.
    if HIDE_WORDS.contains(&first) && (on_screen || matches!(rest[1..], ["that"] | ["it"] | ["det"])) {
        return Some(TvCommand::Hide);
    }
    if !on_screen || !SHOW_WORDS.contains(&first) {
        return None;
    }
    // The longest phrase that is there: "bus times" over "bus".
    SCREENS
        .iter()
        .filter(|(phrase, _)| text.contains(&format!(" {phrase} ")))
        .max_by_key(|(phrase, _)| phrase.len())
        .map(|&(_, screen)| TvCommand::Show(screen))
}

/// TVs that are on, by entity id: what "everything off" turns off.
pub fn on_tvs(ha: &HomeAssistant) -> Result<Vec<String>> {
    let found: Vec<(String, String)> = serde_json::from_str(&ha.template(TVS_TEMPLATE)?).context("parsing the TVs")?;
    Ok(found.into_iter().filter(|(_, state)| !["off", "standby"].contains(&state.as_str())).map(|(id, _)| id).collect())
}

/// Every TV that is not unavailable, as `[id, state]`: those with the TV device class first. A TV
/// that only casts has the name but can neither start nor open apps.
const TVS_TEMPLATE: &str = r#"{% set tvs = states.media_player | rejectattr('state', 'in', ['unavailable', 'unknown']) | rejectattr('attributes.mass_player_type', 'defined') | list %}{% set classed = tvs | selectattr('attributes.device_class', 'eq', 'tv') | list %}{% set named = tvs | rejectattr('attributes.device_class', 'eq', 'tv') | selectattr('name', 'search', '(?i)tv') | list %}[{% for s in classed + named %}[{{ s.entity_id | tojson }}, {{ s.state | tojson }}]{% if not loop.last %},{% endif %}{% endfor %}]"#;

/// How long a TV that was off may take to start before an app is opened on it.
const START_TIMEOUT: Duration = Duration::from_secs(15);
/// A volume step: the TV's own step is 1 of 100, too small to hear.
const VOLUME_STEP: f64 = 0.05;

pub struct Tv {
    /// The `tv` setting: an entity id.
    entity: Option<String>,
    agent: ureq::Agent,
    plex: Option<Plex>,
}

/// The Plex server innestemme searches: its URL and a token. Home Assistant's Plex integration,
/// for the same server, is what lets its Cast player play `plex://<key>`.
#[derive(Clone, Debug)]
pub struct Plex {
    pub url: String,
    pub token: String,
}

/// A TV's Google Cast media player, which NRK and Plex play through. A TV with Cast built in has one besides
/// its own; a speaker group or speaker may cast too, so one named TV first.
const CAST_TEMPLATE: &str = r#"{{ integration_entities('cast') | select('match', 'media_player\.') | list | tojson }}"#;

/// NRK TV's public catalogue API.
const NRK_API: &str = "https://psapi.nrk.no";

impl Tv {
    pub fn new(entity: Option<String>) -> Self {
        Self { entity, agent: crate::geo::agent("innestemme"), plex: None }
    }

    pub fn with_plex(mut self, plex: Option<Plex>) -> Self {
        self.plex = plex;
        self
    }

    fn find(&self, ha: &HomeAssistant) -> Result<Option<String>> {
        if let Some(entity) = &self.entity {
            return Ok(Some(entity.clone()));
        }
        let found: Vec<(String, String)> =
            serde_json::from_str(&ha.template(TVS_TEMPLATE)?).context("parsing the TVs")?;
        Ok(found.into_iter().next().map(|(id, _)| id))
    }

    pub fn run(&mut self, ha: Option<&HomeAssistant>, command: &TvCommand, lang: Lang) -> String {
        let no = lang == Lang::Norwegian;
        let Some(ha) = ha else {
            return if no { "Home Assistant er ikke koblet til ennå." } else { "Home Assistant isn't connected yet." }
                .into();
        };
        self.control(ha, command, no).unwrap_or_else(|error| {
            warn!(error = format!("{error:#}"), ?command, "tv request failed");
            if no { "Beklager, det gikk ikke å styre TV-en." } else { "Sorry, I couldn't control the TV." }.into()
        })
    }

    fn control(&mut self, ha: &HomeAssistant, command: &TvCommand, no: bool) -> Result<String> {
        let Some(entity) = self.find(ha)? else {
            return Ok(if no {
                "Jeg finner ingen TV i Home Assistant."
            } else {
                "I can't find a TV in Home Assistant."
            }
            .into());
        };
        let state = ha.state(&entity)?;
        let on = !["off", "standby", "unavailable"].contains(&state["state"].as_str().unwrap_or("off"));
        let call = |service: &str, data: serde_json::Value| -> Result<()> {
            let mut data = data;
            data["entity_id"] = entity.clone().into();
            info!(service, entity, "tv");
            ha.service("media_player", service, data)
        };
        let off_answer = if no { "TV-en er av." } else { "The TV is off." };
        Ok(match command {
            TvCommand::Status if on => if no { "TV-en er på." } else { "The TV is on." }.into(),
            TvCommand::Status => off_answer.into(),
            TvCommand::Power(true) if on => if no { "TV-en er allerede på." } else { "The TV is already on." }.into(),
            TvCommand::Power(true) => {
                call("turn_on", json!({}))?;
                if no { "Ok, TV-en er på." } else { "Okay, the TV is on." }.into()
            }
            TvCommand::Power(false) if !on => {
                if no { "TV-en er allerede av." } else { "The TV is already off." }.into()
            }
            TvCommand::Power(false) => {
                call("turn_off", json!({}))?;
                if no { "Ok, TV-en er av." } else { "Okay, the TV is off." }.into()
            }
            TvCommand::App(name) => {
                if !on {
                    call("turn_on", json!({}))?;
                    self.wait_until_on(ha, &entity)?;
                }
                let state = ha.state(&entity)?;
                let sources: Vec<String> = state["attributes"]["source_list"]
                    .as_array()
                    .map(|list| list.iter().filter_map(|s| s.as_str().map(str::to_owned)).collect())
                    .unwrap_or_default();
                let names = Names::new(&sources);
                let Some(found) = names.best(name).map(|m| m.name.to_owned()) else {
                    return Ok(if no {
                        format!("Jeg finner ikke {name} på TV-en.")
                    } else {
                        format!("I can't find {name} on the TV.")
                    });
                };
                call("select_source", json!({ "source": found }))?;
                if no {
                    format!("Ok, åpner {found}.")
                } else {
                    format!("Okay, opening {found}.")
                }
            }
            TvCommand::Nrk(title) => {
                let Some((name, program)) = self.nrk_search(title)? else {
                    return Ok(if no {
                        format!("Jeg finner ikke {title} på NRK.")
                    } else {
                        format!("I can't find {title} on NRK.")
                    });
                };
                info!(title = name, program, "nrk on the tv");
                let media = json!({ "app_name": "nrktv", "media_id": program }).to_string();
                self.cast(ha, &entity, on, "cast", &media)?;
                if no {
                    format!("Ok, setter på {name}.")
                } else {
                    format!("Okay, putting on {name}.")
                }
            }
            TvCommand::Plex(title) => {
                let Some(plex) = &self.plex else {
                    return Ok(if no {
                        "Plex er ikke satt opp. Sett plex-url og plex-token i innstillingene."
                    } else {
                        "Plex isn't set up. Set plex-url and plex-token in the settings."
                    }
                    .into());
                };
                let Some(found) = plex_search(&self.agent, plex, title)? else {
                    return Ok(if no {
                        format!("Jeg finner ikke {title} på Plex.")
                    } else {
                        format!("I can't find {title} on Plex.")
                    });
                };
                info!(title = found.name, key = found.key, kind = found.kind, "plex on the tv");
                self.cast(ha, &entity, on, found.kind, &format!("plex://{}", found.key))?;
                if no {
                    format!("Ok, setter på {}.", found.name)
                } else {
                    format!("Okay, putting on {}.", found.name)
                }
            }
            // The engine shows its own screen; this is only reached without one.
            TvCommand::Show(_) | TvCommand::Hide => if no { "Ok." } else { "Okay." }.into(),
            _ if !on => off_answer.into(),
            TvCommand::Louder | TvCommand::Quieter => {
                let now = state["attributes"]["volume_level"].as_f64().unwrap_or(0.2);
                let step = if *command == TvCommand::Louder { VOLUME_STEP } else { -VOLUME_STEP };
                let percent = ((now + step) * 100.0).round().clamp(0.0, 100.0);
                call("volume_set", json!({ "volume_level": volume_level(percent) }))?;
                if no {
                    format!("TV-volumet er {percent}.")
                } else {
                    format!("TV volume {percent}.")
                }
            }
            TvCommand::Volume(n) => {
                call("volume_set", json!({ "volume_level": volume_level(f64::from(*n)) }))?;
                if no {
                    format!("TV-volumet er {n}.")
                } else {
                    format!("TV volume {n}.")
                }
            }
            TvCommand::Mute(mute) => {
                call("volume_mute", json!({ "is_volume_muted": mute }))?;
                match (mute, no) {
                    (true, false) => "Okay, the TV is muted.",
                    (true, true) => "Ok, lyden på TV-en er av.",
                    (false, false) => "Okay, the TV sound is back on.",
                    (false, true) => "Ok, lyden på TV-en er på igjen.",
                }
                .into()
            }
            TvCommand::Pause => {
                call("media_pause", json!({}))?;
                if no { "Ok, pause." } else { "Okay, paused." }.into()
            }
            TvCommand::Resume => {
                call("media_play", json!({}))?;
                if no { "Ok." } else { "Okay." }.into()
            }
        })
    }

    /// The best NRK hit for `title` that can be played: its name and the programme id to play, the
    /// episode NRK itself would start for a series.
    fn nrk_search(&self, title: &str) -> Result<Option<(String, String)>> {
        let url = format!("{NRK_API}/search?q={}", crate::lookup::encode(title));
        let found: serde_json::Value =
            self.agent.get(&url).call().with_context(|| format!("fetching {url}"))?.body_mut().read_json()?;
        let hit = found["hits"].as_array().into_iter().flatten().find(|h| {
            ["serie", "program"].contains(&h["type"].as_str().unwrap_or_default())
                && h["hit"]["hasRights"] != false
                && h["hit"]["sourceMedium"].as_str().is_none_or(|m| m == "tv")
        });
        let Some(hit) = hit else { return Ok(None) };
        let name = hit["hit"]["title"].as_str().unwrap_or(title).to_owned();
        let path = hit["hit"]["url"].as_str().unwrap_or_default();
        let program = match path.split_once('/') {
            Some(("program", id)) => id.to_owned(),
            Some(("serie", series)) => {
                let url = format!("{NRK_API}/tv/catalog/series/{series}");
                let series: serde_json::Value =
                    self.agent.get(&url).call().with_context(|| format!("fetching {url}"))?.body_mut().read_json()?;
                let links = &series["_links"];
                let episode = links["defaultHighlightedEpisode"]["name"]
                    .as_str()
                    .or(links["highlightedEpisode"]["name"].as_str());
                episode.context("an NRK series without an episode to play")?.to_owned()
            }
            _ => return Ok(None),
        };
        Ok(Some((name, program)))
    }

    /// Plays `media` on the TV's Cast player, starting the TV first when it is off.
    fn cast(&self, ha: &HomeAssistant, entity: &str, on: bool, kind: &str, media: &str) -> Result<()> {
        let cast = self.cast_entity(ha)?.context("no Google Cast media player for the TV")?;
        if !on {
            ha.service("media_player", "turn_on", json!({ "entity_id": entity }))?;
            self.wait_until_on(ha, entity)?;
        }
        ha.service(
            "media_player",
            "play_media",
            json!({ "entity_id": cast, "media_content_type": kind, "media_content_id": media }),
        )
    }

    /// The TV's Google Cast media player.
    fn cast_entity(&self, ha: &HomeAssistant) -> Result<Option<String>> {
        let found: Vec<String> =
            serde_json::from_str(&ha.template(CAST_TEMPLATE)?).context("parsing the cast players")?;
        let named_tv = found.iter().find(|id| id.contains("tv")).cloned();
        Ok(named_tv.or_else(|| found.into_iter().next()))
    }

    /// Opens `url` in the TV's web browser, starting the TV first if it is off. Only TVs with
    /// Home Assistant's LG webOS integration can.
    pub fn open_page(&mut self, ha: &HomeAssistant, url: &str) -> Result<()> {
        let entity = self.find(ha)?.context("no TV in Home Assistant")?;
        let state = ha.state(&entity)?;
        if ["off", "standby"].contains(&state["state"].as_str().unwrap_or("off")) {
            ha.service("media_player", "turn_on", json!({ "entity_id": entity }))?;
            self.wait_until_on(ha, &entity)?;
        }
        open_url(ha, &entity, url)
    }

    /// The TV and what it shows (`source`), when it is on. A lockdown takes over only a TV that is
    /// already on, and puts that back when it ends.
    pub fn on_source(&self, ha: &HomeAssistant) -> Result<Option<(String, Option<String>)>> {
        let Some(entity) = self.find(ha)? else { return Ok(None) };
        let state = ha.state(&entity)?;
        if ["off", "standby", "unavailable"].contains(&state["state"].as_str().unwrap_or("off")) {
            return Ok(None);
        }
        let source = state["attributes"]["source"].as_str().map(str::to_owned);
        Ok(Some((entity, source)))
    }

    fn wait_until_on(&self, ha: &HomeAssistant, entity: &str) -> Result<()> {
        let start = Instant::now();
        while start.elapsed() < START_TIMEOUT {
            std::thread::sleep(Duration::from_millis(500));
            let state = ha.state(entity)?;
            if state["state"] == "on" && state["attributes"]["source_list"].is_array() {
                info!(after = ?start.elapsed(), "tv started");
                return Ok(());
            }
        }
        anyhow::bail!("the TV did not start within {START_TIMEOUT:?}")
    }
}

/// What Plex will play: the name to say, the item's key, and the media type for Home Assistant.
struct PlexItem {
    name: String,
    key: String,
    kind: &'static str,
}

/// The best film or show for `title`. A show plays the episode Plex has on deck for it, else the
/// first one not watched, else its first.
fn plex_search(agent: &ureq::Agent, plex: &Plex, title: &str) -> Result<Option<PlexItem>> {
    let get = |path: &str| -> Result<serde_json::Value> {
        let url = format!("{}{path}", plex.url.trim_end_matches('/'));
        let mut response = agent
            .get(&url)
            .header("X-Plex-Token", &plex.token)
            .header("Accept", "application/json")
            .call()
            .with_context(|| format!("fetching {}", url.split('?').next().unwrap_or_default()))?;
        Ok(response.body_mut().read_json()?)
    };
    let found = get(&format!("/hubs/search?limit=5&query={}", crate::lookup::encode(title)))?;
    let hit = found["MediaContainer"]["Hub"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|hub| hub["Metadata"].as_array().into_iter().flatten())
        .find(|item| ["movie", "show"].contains(&item["type"].as_str().unwrap_or_default()));
    let Some(hit) = hit else { return Ok(None) };
    // "The Office (US)" is said as "The Office".
    let name = hit["title"].as_str().unwrap_or(title);
    let name = name.split(" (").next().unwrap_or(name).to_owned();
    let key = hit["ratingKey"].as_str().context("a Plex item without a key")?.to_owned();
    if hit["type"] == "movie" {
        return Ok(Some(PlexItem { name, key, kind: "movie" }));
    }
    let show = get(&format!("/library/metadata/{key}?includeOnDeck=1"))?;
    let on_deck = show["MediaContainer"]["Metadata"][0]["OnDeck"]["Metadata"][0]["ratingKey"].as_str();
    let episode = match on_deck {
        Some(episode) => episode.to_owned(),
        None => {
            let leaves = get(&format!("/library/metadata/{key}/allLeaves"))?;
            let leaves = leaves["MediaContainer"]["Metadata"].as_array().cloned().unwrap_or_default();
            let next = leaves.iter().find(|e| e["viewCount"].as_u64().unwrap_or(0) == 0).or(leaves.first());
            next.and_then(|e| e["ratingKey"].as_str()).context("a Plex show without episodes")?.to_owned()
        }
    };
    Ok(Some(PlexItem { name, key: episode, kind: "episode" }))
}

/// Opens `url` in the browser of a TV that is on.
pub fn open_url(ha: &HomeAssistant, entity: &str, url: &str) -> Result<()> {
    info!(entity, url, "opening the screen on the tv");
    ha.service(
        "webostv",
        "command",
        json!({ "entity_id": entity, "command": "system.launcher/open", "payload": { "target": url } }),
    )
}

/// Switches the TV to `source` ("LG Gallery+", "HDMI 1").
pub fn select_source(ha: &HomeAssistant, entity: &str, source: &str) -> Result<()> {
    info!(entity, source, "tv source");
    ha.service("media_player", "select_source", json!({ "entity_id": entity, "source": source }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dialog::normalize;

    fn tv(text: &str) -> Option<TvCommand> {
        let words: Vec<String> = text.split_whitespace().map(normalize).filter(|w| !w.is_empty()).collect();
        parse(&words)
    }

    #[test]
    fn tv_requests() {
        use TvCommand::*;
        assert_eq!(tv("turn on the TV"), Some(Power(true)));
        assert_eq!(tv("Turn off the TV."), Some(Power(false)));
        assert_eq!(tv("skru av tv-en"), Some(Power(false)));
        assert_eq!(tv("skru på TV-en"), Some(Power(true)));
        assert_eq!(tv("turn the TV up"), Some(Louder));
        assert_eq!(tv("turn the telly down"), Some(Quieter));
        assert_eq!(tv("set the TV volume to 20"), Some(Volume(20)));
        assert_eq!(tv("mute the TV"), Some(Mute(true)));
        assert_eq!(tv("unmute the TV"), Some(Mute(false)));
        assert_eq!(tv("pause the TV"), Some(Pause));
        assert_eq!(tv("put Netflix on the TV"), Some(App("netflix".into())));
        assert_eq!(tv("open YouTube"), Some(App("youtube".into())));
        assert_eq!(tv("start NRK TV"), Some(App("nrk tv".into())));
        assert_eq!(tv("switch the TV to HDMI 1"), Some(App("hdmi 1".into())));
        assert_eq!(tv("turn the TV back on"), Some(Power(true)));
        assert_eq!(tv("Put on Exit on NRK."), Some(Nrk("exit".into())));
        assert_eq!(tv("Can you play Skam on NRK TV?"), Some(Nrk("skam".into())));
        assert_eq!(tv("I want to watch Exit from NRK"), Some(Nrk("exit".into())));
        assert_eq!(tv("put Kompani Lauritzen on NRK on the TV"), Some(Nrk("kompani lauritzen".into())));
        assert_eq!(tv("watch the Weekend News on NRK"), Some(Nrk("the weekend news".into())));
        assert_eq!(tv("sett på Exit på NRK"), Some(Nrk("exit".into())));
        assert_eq!(tv("Play Forrest Gump on Plex."), Some(Plex("forrest gump".into())));
        assert_eq!(tv("put on The Office on Plex"), Some(Plex("the office".into())));
        assert_eq!(tv("put on the next episode of The Office on Plex"), Some(Plex("the office".into())));
        assert_eq!(tv("sett på neste episode av Skam på Plex"), Some(Plex("skam".into())));
        assert_eq!(tv("next episode of The Office on Plex"), Some(Plex("the office".into())));
        assert_eq!(tv("open Plex"), Some(App("plex".into())));
        assert_eq!(tv("put NRK on the TV"), Some(App("nrk".into())));
        assert_eq!(tv("put on NRK"), Some(App("nrk".into())));
        assert_eq!(tv("switch the TV to NRK"), Some(App("nrk".into())));
        assert_eq!(tv("what's on NRK tonight"), None);
        // Not the TV.
        assert_eq!(tv("is the TV on"), Some(Status));
        assert_eq!(tv("did I leave the TV on"), Some(Status));
        assert_eq!(tv("what's on TV tonight"), None);
        assert_eq!(tv("play Careless Whisper"), None);
        assert_eq!(tv("turn off the kitchen lights"), None);
    }

    #[test]
    fn screen_requests() {
        use Screen::*;
        use TvCommand::*;
        assert_eq!(tv("show the weather on the TV"), Some(Show(Weather)));
        assert_eq!(tv("can you put the timers on the telly"), Some(Show(Timers)));
        assert_eq!(tv("show what's playing on the TV"), Some(Show(Music)));
        assert_eq!(tv("put the bus times on the screen"), Some(Show(Departures)));
        assert_eq!(tv("show yourself on the TV"), Some(Show(Jarvis)));
        assert_eq!(tv("vis været på TV-en"), Some(Show(Weather)));
        assert_eq!(tv("close that"), Some(Hide));
        assert_eq!(tv("hide the screen"), Some(Hide));
        assert_eq!(tv("lukk det"), Some(Hide));
        // Still apps and power.
        assert_eq!(tv("show Netflix on the TV"), Some(App("netflix".into())));
        assert_eq!(tv("close the blinds"), None);
        assert_eq!(tv("put the TV on"), Some(Power(true)));
    }
}
