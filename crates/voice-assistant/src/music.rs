//! Music through Music Assistant (the Home Assistant add-on), which holds the Spotify sign-in,
//! searches the catalogue and plays on the Sonos. Everything is a Home Assistant service call:
//! `music_assistant.search` / `get_library` to find what was asked for, `music_assistant.play_media`
//! to start it, and the usual `media_player.*` services on the Music Assistant player for the rest.

use std::sync::Arc;

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

/// Music Assistant's media players with their names and rooms, rendered by Home Assistant.
const PLAYERS_TEMPLATE: &str = r#"[{% for id in integration_entities("music_assistant") | select("match", "media_player[.]") %}{"id": {{ id | tojson }}, "name": {{ (state_attr(id, "friendly_name") or id) | tojson }}, "area": {{ (area_name(id) or "") | tojson }}}{% if not loop.last %},{% endif %}{% endfor %}]"#;

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
    "The music is paused.",
    "Shuffling your liked songs.",
    "Sorry, I couldn't play that.",
    "Nothing is queued after this song.",
];

/// How new music joins what plays: Music Assistant's `enqueue` option.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Enqueue {
    Replace,
    /// After the song playing.
    Next,
    /// At the end of the queue.
    Add,
}

/// What a play request found, and how an answer names it.
struct Pick {
    found: Found,
    media_type: &'static str,
    /// "Hello by Adele", "your Kos playlist"; in Norwegian for a Norwegian answer.
    label: String,
}

/// A room's speaker: its player, and the room as said for the answer.
#[derive(Debug, Clone, Copy)]
struct On<'a> {
    entity: &'a str,
    room: &'a str,
}

/// A Music Assistant player and the Home Assistant area it is in ("" when none).
#[derive(Debug, Clone, PartialEq)]
struct Speaker {
    id: String,
    name: String,
    area: String,
}

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
    /// See [`Player::paused_for_request`].
    held: bool,
    late_failure: Option<Arc<dyn Fn(String, Lang) + Send + Sync>>,
}

/// The library changes rarely.
const LIBRARY_REFRESH: std::time::Duration = std::time::Duration::from_secs(600);

impl Player {
    pub fn new(speaker: Option<String>) -> Self {
        Self { speaker, resolved: None, artists: None, liked: None, dry_run: false, held: false, late_failure: None }
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
                let players = speakers(ha)?;
                let name = |p: &Speaker| normalize_phrase(&p.name);
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
                found.id.clone()
            }
        };
        self.resolved = Some((entry.clone(), entity.clone()));
        Ok((entry, entity))
    }

    /// The player for a room as said, if it has one.
    pub fn room_player(&self, ha: &HomeAssistant, room: &str) -> Option<String> {
        let players = speakers(ha).ok()?;
        in_room(&players, room).map(|p| p.id.clone())
    }

    /// Music Assistant and the player a command is for: a room's, else the speaker's.
    fn target(&mut self, ha: &HomeAssistant, on: Option<On>) -> Result<(String, String)> {
        let (entry, entity) = self.resolve(ha)?;
        Ok((entry, on.map_or(entity, |on| on.entity.to_owned())))
    }

    /// Carries out a command; the answer to speak.
    pub fn run(&mut self, ha: Option<&HomeAssistant>, command: &MusicCommand, lang: Lang) -> String {
        let no = lang == Lang::Norwegian;
        let Some(ha) = ha else {
            return if no { "Home Assistant er ikke koblet til ennå." } else { "Home Assistant isn't connected yet." }
                .into();
        };
        self.dispatch(ha, command, no, None).unwrap_or_else(|error| {
            warn!(error = format!("{error:#}"), ?command, "music request failed");
            // Look Music Assistant up again next time: the add-on may have restarted or been set up since.
            self.resolved = None;
            if no { "Beklager, det gikk ikke å styre musikken." } else { "Sorry, I couldn't control the music." }.into()
        })
    }

    fn dispatch(&mut self, ha: &HomeAssistant, command: &MusicCommand, no: bool, on: Option<On>) -> Result<String> {
        match command {
            MusicCommand::InRoom(room, inner) => {
                let players = speakers(ha)?;
                let Some(speaker) = in_room(&players, room) else {
                    return Ok(no_speaker(room, &players, no));
                };
                info!(room, player = speaker.id, "music in a room");
                self.dispatch(ha, inner, no, Some(On { entity: &speaker.id, room }))
            }
            // One room at a time: grouping players is not done.
            MusicCommand::InRooms(rooms, inner) => {
                let first = MusicCommand::InRoom(rooms[0].clone(), inner.clone());
                let answer = self.dispatch(ha, &first, no, on)?;
                Ok(if no {
                    format!("{answer} Jeg kan bare spille i ett rom om gangen.")
                } else {
                    format!("{answer} I can only play in one room at a time.")
                })
            }
            MusicCommand::MoveTo(to, from) => self.move_to(ha, to, from.as_deref(), no),
            MusicCommand::UpNext => self.up_next(ha, on, no),
            MusicCommand::Queue(inner, _) if **inner == MusicCommand::Resume => Ok(if no {
                "Si hva som skal i køen, for eksempel legg til Kygo i køen."
            } else {
                "Say what to queue, like queue up Hello by Adele."
            }
            .into()),
            MusicCommand::Queue(inner, next) => {
                self.start(ha, inner, no, on, if *next { Enqueue::Next } else { Enqueue::Add })
            }
            MusicCommand::Liked
            | MusicCommand::Playlist(_)
            | MusicCommand::Song(_)
            | MusicCommand::Artist(_)
            | MusicCommand::Album(_) => self.start(ha, command, no, on, Enqueue::Replace),
            MusicCommand::NowPlaying => self.now_playing(ha, on, no),
            _ => self.control(ha, command, no, on),
        }
    }

    fn start(
        &mut self,
        ha: &HomeAssistant,
        command: &MusicCommand,
        no: bool,
        on: Option<On>,
        enqueue: Enqueue,
    ) -> Result<String> {
        let (entry, entity) = self.target(ha, on)?;
        let by = |found: &Found| match (no, found.artist.is_empty()) {
            (_, true) => found.name.clone(),
            (false, false) => format!("{} by {}", found.name, found.artist),
            (true, false) => format!("{} av {}", found.name, found.artist),
        };
        let pick = match command {
            MusicCommand::Song(request) => {
                let artists = self.library_artists(ha, &entry)?;
                let names = Names::new(artists.iter().map(|a| a.name.as_str()));
                let words = request.split_whitespace().count();
                // The whole request is one of your artists ("play kakma de faka"): play the artist.
                if let Some(m) = names.best(request).filter(|m| m.words.len() == words) {
                    let artist =
                        artists.iter().find(|a| a.name == m.name).cloned().context("matched artist vanished")?;
                    info!(heard = request, artist = artist.name, score = m.score, "artist matched");
                    let label = artist.name.clone();
                    Pick { found: artist, media_type: "artist", label }
                } else {
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
                    Pick { label: by(&track), found: track, media_type: "track" }
                }
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
                Pick { label: artist.name.clone(), found: artist, media_type: "artist" }
            }
            MusicCommand::Album(request) => {
                let Some(album) = search(ha, &entry, request, "album")? else {
                    return Ok(if no {
                        format!("Jeg fant ikke albumet {request}.")
                    } else {
                        format!("I couldn't find the album {request}.")
                    });
                };
                Pick { label: by(&album), found: album, media_type: "album" }
            }
            MusicCommand::Playlist(name) => {
                let Some(list) = find_playlist(ha, &entry, name)? else {
                    return Ok(if no {
                        format!("Jeg fant ingen spilleliste som heter {name}.")
                    } else {
                        format!("I couldn't find a playlist called {name}.")
                    });
                };
                let label =
                    if no { format!("spillelisten {}", list.name) } else { format!("your {} playlist", list.name) };
                Pick { found: list, media_type: "playlist", label }
            }
            _ => {
                let list = match &self.liked {
                    Some(list) => list.clone(),
                    None => liked_songs(ha, &entry)?
                        .ok_or_else(|| anyhow!("no Liked Songs playlist in Music Assistant (is Spotify added?)"))?,
                };
                self.liked = Some(list.clone());
                let label = if no { "de likte sangene dine" } else { "your liked songs" };
                Pick { found: list, media_type: "playlist", label: label.into() }
            }
        };
        let answer = play_answer(&pick, enqueue, on.map(|on| on.room), no);
        self.play(ha, &entity, &pick.found, pick.media_type, enqueue, answer, no)
    }

    /// Starts `found` on a thread of its own and answers at once: Music Assistant answers the
    /// call only once the speaker plays, over a second later, and the music starting is the answer.
    /// A call that fails is told afterwards through [`Player::on_late_failure`].
    #[allow(clippy::too_many_arguments)]
    fn play(
        &self,
        ha: &HomeAssistant,
        entity: &str,
        found: &Found,
        media_type: &str,
        enqueue: Enqueue,
        answer: String,
        no: bool,
    ) -> Result<String> {
        info!(
            uri = found.uri,
            name = found.name,
            artist = found.artist,
            entity,
            ?enqueue,
            dry_run = self.dry_run,
            "playing"
        );
        if self.dry_run {
            return Ok(answer);
        }
        let failed = if no { "Beklager, jeg fikk ikke spilt det." } else { "Sorry, I couldn't play that." };
        let (ha, entity, uri, media_type) =
            (ha.detached(), entity.to_owned(), found.uri.clone(), media_type.to_owned());
        let late = self.late_failure.clone();
        std::thread::spawn(move || {
            // Before the start, so the queue is shuffled as it loads, first song included. Queued
            // music leaves the queue's own order alone.
            let shuffle = !matches!(media_type.as_str(), "track" | "album");
            let set_shuffle =
                || ha.service("media_player", "shuffle_set", json!({ "entity_id": entity, "shuffle": shuffle }));
            if enqueue == Enqueue::Replace {
                if let Err(error) = set_shuffle() {
                    warn!(%error, "could not set shuffle");
                }
            }
            let option = match enqueue {
                Enqueue::Replace => "replace",
                Enqueue::Next => "next",
                Enqueue::Add => "add",
            };
            let play = json!({ "entity_id": entity, "media_id": uri, "media_type": media_type, "enqueue": option });
            if let Err(error) = ha.service("music_assistant", "play_media", play) {
                warn!(error = format!("{error:#}"), "could not start the music");
                if let Some(late) = late {
                    late(failed.to_owned(), if no { Lang::Norwegian } else { Lang::English });
                }
            }
        });
        Ok(answer)
    }

    /// `failed` hears the answer to speak, and its language, when music that was already announced
    /// did not start.
    pub fn on_late_failure(&mut self, failed: impl Fn(String, Lang) + Send + Sync + 'static) {
        self.late_failure = Some(Arc::new(failed));
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

    fn control(&mut self, ha: &HomeAssistant, command: &MusicCommand, no: bool, on: Option<On>) -> Result<String> {
        let (_, entity) = self.target(ha, on)?;
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
            _ => unreachable!("dispatch() handles play, room and queue requests"),
        };
        ha.service("media_player", service, json!({ "entity_id": entity }))?;
        info!(service, entity, "speaker");
        Ok(answer.into())
    }

    /// The song playing, from the player's state. The title is a sentence of its own: the rest of
    /// what is said stays the same.
    fn now_playing(&mut self, ha: &HomeAssistant, on: Option<On>, no: bool) -> Result<String> {
        let (_, entity) = self.target(ha, on)?;
        let state = ha.state(&entity)?;
        // Only this room's music is paused for a request.
        let held = self.held && on.is_none();
        info!(
            state = state["state"].as_str(),
            title = state["attributes"]["media_title"].as_str(),
            held,
            "now playing"
        );
        Ok(now_playing_answer(&state, held, no))
    }

    /// The song after this one, from Music Assistant's queue for the player.
    fn up_next(&mut self, ha: &HomeAssistant, on: Option<On>, no: bool) -> Result<String> {
        let (_, entity) = self.target(ha, on)?;
        let response = ha.service_response("music_assistant", "get_queue", json!({ "entity_id": entity }))?;
        // Keyed by the player asked for; older versions answered with the queue itself.
        let queue = if response.get(&entity).is_some() { &response[&entity] } else { &response };
        Ok(up_next_answer(queue, no))
    }

    /// Moves the music's queue to a room's speaker, from the player that has it: the speaker's,
    /// else any other that plays or holds paused music.
    fn move_to(&mut self, ha: &HomeAssistant, room: &str, source: Option<&str>, no: bool) -> Result<String> {
        let (_, here) = self.resolve(ha)?;
        let players = speakers(ha)?;
        let Some(to) = in_room(&players, room) else {
            return Ok(no_speaker(room, &players, no));
        };
        // A room said to move from is where the music is, playing or not.
        let named = match source {
            Some(source) => match in_room(&players, source) {
                Some(player) => Some(player.id.clone()),
                None => return Ok(no_speaker(source, &players, no)),
            },
            None => None,
        };
        let has_music = |id: &str| {
            ha.state(id).is_ok_and(|s| matches!(s["state"].as_str(), Some("playing" | "paused")))
                || (id == here && self.held)
        };
        let others = players.iter().map(|p| p.id.as_str()).filter(|id| *id != here);
        let found = named.as_deref().or_else(|| std::iter::once(here.as_str()).chain(others).find(|id| has_music(id)));
        let Some(from) = found else {
            return Ok(if no { "Ingenting spilles." } else { "Nothing is playing." }.into());
        };
        if from == to.id {
            return Ok(if no {
                format!("Musikken spiller allerede på {room}.")
            } else {
                format!("The music is already playing in the {room}.")
            });
        }
        info!(from, to = to.id, "moving the music");
        if !self.dry_run {
            let data = json!({ "entity_id": to.id, "source_player": from, "auto_play": true });
            ha.service("music_assistant", "transfer_queue", data)?;
        }
        Ok(if no {
            format!("Ok, flytter musikken til {room}.")
        } else {
            format!("Okay, moving the music to the {room}.")
        })
    }

    /// The music was paused for the request being answered, so a paused or idle player still
    /// counts as playing for "what's playing".
    pub fn paused_for_request(&mut self, held: bool) {
        self.held = held;
    }
}

fn speakers(ha: &HomeAssistant) -> Result<Vec<Speaker>> {
    parse_speakers(&ha.template(PLAYERS_TEMPLATE)?)
}

fn parse_speakers(json: &str) -> Result<Vec<Speaker>> {
    let players: Vec<Value> = serde_json::from_str(json).context("parsing Music Assistant players")?;
    let text = |p: &Value, key: &str| p[key].as_str().unwrap_or_default().to_owned();
    Ok(players
        .iter()
        .filter(|p| p["id"].is_string())
        .map(|p| Speaker { id: text(p, "id"), name: text(p, "name"), area: text(p, "area") })
        .collect())
}

/// Words in a player's name that say what it is, not where: "Kitchen speaker", "Bedroom Sonos".
const PLAYER_WORDS: &[&str] = &["player", "speaker", "speakers", "sonos", "høyttaler", "høyttaleren", "ma"];

/// A player's name without its kind: "Kitchen player" is "kitchen".
fn room_of_name(name: &str) -> String {
    let words: Vec<String> = name
        .split_whitespace()
        .map(normalize)
        .filter(|w| !w.is_empty() && !PLAYER_WORDS.contains(&w.as_str()))
        .collect();
    words.join(" ")
}

/// The player for a room as said ("kitchen", "badet"): one in that Home Assistant area, else one
/// named after the room, in either language.
fn in_room<'a>(players: &'a [Speaker], room: &str) -> Option<&'a Speaker> {
    players
        .iter()
        .find(|p| !p.area.is_empty() && crate::ha::is_room(&p.area, room))
        .or_else(|| players.iter().find(|p| crate::ha::is_room(&room_of_name(&p.name), room)))
}

/// No speaker in the room asked for: which rooms have one.
fn no_speaker(room: &str, players: &[Speaker], no: bool) -> String {
    let lang = if no { Lang::Norwegian } else { Lang::English };
    let mut rooms: Vec<String> = Vec::new();
    for player in players {
        let place = if player.area.is_empty() { room_of_name(&player.name) } else { player.area.clone() };
        // In the answer's language when it is a room people name ("Stue" is "the living room").
        let place = match crate::ha::room_name(&place, lang) {
            Some(name) if no => name.to_owned(),
            Some(name) => format!("the {name}"),
            None if no => place.to_lowercase(),
            None => place,
        };
        if !place.is_empty() && !rooms.contains(&place) {
            rooms.push(place);
        }
    }
    match (rooms.is_empty(), no) {
        (true, false) => format!("I can't find a speaker in the {room}."),
        (true, true) => format!("Jeg finner ingen høyttaler på {room}."),
        (false, false) => {
            format!("I can't find a speaker in the {room}. There are speakers in {}.", crate::house::join(&rooms, lang))
        }
        (false, true) => {
            format!("Jeg finner ingen høyttaler på {room}. Det er høyttalere på {}.", crate::house::join(&rooms, lang))
        }
    }
}

/// What is said when music starts or is queued: "Playing Hello by Adele.", "Okay, I'll play
/// Hello by Adele next.", with the room when it is another speaker's.
fn play_answer(pick: &Pick, enqueue: Enqueue, room: Option<&str>, no: bool) -> String {
    let at = match (room, no) {
        (None, _) => String::new(),
        (Some(room), false) => format!(" in the {room}"),
        (Some(room), true) => format!(" på {room}"),
    };
    let label = &pick.label;
    let liked = pick.media_type == "playlist" && (label == "your liked songs" || label == "de likte sangene dine");
    match (enqueue, no) {
        (Enqueue::Replace, false) if liked => format!("Shuffling your liked songs{at}."),
        (Enqueue::Replace, true) if liked => format!("Spiller de likte sangene dine{at}."),
        (Enqueue::Replace, false) => format!("Playing {label}{at}."),
        (Enqueue::Replace, true) => format!("Spiller {label}{at}."),
        (Enqueue::Next, false) => format!("Okay, I'll play {label} next{at}."),
        (Enqueue::Next, true) => format!("Ok, jeg spiller {label} etter denne{at}."),
        (Enqueue::Add, false) => format!("Okay, I've added {label} to the queue{at}."),
        (Enqueue::Add, true) => format!("Ok, jeg la {label} i køen{at}."),
    }
}

/// The song after this one in a Music Assistant queue (`music_assistant.get_queue`).
fn up_next_answer(queue: &Value, no: bool) -> String {
    let item = &queue["next_item"];
    let media = &item["media_item"];
    let title = media["name"].as_str().or_else(|| item["name"].as_str()).unwrap_or_default();
    let artist = media["artists"][0]["name"].as_str().unwrap_or_default();
    match (title.is_empty(), artist.is_empty(), no) {
        (true, _, false) => "Nothing is queued after this song.".into(),
        (true, _, true) => "Det er ingenting i køen etter denne sangen.".into(),
        (false, true, false) => format!("Next is {title}."),
        (false, true, true) => format!("Neste er {title}."),
        (false, false, false) => format!("Next is {title} by {artist}."),
        (false, false, true) => format!("Neste er {title} av {artist}."),
    }
}

fn now_playing_answer(state: &Value, held: bool, no: bool) -> String {
    let text = |key: &str| state["attributes"][key].as_str().unwrap_or_default().to_owned();
    let (title, artist) = (text("media_title"), text("media_artist"));
    let nothing = if no { "Ingenting spilles." } else { "Nothing is playing." };
    // Paused for this very question, the player can say idle while it still holds the song.
    let state = state["state"].as_str().unwrap_or_default();
    if title.is_empty() || matches!(state, "off" | "unavailable" | "unknown" | "") || (state == "idle" && !held) {
        return nothing.into();
    }
    let song = match (artist.is_empty(), no) {
        (true, _) => format!("{title}."),
        (false, false) => format!("{title} by {artist}."),
        (false, true) => format!("{title} av {artist}."),
    };
    match (state == "paused" && !held, no) {
        (true, false) => format!("The music is paused. It's {song}"),
        (true, true) => format!("Musikken står på pause. Det er {song}"),
        (false, false) => format!("This is {song}"),
        (false, true) => format!("Dette er {song}"),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn player(state: &str) -> Value {
        json!({ "state": state, "attributes": { "media_title": "Hello", "media_artist": "Adele" } })
    }

    #[test]
    fn now_playing_tells_a_paused_player_from_one_paused_for_the_question() {
        assert_eq!(now_playing_answer(&player("playing"), false, false), "This is Hello by Adele.");
        assert_eq!(now_playing_answer(&player("paused"), true, false), "This is Hello by Adele.");
        assert_eq!(now_playing_answer(&player("idle"), true, false), "This is Hello by Adele.");
        assert_eq!(now_playing_answer(&player("paused"), false, false), "The music is paused. It's Hello by Adele.");
        assert_eq!(now_playing_answer(&player("idle"), false, false), "Nothing is playing.");
        assert_eq!(now_playing_answer(&player("off"), true, false), "Nothing is playing.");
        let untitled = json!({ "state": "playing", "attributes": {} });
        assert_eq!(now_playing_answer(&untitled, false, false), "Nothing is playing.");
    }

    fn players() -> Vec<Speaker> {
        parse_speakers(
            r#"[{"id": "media_player.stue_2", "name": "Stue", "area": "Stue"},
                {"id": "media_player.kitchen_ma", "name": "Kitchen speaker", "area": ""},
                {"id": "media_player.bed", "name": "Sonos One", "area": "Bedroom"}]"#,
        )
        .unwrap()
    }

    #[test]
    fn finds_a_room_speaker_by_area_or_name_in_either_language() {
        let players = players();
        let id = |room: &str| in_room(&players, room).map(|p| p.id.as_str());
        assert_eq!(id("living room"), Some("media_player.stue_2"));
        assert_eq!(id("stua"), Some("media_player.stue_2"));
        assert_eq!(id("kitchen"), Some("media_player.kitchen_ma"));
        assert_eq!(id("kjøkkenet"), Some("media_player.kitchen_ma"));
        assert_eq!(id("soverommet"), Some("media_player.bed"));
        assert_eq!(id("bathroom"), None);
        assert_eq!(
            no_speaker("bathroom", &players, false),
            "I can't find a speaker in the bathroom. There are speakers in the living room, the kitchen and the bedroom."
        );
        // Rooms in the answer's language, whatever Home Assistant calls them.
        assert_eq!(
            no_speaker("badet", &players, true),
            "Jeg finner ingen høyttaler på badet. Det er høyttalere på stua, kjøkkenet og soverommet."
        );
        let odd = [Speaker { id: "media_player.x".into(), name: "X".into(), area: "Loft".into() }];
        assert_eq!(
            no_speaker("bathroom", &odd, false),
            "I can't find a speaker in the bathroom. There are speakers in Loft."
        );
        assert_eq!(no_speaker("bathroom", &[], false), "I can't find a speaker in the bathroom.");
    }

    #[test]
    fn says_where_and_when_music_plays() {
        let found = Found { uri: "spotify://x".into(), name: "Hello".into(), artist: "Adele".into() };
        let pick = |label: &str, media_type| Pick { found: found.clone(), media_type, label: label.into() };
        let song = pick("Hello by Adele", "track");
        assert_eq!(play_answer(&song, Enqueue::Replace, None, false), "Playing Hello by Adele.");
        assert_eq!(
            play_answer(&song, Enqueue::Replace, Some("kitchen"), false),
            "Playing Hello by Adele in the kitchen."
        );
        assert_eq!(play_answer(&song, Enqueue::Next, None, false), "Okay, I'll play Hello by Adele next.");
        assert_eq!(play_answer(&song, Enqueue::Add, None, false), "Okay, I've added Hello by Adele to the queue.");
        assert_eq!(
            play_answer(&pick("Hello av Adele", "track"), Enqueue::Replace, Some("badet"), true),
            "Spiller Hello av Adele på badet."
        );
        assert_eq!(
            play_answer(&pick("Hello av Adele", "track"), Enqueue::Add, None, true),
            "Ok, jeg la Hello av Adele i køen."
        );
        let liked = pick("your liked songs", "playlist");
        assert_eq!(play_answer(&liked, Enqueue::Replace, None, false), "Shuffling your liked songs.");
        assert_eq!(
            play_answer(&liked, Enqueue::Replace, Some("bedroom"), false),
            "Shuffling your liked songs in the bedroom."
        );
    }

    #[test]
    fn tells_the_next_song_in_the_queue() {
        let queue = json!({ "next_item": { "name": "Adele - Hello", "media_item": { "name": "Hello", "artists": [{ "name": "Adele" }] } } });
        assert_eq!(up_next_answer(&queue, false), "Next is Hello by Adele.");
        assert_eq!(up_next_answer(&queue, true), "Neste er Hello av Adele.");
        let bare = json!({ "next_item": { "name": "Radio stream" } });
        assert_eq!(up_next_answer(&bare, false), "Next is Radio stream.");
        assert_eq!(up_next_answer(&json!({ "next_item": null }), false), "Nothing is queued after this song.");
        assert!(ENGLISH_ANSWERS.contains(&up_next_answer(&json!({}), false).as_str()));
    }
}
