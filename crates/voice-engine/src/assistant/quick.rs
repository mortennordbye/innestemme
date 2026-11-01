//! Requests that are acted on at once or without words, and music requests misheard.

use voice_assistant::ha;
use voice_assistant::house::{self, HouseCommand};
use voice_assistant::intent::{self, Intent, LightLevel, MusicCommand};
use voice_assistant::timer::TimerCommand;
use voice_assistant::tv::TvCommand;

/// "Stop", "stop it": never mind, or the music when it plays.
pub(super) fn bare_stop(request: &str) -> bool {
    let words: Vec<String> =
        request.split_whitespace().map(voice_assistant::dialog::normalize).filter(|w| !w.is_empty()).collect();
    matches!(words.as_slice(), [w] | [w, _] if ["stop", "stopp"].contains(&w.as_str()))
        && intent::parse(request) == Intent::Cancel
}

/// Whisper hears "pause" and "play" after the wake word as "please", "nice", "post": a short request
/// that ends in "music" and is nothing else means pausing it when it was playing, else playing it.
/// A prompt with the command words fixes some of these but makes up commands from room noise.
pub(super) fn music_by_sound(request: &str, playing: bool) -> Option<String> {
    let words: Vec<String> =
        request.split_whitespace().map(voice_assistant::dialog::normalize).filter(|w| !w.is_empty()).collect();
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

/// An action whose result is heard (the music stops, the next song starts) needs no words, which
/// would only arrive a second or two later from the speaker.
pub(super) fn heard_by_itself(intent: &Intent) -> bool {
    matches!(
        intent,
        Intent::Music(
            MusicCommand::Pause
                | MusicCommand::Resume
                | MusicCommand::Stop
                | MusicCommand::Mute(_)
                | MusicCommand::Next
                | MusicCommand::Previous
                | MusicCommand::Liked
                | MusicCommand::Playlist(_)
                | MusicCommand::Song(_)
                | MusicCommand::Artist(_)
                | MusicCommand::Album(_)
        ) | Intent::Tv(TvCommand::Pause | TvCommand::Resume)
    )
}

/// Commands that nothing said after them could change, so a draft that is one is acted on. Not
/// resume: "play" is also the start of "play Careless Whisper"; not louder or quieter: "increase
/// the sound" may go on "by 25%"; not the date: "what day is it" may go on "tomorrow".
pub(super) fn complete_at_once(intent: &Intent) -> bool {
    match intent {
        // "Turn off the kitchen lights", not "turn off the lights" (may go on "in the kitchen").
        Intent::Lights { target, .. } => names_light_last(target),
        // A level or colour ends the request; "dim the kitchen" may go on "to 30 percent".
        Intent::LightLevel { level: LightLevel::Percent(_) | LightLevel::Color(_), target } => {
            ha::names_something(target)
        }
        // Small talk is a whole short phrase; a bare "hey" is often a request still to come.
        Intent::SmallTalk(chat) => *chat != voice_assistant::smalltalk::Chat::Hello,
        _ => complete_command(intent),
    }
}

/// Whether a light request ends on what it switches: "the kitchen lights", "the kitchen".
fn names_light_last(target: &str) -> bool {
    let words: Vec<String> = target.split_whitespace().map(voice_assistant::dialog::normalize).collect();
    let last = words.last().map(String::as_str).unwrap_or_default();
    ha::names_something(target) && (["light", "lights", "lamp", "lamps"].contains(&last) || ha::names_only_room(&words))
}

fn complete_command(intent: &Intent) -> bool {
    matches!(
        intent,
        Intent::Cancel
            | Intent::Music(
                MusicCommand::Pause
                    | MusicCommand::Stop
                    | MusicCommand::Mute(_)
                    | MusicCommand::NowPlaying
                    | MusicCommand::Shuffle(_)
                    | MusicCommand::Repeat(_)
                    | MusicCommand::Next
                    | MusicCommand::Previous
                    | MusicCommand::VolumeBy(_)
                    | MusicCommand::VolumeTo(_)
            )
            | Intent::Tv(
                TvCommand::Pause
                    | TvCommand::Resume
                    | TvCommand::Power(_)
                    | TvCommand::Mute(_)
                    | TvCommand::Louder
                    | TvCommand::Quieter
            )
            | Intent::Tv(TvCommand::Show(_))
            | Intent::House(HouseCommand::FindPhone(_) | HouseCommand::Climate(house::Indoors::Room(_), _))
            | Intent::Routine(_)
            | Intent::Party(_)
            | Intent::Time
            | Intent::Thanks
            | Intent::Joke
            // A length is the end of the request: "set a timer for 5 minutes", "stop the timer".
            | Intent::Timer(
                TimerCommand::Start { seconds: Some(_), .. }
                    | TimerCommand::Cancel(_)
                    | TimerCommand::Pause(_)
                    | TimerCommand::Resume(_)
                    | TimerCommand::Remaining(_)
                    | TimerCommand::Snooze
                    | TimerCommand::StopAlarm
            )
    )
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
    fn quick_commands() {
        let parse = |text: &str| intent::parse(text);
        assert!(heard_by_itself(&parse("pause the music")));
        assert!(heard_by_itself(&parse("next song")));
        assert!(!heard_by_itself(&parse("turn it up")));
        assert!(heard_by_itself(&parse("play some music")));
        assert!(complete_at_once(&parse("pause the music")));
        assert!(complete_at_once(&parse("what time is it")));
        assert!(!complete_at_once(&parse("what day is it")));
        assert!(!complete_at_once(&parse("turn off the lights")));
        assert!(!complete_at_once(&parse("play")));
        assert!(complete_at_once(&parse("turn off the kitchen lights")));
        assert!(complete_at_once(&parse("turn on the kitchen")));
        assert!(!complete_at_once(&parse("turn on the lights in")));
        assert!(complete_at_once(&parse("set the living room to 30 percent")));
        assert!(!complete_at_once(&parse("dim the living room")));
        assert!(complete_at_once(&parse("put the weather on the tv")));
        assert!(complete_at_once(&parse("where is my phone")));
        assert!(complete_at_once(&parse("what's the temperature in the bedroom")));
        assert!(!complete_at_once(&parse("what's the temperature")));
        assert!(complete_at_once(&parse("good night")));
        assert!(complete_at_once(&parse("initiate lockdown")));
        assert!(complete_at_once(&parse("how you doing")));
        assert!(!complete_at_once(&parse("hey")));
    }
}
