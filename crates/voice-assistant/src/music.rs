//! Music through Music Assistant (the Home Assistant add-on), which holds the Spotify sign-in,
//! searches the catalogue and plays on the Sonos. Everything is a Home Assistant service call:
//! `music_assistant.search` / `get_library` to find what was asked for, `music_assistant.play_media`
//! to start it, and the usual `media_player.*` services on the Music Assistant player for the rest.

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use tracing::{info, warn};

use crate::dialog::normalize;
use crate::ha::{volume_level, HomeAssistant};
use crate::intent::{MusicCommand, Repeat};
use crate::lang::Lang;
use crate::names::Names;

/// Volume change in percent per "louder" / "quieter".
const VOLUME_STEP: f64 = 10.0;

/// Music Assistant's media players with their names, rendered by Home Assistant.
const PLAYERS_TEMPLATE: &str = r#"[{% for id in integration_entities("music_assistant") | select("match", "media_player[.]") %}{"id": {{ id | tojson }}, "name": {{ (state_attr(id, "friendly_name") or id) | tojson }}}{% if not loop.last %},{% endif %}{% endfor %}]"#;

#[derive(Debug, Clone, PartialEq)]
struct Found {
    uri: String,
    name: String,
    artist: String,
}

/// Fixed English answers, rendered ahead of time.
pub const ENGLISH_ANSWERS: &[&str] = &[
    "Okay, paused.",
    "Okay.",
    "Okay, next song.",
    "Okay, previous song.",
    "Okay, stopped.",
    "Okay, muted.",
    "Okay, unmuted.",
    "Okay, shuffle is on.",
    "Okay, shuffle is off.",
    "Okay, repeating this song.",
    "Okay, repeat is on.",
    "Okay, repeat is off.",
    "Nothing is playing.",
];

pub struct Player {
    /// The speaker: a Music Assistant media player entity id, or its name ("Living Room"). `None`:
    /// the only player there is.
    speaker: Option<String>,
    /// Resolved on first use: (Music Assistant config entry id, player entity id).
    resolved: Option<(String, String)>,
    /// Library artists, for spoken names the transcriber misspells; refreshed now and then.
    artists: Option<(std::time::Instant, Vec<Found>)>,
    /// Liked Songs, found once: it is always the same playlist.
    liked: Option<Found>,
    /// Find what to play but do not start it (testing against a real library).
    dry_run: bool,
}

/// The library changes rarely.
const LIBRARY_REFRESH: std::time::Duration = std::time::Duration::from_secs(600);

impl Player {
    pub fn new(speaker: Option<String>) -> Self {
        Self { speaker, resolved: None, artists: None, liked: None, dry_run: false }
    }

    pub fn dry_run(mut self) -> Self {
        self.dry_run = true;
        self
    }

    /// Finds Music Assistant and the speaker's player; the player entity id.
    pub fn check(&mut self, ha: &HomeAssistant) -> Result<String> {
        Ok(self.resolve(ha)?.1)
    }

    fn resolve(&mut self, ha: &HomeAssistant) -> Result<(String, String)> {
        if let Some(resolved) = &self.resolved {
            return Ok(resolved.clone());
        }
        let entries = ha.get("/api/config/config_entries/entry?domain=music_assistant")?;
        let entry = entries
            .as_array()
            .and_then(|e| e.iter().find(|e| e["state"] == "loaded"))
            .and_then(|e| e["entry_id"].as_str())
            .ok_or_else(|| anyhow!("the Music Assistant integration is not set up in Home Assistant"))?
            .to_owned();
        let entity = match &self.speaker {
            Some(id) if id.contains('.') => id.clone(),
            speaker => {
                let players: Vec<Value> =
                    serde_json::from_str(&ha.template(PLAYERS_TEMPLATE)?).context("parsing Music Assistant players")?;
                let name = |p: &Value| normalize_phrase(p["name"].as_str().unwrap_or_default());
                let names = || players.iter().map(name).collect::<Vec<_>>();
                let found = match speaker {
                    // Music Assistant names its players after the speaker, e.g. "Living Room player".
                    Some(speaker) => {
                        let wanted = normalize_phrase(speaker);
                        players
                            .iter()
                            .find(|p| name(p) == wanted)
                            .or_else(|| players.iter().find(|p| name(p).starts_with(&wanted)))
                            .ok_or_else(|| {
                                anyhow!("no Music Assistant player called {speaker:?} (have {:?})", names())
                            })?
                    }
                    None => match players.as_slice() {
                        [only] => only,
                        _ => bail!("set `speaker` to one of the Music Assistant players: {:?}", names()),
                    },
                };
                found["id"].as_str().context("Music Assistant player without an id")?.to_owned()
            }
        };
        self.resolved = Some((entry.clone(), entity.clone()));
        Ok((entry, entity))
    }

    /// Carries out a command; the answer to speak.
    pub fn run(&mut self, ha: Option<&HomeAssistant>, command: &MusicCommand, lang: Lang) -> String {
        let no = lang == Lang::Norwegian;
        let Some(ha) = ha else {
            return if no { "Home Assistant er ikke koblet til ennå." } else { "Home Assistant isn't connected yet." }
                .into();
        };
        let result = match command {
            MusicCommand::Liked
            | MusicCommand::Playlist(_)
            | MusicCommand::Song(_)
            | MusicCommand::Artist(_)
            | MusicCommand::Album(_) => self.start(ha, command, no),
            MusicCommand::NowPlaying => self.now_playing(ha, no),
            _ => self.control(ha, command, no),
        };
        result.unwrap_or_else(|error| {
            warn!(%error, ?command, "music request failed");
            // Look Music Assistant up again next time: the add-on may have restarted or been set up since.
            self.resolved = None;
            if no { "Beklager, det gikk ikke å styre musikken." } else { "Sorry, I couldn't control the music." }.into()
        })
    }

    fn start(&mut self, ha: &HomeAssistant, command: &MusicCommand, no: bool) -> Result<String> {
        let (entry, entity) = self.resolve(ha)?;
        let (found, media_type, answer) = match command {
            MusicCommand::Song(request) => {
                let artists = self.library_artists(ha, &entry)?;
                let names = Names::new(artists.iter().map(|a| a.name.as_str()));
                let words = request.split_whitespace().count();
                // The whole request is one of your artists ("play kakma de faka"): play the artist.
                if let Some(m) = names.best(request).filter(|m| m.words.len() == words) {
                    let artist =
                        artists.iter().find(|a| a.name == m.name).cloned().context("matched artist vanished")?;
                    info!(heard = request, artist = artist.name, score = m.score, "artist matched");
                    let answer =
                        if no { format!("Spiller {}.", artist.name) } else { format!("Playing {}.", artist.name) };
                    return self.play(ha, &entity, &artist, "artist", answer);
                }
                // Part of it is ("play hello by the neighborhood"): search with the right spelling.
                let corrected = names.best(request).map(|m| {
                    let words: Vec<&str> = request.split_whitespace().collect();
                    [&words[..m.words.start], &[m.name], &words[m.words.end..]].concat().join(" ")
                });
                let found = match search_track(ha, &entry, corrected.as_deref().unwrap_or(request))? {
                    Some(track) => Some(track),
                    None if corrected.is_some() => search_track(ha, &entry, request)?,
                    None => None,
                };
                let Some(track) = found else {
                    return Ok(if no {
                        format!("Jeg fant ikke {request}.")
                    } else {
                        format!("I couldn't find {request}.")
                    });
                };
                let answer = match (no, track.artist.is_empty()) {
                    (false, false) => format!("Playing {} by {}.", track.name, track.artist),
                    (false, true) => format!("Playing {}.", track.name),
                    (true, false) => format!("Spiller {} av {}.", track.name, track.artist),
                    (true, true) => format!("Spiller {}.", track.name),
                };
                (track, "track", answer)
            }
            MusicCommand::Artist(request) => {
                let artists = self.library_artists(ha, &entry)?;
                let names = Names::new(artists.iter().map(|a| a.name.as_str()));
                let mine = names.best(request).and_then(|m| artists.iter().find(|a| a.name == m.name)).cloned();
                let Some(artist) = mine.map_or_else(|| search(ha, &entry, request, "artist"), |a| Ok(Some(a)))? else {
                    return Ok(if no {
                        format!("Jeg fant ikke {request}.")
                    } else {
                        format!("I couldn't find {request}.")
                    });
                };
                let answer = if no { format!("Spiller {}.", artist.name) } else { format!("Playing {}.", artist.name) };
                (artist, "artist", answer)
            }
            MusicCommand::Album(request) => {
                let Some(album) = search(ha, &entry, request, "album")? else {
                    return Ok(if no {
                        format!("Jeg fant ikke albumet {request}.")
                    } else {
                        format!("I couldn't find the album {request}.")
                    });
                };
                let answer = match (no, album.artist.is_empty()) {
                    (false, false) => format!("Playing {} by {}.", album.name, album.artist),
                    (false, true) => format!("Playing {}.", album.name),
                    (true, false) => format!("Spiller {} av {}.", album.name, album.artist),
                    (true, true) => format!("Spiller {}.", album.name),
                };
                (album, "album", answer)
            }
            MusicCommand::Playlist(name) => {
                let Some(list) = find_playlist(ha, &entry, name)? else {
                    return Ok(if no {
                        format!("Jeg fant ingen spilleliste som heter {name}.")
                    } else {
                        format!("I couldn't find a playlist called {name}.")
                    });
                };
                let answer = if no {
                    format!("Spiller spillelisten {}.", list.name)
                } else {
                    format!("Playing your {} playlist.", list.name)
                };
                (list, "playlist", answer)
            }
            _ => {
                let list = match &self.liked {
                    Some(list) => list.clone(),
                    None => liked_songs(ha, &entry)?
                        .ok_or_else(|| anyhow!("no Liked Songs playlist in Music Assistant (is Spotify added?)"))?,
                };
                self.liked = Some(list.clone());
                let answer = if no { "Spiller de likte sangene dine." } else { "Shuffling your liked songs." };
                (list, "playlist", answer.to_owned())
            }
        };
        self.play(ha, &entity, &found, media_type, answer)
    }

    fn play(
        &self,
        ha: &HomeAssistant,
        entity: &str,
        found: &Found,
        media_type: &str,
        answer: String,
    ) -> Result<String> {
        info!(uri = found.uri, name = found.name, artist = found.artist, entity, dry_run = self.dry_run, "playing");
        if self.dry_run {
            return Ok(answer);
        }
        // Starting answers only once the speaker plays, over a second later; the music starting is
        // the answer, so nothing waits for it.
        let (ha, entity, uri, media_type) =
            (ha.detached(), entity.to_owned(), found.uri.clone(), media_type.to_owned());
        std::thread::spawn(move || {
            // Before the start, so the queue is shuffled as it loads, first song included.
            let shuffle = !matches!(media_type.as_str(), "track" | "album");
            if let Err(error) =
                ha.service("media_player", "shuffle_set", json!({ "entity_id": entity, "shuffle": shuffle }))
            {
                warn!(%error, "could not set shuffle");
            }
            let play = json!({ "entity_id": entity, "media_id": uri, "media_type": media_type, "enqueue": "replace" });
            if let Err(error) = ha.service("music_assistant", "play_media", play) {
                warn!(error = format!("{error:#}"), "could not start the music");
            }
        });
        Ok(answer)
    }

    fn library_artists(&mut self, ha: &HomeAssistant, entry: &str) -> Result<Vec<Found>> {
        if let Some((at, artists)) = &self.artists {
            if at.elapsed() < LIBRARY_REFRESH {
                return Ok(artists.clone());
            }
        }
        let artists = library(ha, entry, "artist", None)?;
        self.artists = Some((std::time::Instant::now(), artists.clone()));
        Ok(artists)
    }

    fn control(&mut self, ha: &HomeAssistant, command: &MusicCommand, no: bool) -> Result<String> {
        let (_, entity) = self.resolve(ha)?;
        let (service, answer) = match command {
            MusicCommand::Pause => ("media_pause", if no { "Ok, pause." } else { "Okay, paused." }),
            MusicCommand::Resume => ("media_play", if no { "Ok." } else { "Okay." }),
            MusicCommand::Next => ("media_next_track", if no { "Ok, neste." } else { "Okay, next song." }),
            MusicCommand::Previous => {
                ("media_previous_track", if no { "Ok, forrige." } else { "Okay, previous song." })
            }
            MusicCommand::Louder | MusicCommand::Quieter | MusicCommand::VolumeBy(_) | MusicCommand::VolumeTo(_) => {
                // A set level needs no reading of the current one first.
                let now = match command {
                    MusicCommand::VolumeTo(_) => None,
                    _ => Some(ha.state(&entity)?["attributes"]["volume_level"].as_f64().unwrap_or(0.3)),
                };
                let percent = match (command, now.unwrap_or(0.0) * 100.0) {
                    (MusicCommand::Louder, now) => now + VOLUME_STEP,
                    (MusicCommand::Quieter, now) => now - VOLUME_STEP,
                    (MusicCommand::VolumeBy(points), now) => now + f64::from(*points),
                    (MusicCommand::VolumeTo(percent), _) => f64::from(*percent),
                    _ => unreachable!(),
                }
                .round()
                .clamp(0.0, 100.0);
                let level = volume_level(percent);
                ha.service("media_player", "volume_set", json!({ "entity_id": entity, "volume_level": level }))?;
                info!(from = ?now, to = percent, "volume set");
                return Ok(if no {
                    format!("Volumet er {percent} prosent.")
                } else {
                    format!("Volume {percent} percent.")
                });
            }
            MusicCommand::Stop => ("media_stop", if no { "Ok, stoppet." } else { "Okay, stopped." }),
            MusicCommand::Mute(mute) => {
                ha.service("media_player", "volume_mute", json!({ "entity_id": entity, "is_volume_muted": mute }))?;
                return Ok(match (mute, no) {
                    (true, false) => "Okay, muted.",
                    (false, false) => "Okay, unmuted.",
                    (true, true) => "Ok, dempet.",
                    (false, true) => "Ok, lyden er på.",
                }
                .into());
            }
            MusicCommand::Shuffle(on) => {
                ha.service("media_player", "shuffle_set", json!({ "entity_id": entity, "shuffle": on }))?;
                return Ok(match (on, no) {
                    (true, false) => "Okay, shuffle is on.",
                    (false, false) => "Okay, shuffle is off.",
                    (true, true) => "Ok, tilfeldig rekkefølge.",
                    (false, true) => "Ok, i rekkefølge.",
                }
                .into());
            }
            MusicCommand::Repeat(repeat) => {
                let mode = match repeat {
                    Repeat::Off => "off",
                    Repeat::All => "all",
                    Repeat::One => "one",
                };
                ha.service("media_player", "repeat_set", json!({ "entity_id": entity, "repeat": mode }))?;
                return Ok(match (repeat, no) {
                    (Repeat::One, false) => "Okay, repeating this song.",
                    (Repeat::All, false) => "Okay, repeat is on.",
                    (Repeat::Off, false) => "Okay, repeat is off.",
                    (Repeat::One, true) => "Ok, gjentar denne sangen.",
                    (Repeat::All, true) => "Ok, gjentar.",
                    (Repeat::Off, true) => "Ok, gjentar ikke.",
                }
                .into());
            }
            _ => unreachable!("start() handles play requests"),
        };
        ha.service("media_player", service, json!({ "entity_id": entity }))?;
        info!(service, entity, "speaker");
        Ok(answer.into())
    }

    /// The song playing, from the player's state. The title is a sentence of its own: the rest of
    /// what is said stays the same.
    fn now_playing(&mut self, ha: &HomeAssistant, no: bool) -> Result<String> {
        let (_, entity) = self.resolve(ha)?;
        let state = ha.state(&entity)?;
        let text = |key: &str| state["attributes"][key].as_str().unwrap_or_default().to_owned();
        let (title, artist) = (text("media_title"), text("media_artist"));
        // Paused for this very question, the player can say idle while it still holds the song.
        info!(state = state["state"].as_str(), title, artist, "now playing");
        if title.is_empty() || matches!(state["state"].as_str(), Some("off" | "unavailable" | "unknown")) {
            return Ok(if no { "Ingenting spilles." } else { "Nothing is playing." }.into());
        }
        Ok(match (artist.is_empty(), no) {
            (true, false) => format!("This is {title}."),
            (false, false) => format!("This is {title} by {artist}."),
            (true, true) => format!("Dette er {title}."),
            (false, true) => format!("Dette er {title} av {artist}."),
        })
    }
}

/// The best catalogue match of a kind ("artist", "album") for a spoken name.
fn search(ha: &HomeAssistant, entry: &str, name: &str, media_type: &str) -> Result<Option<Found>> {
    let data = json!({ "config_entry_id": entry, "name": name, "media_type": [media_type], "limit": 5 });
    let response = ha.service_response("music_assistant", "search", data)?;
    Ok(response[format!("{media_type}s")].as_array().and_then(|items| items.first()).and_then(found))
}

/// The best match for a spoken song request. "X by Y" searches title and artist separately.
fn search_track(ha: &HomeAssistant, entry: &str, request: &str) -> Result<Option<Found>> {
    let search = |name: &str, artist: Option<&str>| -> Result<Option<Found>> {
        let mut data = json!({ "config_entry_id": entry, "name": name, "media_type": ["track"], "limit": 5 });
        if let Some(artist) = artist {
            data["artist"] = artist.into();
        }
        let response = ha.service_response("music_assistant", "search", data)?;
        Ok(response["tracks"].as_array().and_then(|t| t.first()).and_then(found))
    };
    if let Some((title, artist)) = request.rsplit_once(" by ") {
        if !title.trim().is_empty() && !artist.trim().is_empty() {
            if let Some(track) = search(title.trim(), Some(artist.trim()))? {
                return Ok(Some(track));
            }
        }
    }
    // Also when "by" belongs to the title ("Stand by Me").
    search(request, None)
}

/// One of the user's playlists by name, from the Music Assistant library.
fn find_playlist(ha: &HomeAssistant, entry: &str, name: &str) -> Result<Option<Found>> {
    let wanted = normalize_phrase(name);
    let mut lists = library_playlists(ha, entry, Some(name))?;
    if lists.is_empty() {
        lists = library_playlists(ha, entry, None)?;
    }
    let names: Vec<String> = lists.iter().map(|p| normalize_phrase(&p.name)).collect();
    let words: Vec<&str> = wanted.split(' ').collect();
    let index = names
        .iter()
        .position(|n| *n == wanted)
        .or_else(|| names.iter().position(|n| n.contains(&wanted)))
        .or_else(|| names.iter().position(|n| words.iter().all(|w| n.split(' ').any(|h| h == *w))));
    if let Some(i) = index {
        return Ok(Some(lists.swap_remove(i)));
    }
    // Misheard ("my cost playlist" for Kos): match against every playlist by sound.
    let all = library(ha, entry, "playlist", None)?;
    let matched = Names::new(all.iter().map(|p| p.name.as_str())).best(name).map(|m| (m.name.to_owned(), m.score));
    Ok(matched.and_then(|(found, score)| {
        info!(heard = name, playlist = found, score, "playlist matched");
        all.into_iter().find(|p| p.name == found)
    }))
}

/// Spotify's Liked Songs, which Music Assistant lists as a playlist.
fn liked_songs(ha: &HomeAssistant, entry: &str) -> Result<Option<Found>> {
    let liked = |p: &Found| {
        let name = normalize_phrase(&p.name);
        name.contains("liked") || name.contains("likte")
    };
    if let Some(list) = library_playlists(ha, entry, Some("liked"))?.into_iter().find(liked) {
        return Ok(Some(list));
    }
    Ok(library_playlists(ha, entry, None)?.into_iter().find(liked))
}

fn library_playlists(ha: &HomeAssistant, entry: &str, search: Option<&str>) -> Result<Vec<Found>> {
    library(ha, entry, "playlist", search)
}

fn library(ha: &HomeAssistant, entry: &str, media_type: &str, search: Option<&str>) -> Result<Vec<Found>> {
    let mut data = json!({ "config_entry_id": entry, "media_type": media_type, "limit": 500 });
    if let Some(search) = search {
        data["search"] = search.into();
    }
    let response = ha.service_response("music_assistant", "get_library", data)?;
    Ok(response["items"].as_array().map(|items| items.iter().filter_map(found).collect()).unwrap_or_default())
}

fn found(item: &Value) -> Option<Found> {
    Some(Found {
        uri: item["uri"].as_str()?.to_owned(),
        name: item["name"].as_str()?.to_owned(),
        artist: item["artists"][0]["name"].as_str().unwrap_or_default().to_owned(),
    })
}

fn normalize_phrase(text: &str) -> String {
    text.split_whitespace().map(normalize).filter(|w| !w.is_empty()).collect::<Vec<_>>().join(" ")
}
