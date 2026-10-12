//! The TV through its Home Assistant media player: on and off, volume, mute, pause, opening an app
//! ("put Netflix on the TV"), and the assistant's own screen on it ("show the weather on the TV").
//! The TV is the `tv` setting, else the media player that is a TV by its device class or name and
//! not a Music Assistant player; one with the TV device class (LG webOS) before one that only casts.

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde_json::json;
use tracing::{info, warn};

use crate::ha::HomeAssistant;
use crate::lang::Lang;
use crate::names::Names;

/// Fixed English answers, rendered ahead of time.
pub const ENGLISH_ANSWERS: &[&str] = &[
    "The TV is off.",
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
    Louder,
    Quieter,
    /// Volume in percent.
    Volume(u8),
    Mute(bool),
    Pause,
    Resume,
    /// An app or input by name, as said.
    App(String),
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
    ("deg", Screen::Jarvis),
    ("deg selv", Screen::Jarvis),
];
const SHOW_WORDS: &[&str] = &["show", "put", "display", "bring", "vis", "sett"];
const HIDE_WORDS: &[&str] = &["hide", "close", "clear", "dismiss", "skjul", "lukk", "fjern"];
/// Where the assistant's screen is, besides the TV words.
const SCREEN_WORDS: &[&str] = &["screen", "skjermen"];

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

pub fn parse(words: &[String]) -> Option<TvCommand> {
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
    // "Is the TV on?", "what's on TV tonight" are questions for someone else.
    if ["is", "are", "what", "what's", "er", "hva"].contains(&first) {
        return None;
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
}

impl Tv {
    pub fn new(entity: Option<String>) -> Self {
        Self { entity }
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
            // The engine shows its own screen; this is only reached without one.
            TvCommand::Show(_) | TvCommand::Hide => if no { "Ok." } else { "Okay." }.into(),
            _ if !on => off_answer.into(),
            TvCommand::Louder | TvCommand::Quieter => {
                let now = state["attributes"]["volume_level"].as_f64().unwrap_or(0.2);
                let step = if *command == TvCommand::Louder { VOLUME_STEP } else { -VOLUME_STEP };
                let level = (now + step).clamp(0.0, 1.0);
                call("volume_set", json!({ "volume_level": level }))?;
                let percent = (level * 100.0).round();
                if no {
                    format!("TV-volumet er {percent}.")
                } else {
                    format!("TV volume {percent}.")
                }
            }
            TvCommand::Volume(n) => {
                call("volume_set", json!({ "volume_level": f64::from(*n) / 100.0 }))?;
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

    /// Opens `url` in the TV's web browser, starting the TV first if it is off. Only TVs with
    /// Home Assistant's LG webOS integration can.
    pub fn open_page(&mut self, ha: &HomeAssistant, url: &str) -> Result<()> {
        let entity = self.find(ha)?.context("no TV in Home Assistant")?;
        let state = ha.state(&entity)?;
        if ["off", "standby"].contains(&state["state"].as_str().unwrap_or("off")) {
            ha.service("media_player", "turn_on", json!({ "entity_id": entity }))?;
            self.wait_until_on(ha, &entity)?;
        }
        info!(entity, url, "opening the screen on the tv");
        ha.service(
            "webostv",
            "command",
            json!({ "entity_id": entity, "command": "system.launcher/open", "payload": { "target": url } }),
        )
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
        // Not the TV.
        assert_eq!(tv("is the TV on"), None);
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
