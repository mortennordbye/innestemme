//! Drafts that are not whole requests, and music requests misheard. Which requests are acted
//! on at once or without words is in `Intent::meta`.

use voice_assistant::intent::{self, Intent};

/// "Stop", "stop it": never mind, or the music when it plays.
pub(super) fn bare_stop(request: &str) -> bool {
    let words = voice_assistant::dialog::words(request);
    matches!(words.as_slice(), [w] | [w, _] if ["stop", "stopp"].contains(&w.as_str()))
        && intent::parse(request) == Intent::Cancel
}

/// Whisper hears "pause" and "play" after the wake word as "please", "nice", "post": a short request
/// that ends in "music" and is nothing else means pausing it when it was playing, else playing it.
/// A prompt with the command words fixes some of these but makes up commands from room noise.
pub(super) fn music_by_sound(request: &str, playing: bool) -> Option<String> {
    let words = voice_assistant::dialog::words(request);
    // "Stop" over music means the music, not "never mind".
    if bare_stop(request) && playing {
        return Some("pause the music".into());
    }
    // Over music, a short request with a percentage and no light is the volume: "put the percentage
    // at 70 percent", "trop with the sign at 50 percent" (drop the sound).
    if playing && words.len() <= 8 && intent::parse(request) == Intent::Unknown {
        let percent = words.windows(2).find_map(|p| {
            let n = p[0].parse::<u8>().ok().filter(|n| *n <= 100)?;
            ["percent", "prosent"].contains(&p[1].as_str()).then_some(n)
        });
        if let Some(n) = percent {
            return Some(format!("set the volume to {n}"));
        }
    }
    // "Play my liked songs" comes out as "claim a light song", "they're my like songs", "clean
    // my life's songs": the play is lost under the wake word's end and "liked" sounds like these.
    let songs = words.last().is_some_and(|w| w == "song" || w == "songs");
    let liked = words
        .iter()
        .any(|w| ["like", "liked", "likes", "light", "lights", "life", "life's", "lifes"].contains(&w.as_str()));
    if songs && liked && words.len() <= 5 && intent::parse(request) == Intent::Unknown {
        return Some("play my liked songs".into());
    }
    let music = words.last().is_some_and(|w| ["music", "musikk", "musikken"].contains(&w.as_str()));
    (music && words.len() <= 4 && intent::parse(request) == Intent::Unknown)
        .then(|| if playing { "pause the music" } else { "play some music" }.into())
}

/// A draft that ends in "and" ("pause the music and ...") is not a whole request, whatever it parses as.
pub(super) fn goes_on(request: &str) -> bool {
    let last = request.split_whitespace().last().map(voice_assistant::dialog::normalize);
    last.is_some_and(|w| w == "and" || w == "og")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn misheard_music_requests() {
        assert_eq!(music_by_sound("Nice music.", true), Some("pause the music".into()));
        assert_eq!(music_by_sound("Please song music.", false), Some("play some music".into()));
        assert_eq!(music_by_sound("Tyson music.", true), Some("pause the music".into()));
        assert_eq!(music_by_sound("Pause the music.", false), None);
        assert_eq!(music_by_sound("I really like this kind of music", true), None);
        assert_eq!(music_by_sound("Nice weather.", true), None);
        assert_eq!(music_by_sound("They're my like songs.", false), Some("play my liked songs".into()));
        assert_eq!(music_by_sound("claim a light song.", false), Some("play my liked songs".into()));
        assert_eq!(music_by_sound("Clean my life's songs.", true), Some("play my liked songs".into()));
        assert_eq!(music_by_sound("I like songs about the sea and the long summer nights", false), None);
        assert_eq!(music_by_sound("Stop.", true), Some("pause the music".into()));
        assert_eq!(music_by_sound("stop it", true), Some("pause the music".into()));
        assert_eq!(music_by_sound("Stop.", false), None);
        assert_eq!(music_by_sound("Put the percentage at 70 percent.", true), Some("set the volume to 70".into()));
        assert_eq!(music_by_sound("Trop with the sign at 50 percent.", true), Some("set the volume to 50".into()));
        assert_eq!(music_by_sound("Put the percentage at 70 percent.", false), None);
    }

    #[test]
    fn goes_on_after_and() {
        assert!(goes_on("pause the music and"));
        assert!(goes_on("stopp musikken og"));
        assert!(!goes_on("pause the music"));
        assert!(!goes_on(""));
    }
}
