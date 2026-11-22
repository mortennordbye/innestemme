//! Music requests: play, pause, volume, rooms and the queue. Asked after the timers, the
//! shopping list and the house, since song titles hold every kind of word.

use super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MusicCommand {
    /// The user's Liked Songs, shuffled.
    Liked,
    /// One of the user's playlists, by name.
    Playlist(String),
    /// A song search: "careless whisper", "hello by adele".
    Song(String),
    Resume,
    Pause,
    Next,
    Previous,
    Louder,
    Quieter,
    /// "Increase the sound by 25%": percentage points up (or down when negative).
    VolumeBy(i8),
    /// "Set the volume to 40 percent".
    VolumeTo(u8),
    /// "Stop the music": ends it, where a bare "stop" pauses.
    Stop,
    /// Mute (true) or unmute the speaker, leaving the music playing.
    Mute(bool),
    /// "What's playing?"
    NowPlaying,
    Shuffle(bool),
    Repeat(Repeat),
    /// "Play music by Ghost", "play the artist Ghost".
    Artist(String),
    /// "Play the album Rumours".
    Album(String),
    /// "Play jazz in the kitchen", "pause the music in the bedroom": a command for the speaker in a
    /// room, named as said ("kitchen", "badet").
    InRoom(String, Box<MusicCommand>),
    /// "Play jazz in the kitchen and the bedroom": rooms as said; it plays in the first.
    InRooms(Vec<String>, Box<MusicCommand>),
    /// "Play Hello after this song" (true), "add Rumours to the queue" (false): a play request queued
    /// after the song playing, or at the end, instead of replacing the music. A bare `Resume` inside
    /// is "queue up" with nothing named; `NowPlaying` is "queue this song".
    Queue(Box<MusicCommand>, bool),
    /// "Move the music (in the kitchen) to the bedroom": the room it moves to, and the room it
    /// comes from when said.
    MoveTo(String, Option<String>),
    /// "What's next?": the song after this one in the queue.
    UpNext,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Repeat {
    Off,
    All,
    /// The current song.
    One,
}

pub(super) const PLAY_WORDS: &[&str] = &["play", "spill"];
/// Polite words before "play": "can you please play", "kan du spille".
pub(super) const POLITE: &[&str] = &[
    "can", "could", "would", "will", "you", "please", "just", "now", "ok", "okay", "kan", "du", "vil", "vær", "så",
    "snill",
];
/// Replies that send the assistant back to sleep.
pub(super) const NOT_FOR_YOU: &[&str] = &[
    "no",
    "nope",
    "not you",
    "not now",
    "go back to sleep",
    "back to sleep",
    "false alarm",
    "wrong",
    "nothing",
    "ignore that",
    "ignore me",
    // Said over an answer: stop, not a remark to answer.
    "shut up",
    "be quiet",
    "quiet",
    "hold kjeft",
    "vær stille",
    "nei",
    "ikke deg",
    "ikke nå",
    "ingenting",
];
const MUSIC_WORDS: &[&str] =
    &["music", "musikk", "musikken", "song", "songs", "sang", "sangen", "låt", "låten", "track", "spotify"];
/// Every word of a Liked Songs request comes from here ("my liked songs", "de likte sangene mine"),
/// so a song title that contains "like" is still a song.
const LIKED_WORDS: &[&str] = &[
    "liked",
    "like",
    "likes",
    "favorite",
    "favorites",
    "favourite",
    "favourites",
    "likte",
    "favoritt",
    "favoritter",
    "favorittsanger",
    "favorittsangene",
];
const LIKED_HEARD_AS: &[&str] = &["light", "lights", "life", "life's", "lifes"];
const LIKED_FILLER: &[&str] = &[
    "my",
    "the",
    "songs",
    "song",
    "playlist",
    "music",
    "tracks",
    "list",
    "some",
    "of",
    "from",
    "mine",
    "min",
    "mi",
    "de",
    "sangene",
    "sanger",
    "låtene",
    "låter",
    "musikk",
    "musikken",
    "spillelisten",
    "spilleliste",
    "spillelista",
    "liste",
    "lista",
    "fra",
];
const PLAYLIST_WORDS: &[&str] = &["playlist", "spilleliste", "spillelisten", "spillelista"];
const PLAYLIST_FILLER: &[&str] = &["my", "the", "called", "named", "min", "mi", "mine", "som", "heter", "den", "det"];
/// Dropped from the end of a play request: "... please", "... on the sonos".
const TAIL: &[&str] = &[
    "please",
    "takk",
    "now",
    "nå",
    "for",
    "me",
    "meg",
    "on",
    "the",
    "sonos",
    "speaker",
    "spotify",
    "på",
    "høyttaleren",
];
const GENERIC: &[&str] = &["music", "some", "musikk", "noe", "the", "litt", "something", "a", "song", "songs"];

/// Words a room follows in a music request: "in the kitchen", "on the bedroom speaker", "på badet".
const ROOM_MARKERS: &[&str] = &["in", "on", "i", "på"];
/// Words a room is moved to, and from: "move the music in the kitchen to the bedroom".
const TO_MARKERS: &[&str] = &["to", "into", "til"];
const FROM_MARKERS: &[&str] = &["in", "from", "i", "på", "fra"];
const SPEAKER_WORDS: &[&str] = &["speaker", "speakers", "sonos", "høyttaler", "høyttaleren"];
/// Words that may follow a room in a request. Anything else makes it part of a title: "play In the
/// Kitchen Tonight".
const AFTER_ROOM: &[&str] =
    &["and", "og", "too", "please", "takk", "now", "nå", "then", "da", "after", "etter", "next", "neste", "to", "til"];
/// The ends of "add Rumours to the queue", "legg Kygo i køen".
const QUEUE_ENDS: &[&[&str]] = &[
    &["to", "the", "queue"],
    &["on", "the", "queue"],
    &["in", "the", "queue"],
    &["to", "my", "queue"],
    &["to", "queue"],
    &["i", "køen"],
    &["til", "køen"],
    &["på", "køen"],
    &["i", "kø"],
];
/// The ends of a play request that queue it after the song playing. A bare "next" or "after this"
/// is not one: "Thank U, Next", "Life After This" are titles.
const NEXT_ENDS: &[&[&str]] = &[
    &["next", "in", "the", "queue"],
    &["next", "in", "queue"],
    &["after", "this", "song"],
    &["after", "this", "track"],
    &["after", "the", "current", "song"],
    &["after", "the", "current", "track"],
    &["etter", "denne", "sangen"],
    &["etter", "denne", "låten"],
    &["neste", "i", "køen"],
];
/// Said after a request, they change nothing: "... please".
const POLITE_END: &[&str] = &["please", "takk", "now", "nå"];

/// A room after `at`, a marker word ("in the kitchen", "på badet"): the room as said and the index
/// after it, a trailing "speaker" included. It ends the request or comes before a word that may
/// follow a room.
fn room_after(words: &[String], at: usize) -> Option<(String, usize)> {
    let mut start = at + 1;
    if words.get(start).is_some_and(|w| w == "the") {
        start += 1;
    }
    let (room, mut end) = [2, 1].into_iter().find_map(|len| {
        let room = words.get(start..start + len)?.join(" ");
        crate::ha::is_room_phrase(&room).then_some((room, start + len))
    })?;
    if words.get(end).is_some_and(|w| SPEAKER_WORDS.contains(&w.as_str())) {
        end += 1;
    }
    words.get(end).is_none_or(|w| AFTER_ROOM.contains(&w.as_str())).then_some((room, end))
}

/// The rooms said in a music request, and the words without them: "play jazz in the kitchen" is the
/// kitchen and "play jazz"; "in the kitchen and the bedroom" names two. Only the rooms people name
/// (`ha::ROOMS`), so "Dancing in the Dark" keeps its title.
fn music_rooms(words: &[String]) -> Option<(Vec<String>, Vec<String>)> {
    // From the front, so "in the kitchen and in the bedroom" starts at the first room.
    (0..words.len()).filter(|&at| ROOM_MARKERS.contains(&words[at].as_str())).find_map(|at| {
        let (room, mut end) = room_after(words, at)?;
        let mut rooms = vec![room];
        while words.get(end).is_some_and(|w| w == "and" || w == "og") {
            // "and the bedroom", "and in the bedroom", "og på soverommet".
            let marker =
                if words.get(end + 1).is_some_and(|w| ROOM_MARKERS.contains(&w.as_str())) { end + 1 } else { end };
            let Some((more, after)) = room_after(words, marker) else { break };
            rooms.push(more);
            end = after;
        }
        // A word left alone after the room says nothing: "play music in the kitchen too".
        if words[end..].iter().all(|w| LONE_AFTER_ROOM.contains(&w.as_str())) {
            end = words.len();
        }
        Some((rooms, [&words[..at], &words[end..]].concat()))
    })
}

/// Words that, alone after a room, end the request: "... in the kitchen too", "... på kjøkkenet da".
const LONE_AFTER_ROOM: &[&str] =
    &["too", "then", "da", "and", "og", "next", "after", "to", "etter", "please", "takk", "now", "nå"];

/// "Queue this song", "add it to the queue": the song playing.
const THIS_SONG: &[&[&str]] = &[
    &["it"],
    &["this"],
    &["that"],
    &["this", "song"],
    &["that", "song"],
    &["this", "one"],
    &["this", "track"],
    &["the", "current", "song"],
    &["den"],
    &["denne"],
    &["denne", "sangen"],
    &["den", "sangen"],
    &["denne", "låten"],
];

/// "Move the music to the bedroom", "move the music in the kitchen to the bedroom", "flytt
/// musikken til soverommet": where it goes, and where it comes from when said.
fn move_music(words: &[String]) -> Option<MusicCommand> {
    let verb = words.iter().position(|w| ["move", "transfer", "flytt", "send"].contains(&w.as_str()))?;
    if !words[verb..].iter().any(|w| MUSIC_WORDS.contains(&w.as_str())) {
        return None;
    }
    let (to_at, to) = (verb + 1..words.len())
        .filter(|&at| TO_MARKERS.contains(&words[at].as_str()))
        .find_map(|at| Some((at, room_after(words, at)?.0)))?;
    let from = (verb + 1..to_at)
        .filter(|&at| FROM_MARKERS.contains(&words[at].as_str()))
        .find_map(|at| room_after(words, at))
        .map(|(room, _)| room);
    Some(MusicCommand::MoveTo(to, from))
}

/// A request to queue music, as whether it goes next (true) or last, and the play request it
/// holds: "add Rumours to the queue" is "play rumours", "play Hello after this song" is "play
/// hello". "Queue up" alone holds a bare "play".
fn queue_cue(words: &[String]) -> Option<(bool, Vec<String>)> {
    let start = words.iter().position(|w| !POLITE.contains(&w.as_str()))?;
    let mut end = words.len();
    while end > start + 1 && POLITE_END.contains(&words[end - 1].as_str()) {
        end -= 1;
    }
    let w: Vec<&str> = words[start..end].iter().map(String::as_str).collect();
    let request = |play: &str, item: &[&str]| -> Vec<String> {
        std::iter::once(play).chain(item.iter().copied()).map(str::to_owned).collect()
    };
    let without = |rest: &[&str], ends: &[&[&str]]| -> Option<usize> {
        ends.iter().find(|e| rest.len() >= e.len() && rest.ends_with(e)).map(|e| rest.len() - e.len())
    };
    match w.as_slice() {
        // "Queue Hello next": after the song playing.
        ["queue", "up", rest @ .., "next"] | ["queue", rest @ .., "next"] if !rest.is_empty() => {
            Some((true, request("play", rest)))
        }
        ["queue", "up", rest @ ..] | ["queue", rest @ ..] => Some((false, request("play", rest))),
        ["add" | "put", rest @ ..] => without(rest, QUEUE_ENDS).map(|n| (false, request("play", &rest[..n]))),
        ["legg" | "sett", rest @ ..] => {
            let rest = rest.strip_prefix(&["til"]).unwrap_or(rest);
            without(rest, QUEUE_ENDS).map(|n| (false, request("spill", &rest[..n])))
        }
        [play, rest @ ..] if PLAY_WORDS.contains(play) => {
            without(rest, NEXT_ENDS).filter(|&n| n > 0).map(|n| (true, request(play, &rest[..n])))
        }
        _ => None,
    }
}

/// A command that starts music, which can be queued.
fn playable(command: &MusicCommand) -> bool {
    matches!(
        command,
        MusicCommand::Liked
            | MusicCommand::Playlist(_)
            | MusicCommand::Song(_)
            | MusicCommand::Artist(_)
            | MusicCommand::Album(_)
    )
}

/// The rooms and the queueing come off first, once, and the rest is parsed once: parsing what is
/// left again for each would take exponential time on a long sentence.
pub(super) fn music(words: &[String]) -> Option<MusicCommand> {
    if let Some(command) = move_music(words) {
        return Some(command);
    }
    let (rooms, rest) = music_rooms(words).unwrap_or_else(|| (Vec::new(), words.to_vec()));
    let queue = queue_cue(&rest);
    let request = queue.as_ref().map_or(rest.as_slice(), |(_, request)| request.as_slice());
    // "Queue this song": the song playing, again.
    let this_song = queue.is_some() && request.len() > 1 && THIS_SONG.iter().any(|t| request[1..] == **t);
    let command = if this_song { Some(MusicCommand::NowPlaying) } else { music_core(request) };
    let command = command.and_then(|command| match &queue {
        Some((next, _)) if command == MusicCommand::NowPlaying => Some(MusicCommand::Queue(Box::new(command), *next)),
        None => Some(command),
        Some((next, _)) if playable(&command) => Some(MusicCommand::Queue(Box::new(command), *next)),
        // "Queue up" with nothing to queue: the answer asks what.
        Some((next, _)) if command == MusicCommand::Resume => Some(MusicCommand::Queue(Box::new(command), *next)),
        Some(_) => None,
    });
    // "Play In the Kitchen": a play request that is only a room is a title.
    let only_play = rest.iter().filter(|w| !POLITE.contains(&w.as_str())).count() == 1
        && rest.iter().any(|w| PLAY_WORDS.contains(&w.as_str()));
    match (command, rooms.len()) {
        (command, 0) => command,
        (Some(command), 1) if !only_play => Some(MusicCommand::InRoom(rooms[0].clone(), Box::new(command))),
        (Some(command), _) if !only_play => Some(MusicCommand::InRooms(rooms, Box::new(command))),
        // Without the rooms it is not music: the whole sentence, read as it was.
        _ => music_core(words),
    }
}

/// Kinds of music said before "music" with no verb: "soft music" in the follow-up window (20 Nov)
/// was dropped. Only these, so a sentence that ends in "music" is not a request.
const STYLES: &[&str] = &[
    "soft",
    "calm",
    "relaxing",
    "relaxed",
    "quiet",
    "chill",
    "gentle",
    "mellow",
    "peaceful",
    "ambient",
    "background",
    "jazz",
    "jazzy",
    "classical",
    "piano",
    "acoustic",
    "romantic",
    "dinner",
    "happy",
    "upbeat",
    "christmas",
    "rolig",
    "stille",
];

/// The whole request is a kind of music: "soft music", "some jazz music", "relaxing music".
fn bare_style(words: &[String]) -> bool {
    let mut words: Vec<&str> = words.iter().map(String::as_str).collect();
    while words.last().is_some_and(|w| TAIL.contains(w)) {
        words.pop();
    }
    let Some((last, before)) = words.split_last() else { return false };
    let before = before.strip_prefix(&["some"]).or_else(|| before.strip_prefix(&["a", "little"])).unwrap_or(before);
    ["music", "musikk"].contains(last)
        && !before.is_empty()
        && before.len() <= 3
        && before.iter().all(|w| STYLES.contains(w) || ["and", "nice", "litt", "og"].contains(w))
        && before.iter().any(|w| STYLES.contains(w))
}

/// A music request with no room or queueing in it.
fn music_core(words: &[String]) -> Option<MusicCommand> {
    let has = |w: &str| words.iter().any(|x| x == w);
    let start = words.iter().position(|w| !POLITE.contains(&w.as_str()))?;
    if PLAY_WORDS.contains(&words[start].as_str()) {
        let mut rest: Vec<&str> = words[start + 1..].iter().map(String::as_str).collect();
        // "spill av" is "play" in Norwegian.
        if words[start] == "spill" && rest.first() == Some(&"av") {
            rest.remove(0);
        }
        while rest.last().is_some_and(|w| TAIL.contains(w)) {
            rest.pop();
        }
        if rest.is_empty() {
            return Some(MusicCommand::Resume);
        }
        if let Some(album) = after_any(&rest, &[&["the", "album"], &["album"], &["albumet"]]) {
            return Some(MusicCommand::Album(album));
        }
        let by_artist: &[&[&str]] = &[
            &["the", "artist"],
            &["artist"],
            &["artisten"],
            &["music", "by"],
            &["songs", "by"],
            &["something", "by"],
            &["some", "music", "by"],
            &["some", "songs", "by"],
            &["musikk", "av"],
            &["noe", "av"],
            &["litt", "musikk", "av"],
        ];
        if let Some(artist) = after_any(&rest, by_artist) {
            return Some(MusicCommand::Artist(artist));
        }
        if rest.iter().all(|w| GENERIC.contains(w)) {
            return Some(MusicCommand::Liked);
        }
        if rest.iter().any(|w| LIKED_WORDS.contains(w))
            && rest.iter().all(|w| LIKED_WORDS.contains(w) || LIKED_FILLER.contains(w))
        {
            return Some(MusicCommand::Liked);
        }
        // Whisper's "liked": "play my light songs", "play my life's songs". Only with "songs", so
        // "play Light" stays a song.
        let songs = rest.iter().any(|w| ["song", "songs"].contains(w));
        if songs
            && rest.iter().any(|w| LIKED_HEARD_AS.contains(w))
            && rest.iter().all(|w| LIKED_HEARD_AS.contains(w) || LIKED_FILLER.contains(w))
        {
            return Some(MusicCommand::Liked);
        }
        if rest.iter().any(|w| PLAYLIST_WORDS.contains(w)) {
            let name: Vec<&str> =
                rest.iter().copied().filter(|w| !PLAYLIST_WORDS.contains(w) && !PLAYLIST_FILLER.contains(w)).collect();
            return Some(if name.is_empty() { MusicCommand::Liked } else { MusicCommand::Playlist(name.join(" ")) });
        }
        return Some(MusicCommand::Song(rest.join(" ")));
    }
    if bare_style(words) {
        let play: Vec<String> = std::iter::once("play".to_owned()).chain(words.iter().cloned()).collect();
        return music_core(&play);
    }
    if words.iter().any(|w| LIGHT_WORDS.contains(&w.as_str())) {
        return None;
    }
    let about_music = words.iter().any(|w| MUSIC_WORDS.contains(&w.as_str()));
    // "What's next", "what song is next", "hva er neste sang": before "what song is this" below,
    // and not "what's next week".
    let asks = ["what", "what's", "whats", "which", "hva", "hvilken"].iter().any(|w| has(w));
    let after = has("next") || has("neste") || has("etterpå");
    let of_when = words.iter().any(|w| WHEN_WORDS.contains(&w.trim_end_matches("'s")));
    if asks && after && !of_when && (about_music || has("playing") || has("kommer") || words.len() <= 3) {
        return Some(MusicCommand::UpNext);
    }
    let now_playing = (has("what") || has("what's") || has("whats") || has("who") || has("hva"))
        && (has("playing")
            || has("song")
            || has("sings")
            || has("singing")
            || has("artist")
            || has("spilles")
            || has("sangen"));
    if now_playing && !has("play") {
        return Some(MusicCommand::NowPlaying);
    }
    let off = ["off", "av", "stop", "stopp", "disable"].iter().any(|w| has(w));
    if has("shuffle") || has("shuffling") || has("tilfeldig") {
        return Some(MusicCommand::Shuffle(!off));
    }
    if has("repeat") || has("repeating") || has("gjenta") {
        let one = ["this", "song", "track", "one", "denne", "sangen"].iter().any(|w| has(w));
        return Some(MusicCommand::Repeat(match (off, one) {
            (true, _) => Repeat::Off,
            (false, true) => Repeat::One,
            (false, false) => Repeat::All,
        }));
    }
    if has("unmute") || (has("demp") && has("ikke")) {
        return Some(MusicCommand::Mute(false));
    }
    if (has("mute") || has("demp")) && (about_music || words.len() <= 2) {
        return Some(MusicCommand::Mute(true));
    }
    if about_music && (has("stop") || has("stopp")) {
        return Some(MusicCommand::Stop);
    }
    if has("pause") || (about_music && ["off", "av"].iter().any(|w| has(w))) {
        return Some(MusicCommand::Pause);
    }
    if has("resume") || has("unpause") || (about_music && ["continue", "fortsett", "start"].iter().any(|w| has(w))) {
        return Some(MusicCommand::Resume);
    }
    // "Neste" is also "neste helg", "neste buss": only music when said of music or on its own, and
    // never of a day or a week ("og neste helg", "and next weekend" follow a weather answer).
    const WHEN_WORDS: &[&str] = &[
        "weekend",
        "week",
        "day",
        "month",
        "year",
        "monday",
        "tuesday",
        "wednesday",
        "thursday",
        "friday",
        "saturday",
        "sunday",
        "game",
        "match",
        "helg",
        "helgen",
        "uke",
        "uka",
        "dag",
        "dagen",
        "måned",
        "år",
        "mandag",
        "tirsdag",
        "onsdag",
        "torsdag",
        "fredag",
        "lørdag",
        "søndag",
        "kamp",
        "kampen",
    ];
    let about_when = words.iter().any(|w| WHEN_WORDS.contains(&w.trim_end_matches("'s")));
    // "Kan du hoppe til neste", "ta neste": a skip verb with it is music too.
    let skip_verb = ["hopp", "hoppe", "ta", "gå", "go", "skip"].iter().any(|w| has(w));
    let next = has("next") || has("neste");
    if has("skip") || (next && !about_when && (about_music || skip_verb || words.len() <= 3)) {
        return Some(MusicCommand::Next);
    }
    if has("previous") || has("forrige") {
        return Some(MusicCommand::Previous);
    }
    const VOLUME: &[&str] = &["volume", "volumet", "sound", "lyd", "lyden", "turn", "skru", "it", "music", "musikken"];
    // "Lower the brightness", "turn the lights down": the lights, unless the sound is named too.
    let sound = ["volume", "volumet", "sound", "lyd", "lyden", "music", "musikk", "musikken"].iter().any(|w| has(w));
    let lights = ["brightness", "lysstyrke", "lysstyrken"].iter().any(|w| has(w))
        || words.iter().any(|w| LIGHT_WORDS.contains(&w.as_str()));
    if lights && !sound {
        return None;
    }
    let volume = words.iter().any(|w| VOLUME.contains(&w.as_str()));
    // "Up" and "down" only after a volume word ("turn it up", "skru ned lyden"): in "show the bus
    // times ... up it" the "it" comes too late to be turned up.
    let particle = |list: &[&str]| {
        let first_volume = words.iter().position(|w| VOLUME.contains(&w.as_str()));
        words.iter().enumerate().any(|(i, w)| list.contains(&w.as_str()) && first_volume.is_some_and(|v| v < i))
    };
    let up =
        ["increase", "increased", "raise", "raised", "øk", "øke"].iter().any(|w| has(w)) || particle(&["up", "opp"]);
    let down = ["decrease", "decreased", "reduce", "reduced", "lowered", "senk", "senke"].iter().any(|w| has(w))
        || particle(&["down", "ned"]);
    let louder = has("louder") || has("høyere") || (volume && up);
    let quieter = ["quieter", "softer", "lower", "lavere"].iter().any(|w| has(w)) || (volume && down);
    if let Some((to, amount)) = volume_amount(words).filter(|_| volume || louder || quieter) {
        return Some(match (to, louder, quieter) {
            (true, _, _) => MusicCommand::VolumeTo(amount),
            (false, true, false) => MusicCommand::VolumeBy(amount as i8),
            (false, false, true) => MusicCommand::VolumeBy(-(amount as i8)),
            // "Volume 30", "music 30 percent": no direction is a level.
            (false, false, false) => MusicCommand::VolumeTo(amount),
            _ => return None,
        });
    }
    if louder {
        return Some(MusicCommand::Louder);
    }
    if quieter {
        return Some(MusicCommand::Quieter);
    }
    None
}

/// What follows one of these openings of a play request: "the album Rumours" gives "rumours".
fn after_any(rest: &[&str], openings: &[&[&str]]) -> Option<String> {
    openings.iter().find(|o| rest.len() > o.len() && rest.starts_with(o)).map(|o| rest[o.len()..].join(" "))
}

/// A percentage in a volume request, and whether it is the level to set ("to 40") rather than a
/// change ("by 25%", "20 percent up"). A transcriber writes "25%", which normalises to "25".
fn volume_amount(words: &[String]) -> Option<(bool, u8)> {
    let at = words.iter().position(|w| w.parse::<u8>().is_ok_and(|n| (1..=100).contains(&n)))?;
    let amount = words[at].parse().ok()?;
    let to = at > 0 && ["to", "til", "på", "at"].contains(&words[at - 1].as_str());
    Some((to, amount))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn up_and_down_follow_a_volume_word() {
        let music = |text: &str| match parse(text) {
            Intent::Music(command) => Some(command),
            _ => None,
        };
        assert_eq!(music("turn it up"), Some(MusicCommand::Louder));
        assert_eq!(music("turn up the volume"), Some(MusicCommand::Louder));
        assert_eq!(music("skru ned lyden"), Some(MusicCommand::Quieter));
        assert_eq!(music("release up it"), None);
    }

    #[test]
    fn music_requests() {
        use MusicCommand::*;
        let music = |text: &str| match parse(text) {
            Intent::Music(command) => Some(command),
            _ => None,
        };
        let song = |s: &str| Some(Song(s.into()));
        assert_eq!(music("Play my liked songs."), Some(Liked));
        assert_eq!(music("Play my light songs."), Some(Liked));
        assert_eq!(music("play my life's songs"), Some(Liked));
        assert_eq!(music("play Light My Love"), song("light my love"));
        assert_eq!(music("play my like playlist"), Some(Liked));
        assert_eq!(music("Can you play my favorite songs please"), Some(Liked));
        assert_eq!(music("play some music"), Some(Liked));
        assert_eq!(music("play a song"), Some(Liked));
        assert_eq!(music("spill de likte sangene mine"), Some(Liked));
        assert_eq!(music("play Careless Whisper"), song("careless whisper"));
        assert_eq!(music("Play Hello by Adele on the Sonos."), song("hello by adele"));
        assert_eq!(music("play Here Comes the Rain Again"), song("here comes the rain again"));
        assert_eq!(music("play I Like It"), song("i like it"));
        assert_eq!(music("spill av Kygo"), song("kygo"));
        assert_eq!(music("play my running playlist"), Some(Playlist("running".into())));
        assert_eq!(music("play the playlist called Chill Vibes"), Some(Playlist("chill vibes".into())));
        assert_eq!(music("play"), Some(Resume));
        assert_eq!(music("pause"), Some(Pause));
        assert_eq!(music("stop the music"), Some(Stop));
        assert_eq!(music("turn off the music"), Some(Pause));
        assert_eq!(music("stopp musikken"), Some(Stop));
        assert_eq!(music("mute the music"), Some(Mute(true)));
        assert_eq!(music("mute"), Some(Mute(true)));
        assert_eq!(music("unmute the music"), Some(Mute(false)));
        assert_eq!(music("what's playing"), Some(NowPlaying));
        assert_eq!(music("What song is this?"), Some(NowPlaying));
        assert_eq!(music("who sings this"), Some(NowPlaying));
        assert_eq!(music("turn on shuffle"), Some(Shuffle(true)));
        assert_eq!(music("stop shuffling"), Some(Shuffle(false)));
        assert_eq!(music("repeat this song"), Some(Repeat(super::Repeat::One)));
        assert_eq!(music("turn on repeat"), Some(Repeat(super::Repeat::All)));
        assert_eq!(music("turn off repeat"), Some(Repeat(super::Repeat::Off)));
        assert_eq!(music("play the album Rumours"), Some(Album("rumours".into())));
        assert_eq!(music("play music by Ghost"), Some(Artist("ghost".into())));
        assert_eq!(music("play some songs by Fleetwood Mac"), Some(Artist("fleetwood mac".into())));
        assert_eq!(music("play the artist Kygo"), Some(Artist("kygo".into())));
        assert_eq!(music("continue the music"), Some(Resume));
        assert_eq!(music("next song"), Some(Next));
        assert_eq!(music("skip this one"), Some(Next));
        assert_eq!(music("neste sang"), Some(Next));
        assert_eq!(music("previous song"), Some(Previous));
        assert_eq!(music("turn it up"), Some(Louder));
        assert_eq!(music("louder"), Some(Louder));
        assert_eq!(music("turn the music down"), Some(Quieter));
        assert_eq!(music("skru ned musikken"), Some(Quieter));
        assert_eq!(music("increase the sound"), Some(Louder));
        assert_eq!(music("Jarvis increased the sound"), Some(Louder));
        assert_eq!(music("lower the volume"), Some(Quieter));
        assert_eq!(music("Increase the sound by 25%."), Some(VolumeBy(25)));
        assert_eq!(music("turn it down 10 percent"), Some(VolumeBy(-10)));
        assert_eq!(music("set the volume to 40%"), Some(VolumeTo(40)));
        assert_eq!(music("put the music at 30%."), Some(VolumeTo(30)));
        assert_eq!(music("volume 25"), Some(VolumeTo(25)));
        assert_eq!(music("skru opp lyden med 20 prosent"), Some(VolumeBy(20)));
        // A kind of music said alone is a play request.
        assert_eq!(music("Soft music."), song("soft music"));
        assert_eq!(music("calm music"), song("calm music"));
        assert_eq!(music("some jazz music"), music("play some jazz music"));
        assert_eq!(music("relaxing music please"), song("relaxing music"));
        assert_eq!(music("I like soft music"), None);
        assert_eq!(music("music"), None);
        // Not music.
        assert_eq!(music("turn off the light in my living room"), None);
        assert_eq!(music("turn up the lights"), None);
        assert_eq!(music("stop"), None);
        assert_eq!(music("what's the weather tomorrow"), None);
    }

    #[test]
    fn music_in_rooms_and_queued() {
        use MusicCommand::*;
        let music = |text: &str| match parse(text) {
            Intent::Music(command) => Some(command),
            _ => None,
        };
        let room = |room: &str, command: MusicCommand| Some(InRoom(room.into(), Box::new(command)));
        let song = |s: &str| Song(s.into());
        assert_eq!(music("play jazz in the kitchen"), room("kitchen", song("jazz")));
        assert_eq!(music("Play Hello by Adele in the living room."), room("living room", song("hello by adele")));
        assert_eq!(music("play some music in the bedroom"), room("bedroom", Liked));
        assert_eq!(music("play my liked songs on the kitchen speaker"), room("kitchen", Liked));
        assert_eq!(music("spill musikk på badet"), room("badet", Liked));
        assert_eq!(music("spill Kygo på kjøkkenet"), room("kjøkkenet", song("kygo")));
        assert_eq!(music("pause the music in the kitchen"), room("kitchen", Pause));
        assert_eq!(music("turn the music up in the bedroom"), room("bedroom", Louder));
        assert_eq!(music("what's playing in the kitchen"), room("kitchen", NowPlaying));
        // A title with "in the" keeps it.
        assert_eq!(music("play Dancing in the Dark"), Some(song("dancing in the dark")));
        assert_eq!(music("play Singing in the Rain"), Some(song("singing in the rain")));
        assert_eq!(music("turn off the lights in the kitchen"), None);
        // Other skills keep their rooms.
        assert!(matches!(parse("what's the temperature in the kitchen"), Intent::House(_)));
        assert!(matches!(parse("set a timer in the kitchen for 5 minutes"), Intent::Timer(_)));
        assert!(matches!(parse("add milk to the shopping list"), Intent::ShoppingList(_)));
        assert!(matches!(parse("what's next on the shopping list"), Intent::ShoppingList(_)));

        // A room that is the whole request, or not at its end, is part of a title.
        assert_eq!(music("play In the Kitchen"), Some(song("in the kitchen")));
        assert_eq!(music("play Live in the Kitchen Tonight"), Some(song("live in the kitchen tonight")));
        // Two rooms: it plays in the first and says so.
        assert_eq!(
            music("play music in the kitchen and the bedroom"),
            Some(InRooms(vec!["kitchen".into(), "bedroom".into()], Box::new(Liked)))
        );

        let to = |to: &str, from: Option<&str>| Some(MoveTo(to.into(), from.map(Into::into)));
        assert_eq!(music("move the music to the bedroom"), to("bedroom", None));
        assert_eq!(music("Can you move the music to the kitchen?"), to("kitchen", None));
        assert_eq!(music("flytt musikken til soverommet"), to("soverommet", None));
        assert_eq!(music("move the music in the kitchen to the bedroom"), to("bedroom", Some("kitchen")));
        assert_eq!(music("move the music from the kitchen to the bedroom"), to("bedroom", Some("kitchen")));
        assert_eq!(music("flytt musikken fra kjøkkenet til badet"), to("badet", Some("kjøkkenet")));
        assert_eq!(music("move the music"), None);

        let next = |command: MusicCommand| Some(Queue(Box::new(command), true));
        let last = |command: MusicCommand| Some(Queue(Box::new(command), false));
        assert_eq!(music("play Careless Whisper after this song"), next(song("careless whisper")));
        assert_eq!(music("play Hello by Adele next in the queue"), next(song("hello by adele")));
        assert_eq!(music("play Hello after the current song please"), next(song("hello")));
        assert_eq!(music("spill Kygo etter denne sangen"), next(song("kygo")));
        assert_eq!(music("add Rumours to the queue"), last(song("rumours")));
        assert_eq!(music("add the album Rumours to the queue"), last(Album("rumours".into())));
        assert_eq!(music("queue up Hello by Adele"), last(song("hello by adele")));
        assert_eq!(music("legg til Kygo i køen"), last(song("kygo")));
        assert_eq!(
            music("play Hello after this song in the kitchen"),
            room("kitchen", Queue(Box::new(song("hello")), true))
        );
        // Nothing named: the answer asks what.
        assert_eq!(music("queue up"), last(Resume));
        assert_eq!(music("queue Hello next"), next(song("hello")));
        // The song playing, queued again.
        for text in ["queue this song", "queue it", "add this song to the queue", "legg denne sangen i køen"] {
            assert_eq!(music(text), last(NowPlaying), "{text}");
        }

        // A word left alone after the room says nothing.
        assert_eq!(music("play music in the kitchen too"), room("kitchen", Liked));
        assert_eq!(music("spill musikk på kjøkkenet da"), room("kjøkkenet", Liked));
        assert_eq!(music("play Hello in the kitchen next"), room("kitchen", song("hello")));
        // "And in the bedroom" as well as "and the bedroom".
        assert_eq!(
            music("play jazz in the kitchen and in the bedroom"),
            Some(InRooms(vec!["kitchen".into(), "bedroom".into()], Box::new(song("jazz"))))
        );
        // Titles that end in "next" or "after this".
        for title in ["thank u next", "coming up next", "you're next", "up next", "life after this", "who's next"] {
            assert_eq!(music(&format!("play {title}")), Some(song(title)), "{title}");
        }
        assert_eq!(music("play Hello next"), Some(song("hello next")));
        assert_eq!(music("next song"), Some(Next));
        assert_eq!(music("play next"), Some(song("next")));

        assert_eq!(music("what's next?"), Some(UpNext));
        assert_eq!(music("what song is next"), Some(UpNext));
        assert_eq!(music("what's playing next"), Some(UpNext));
        assert_eq!(music("hva er neste sang"), Some(UpNext));
        assert_eq!(music("what's next week"), None);
    }
}
