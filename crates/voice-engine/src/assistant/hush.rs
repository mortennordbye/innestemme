//! Music paused for a request and resumed after it.

use std::time::{Duration, Instant};

use tracing::{info, warn};
use voice_assistant::ha::Playing;

use super::Worker;

/// Music paused for a request plays on after this long with nothing heard (a wake with no request,
/// speech that was not speech), and at the latest after `HUSH_CAP`, so talk in the room cannot
/// keep it paused.
pub(super) const HUSH_QUIET: Duration = Duration::from_secs(12);
pub(super) const HUSH_CAP: Duration = Duration::from_secs(40);

/// Players paused for a request.
pub(super) struct Hushed {
    pub(super) players: Vec<Playing>,
    pub(super) at: Instant,
    pub(super) last_heard: Instant,
}

/// The players to pause for a request. While Music Assistant plays on a speaker, the speaker's own
/// player reports playing too; pausing and resuming it as well races Music Assistant's stream and
/// can bring back the speaker's last radio station instead.
fn players_to_hush(players: Vec<Playing>, music: &[String]) -> Vec<Playing> {
    let assistant_plays = players.iter().any(|p| music.contains(&p.id));
    players.into_iter().filter(|p| !(assistant_plays && p.sonos)).collect()
}

impl Worker {
    /// Music Assistant's player is playing.
    pub(super) fn music_playing(&mut self) -> bool {
        let Some(ha) = &self.home_assistant else { return false };
        let Ok(entity) = self.player.check(ha) else { return false };
        ha.state(&entity).is_ok_and(|s| s["state"] == "playing")
    }

    /// Pauses the music for the request that follows a wake word.
    pub(super) fn hush(&mut self) {
        if let Some(hushed) = &mut self.hushed {
            hushed.last_heard = Instant::now();
            return;
        }
        let Some(ha) = &mut self.home_assistant else { return };
        // The Sonos itself can be playing while its Music Assistant player says idle, so both count.
        let music = self.player.check(ha).ok().into_iter().collect::<Vec<_>>();
        let players = match ha.playing(self.room.as_deref(), &music) {
            Ok(players) => players,
            Err(error) => return warn!(error = format!("{error:#}"), "could not see what is playing"),
        };
        let players = players_to_hush(players, &music);
        if players.is_empty() {
            return;
        }
        let ids: Vec<&str> = players.iter().map(|p| p.id.as_str()).collect();
        if let Err(error) = ha.send("media_player", "media_pause", serde_json::json!({ "entity_id": ids })) {
            return warn!(error = format!("{error:#}"), "could not pause the music");
        }
        info!(players = ?ids, "music paused for a request");
        let now = Instant::now();
        self.hushed = Some(Hushed { players, at: now, last_heard: now });
    }

    pub(super) fn unhush(&mut self) {
        let Some(hushed) = self.hushed.take() else { return };
        let Some(ha) = &self.home_assistant else { return };
        // A speaker's own player resumes whatever its transport holds by then (a radio station, a
        // stale stream), so only one still holding what was paused plays on.
        let ids: Vec<&str> = hushed
            .players
            .iter()
            .filter(|p| {
                !p.sonos || ha.state(&p.id).is_ok_and(|s| s["attributes"]["media_content_id"] == p.content.as_str())
            })
            .map(|p| p.id.as_str())
            .collect();
        if ids.len() < hushed.players.len() {
            info!(players = ?hushed.players.iter().map(|p| &p.id).collect::<Vec<_>>(), "a paused speaker changed source; not resuming it");
        }
        if ids.is_empty() {
            return;
        }
        match ha.send("media_player", "media_play", serde_json::json!({ "entity_id": ids })) {
            Ok(()) => info!(players = ?ids, "music resumed after a request"),
            Err(error) => warn!(error = format!("{error:#}"), "could not resume the music"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hushes_music_assistant_not_the_speaker_under_it() {
        let playing = |id: &str, sonos| Playing { id: id.into(), sonos, content: String::new() };
        let music = vec!["media_player.living_room_2".to_owned()];
        let hushed = |players| players_to_hush(players, &music).into_iter().map(|p| p.id).collect::<Vec<_>>();
        assert_eq!(
            hushed(vec![
                playing("media_player.living_room_2", false),
                playing("media_player.living_room", true),
                playing("media_player.tv", false),
            ]),
            ["media_player.living_room_2", "media_player.tv"]
        );
        // Spotify from a phone plays on the speaker alone: that is paused.
        assert_eq!(hushed(vec![playing("media_player.living_room", true)]), ["media_player.living_room"]);
    }
}
