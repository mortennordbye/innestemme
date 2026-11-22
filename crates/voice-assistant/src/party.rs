//! Party mode: the Hue lights follow the music ("party mode", "fast party mode", "relaxed party
//! mode", "stop the party"). The engine runs it; this is the rules' side.

/// How the lights follow the music.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pace {
    /// Warm, relaxing light that drifts with the beat.
    Relaxed,
    Normal,
    Fast,
}

/// What was asked: on at a pace, or off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PartyCommand {
    On(Pace),
    Off,
    /// "Is party mode on?"
    Status,
}

const PARTY_WORDS: &[&str] = &["party", "party's", "partymode", "festmodus", "fest", "disco"];
const FAST_WORDS: &[&str] = &["fast", "faster", "strobe", "rave", "rask", "raskt", "raskere"];
// "Slope the party" (16 Nov) was "stop the party": a sound-alike of stop is never a start.
const OFF_WORDS: &[&str] = &[
    "stop", "stops", "stopped", "slope", "stomp", "pause", "kill", "disable", "end", "off", "over", "cancel", "enough",
    "stopp", "slutt", "av", "ferdig",
];
/// Words that ask for the party to start. Without one, "<verb> the party" is not a start: an
/// unknown verb may be a misheard "stop".
const START_WORDS: &[&str] = &[
    "started",
    "going",
    "activate",
    "enable",
    "start",
    "begin",
    "let's",
    "lets",
    "on",
    "mode",
    "partymode",
    "festmodus",
    "disco",
    "have",
    "throw",
    "kick",
    "time",
    "på",
    "starte",
    "begynn",
];
const RELAXED_WORDS: &[&str] = &[
    "relaxed",
    "relax",
    "relaxing",
    "chill",
    "chilled",
    "calm",
    "slow",
    "slower",
    "mellow",
    "cosy",
    "cozy",
    "gentle",
    "soft",
    "rolig",
    "avslappet",
    "avslappende",
];
const NORMAL_WORDS: &[&str] = &["normal", "vanlig"];

/// Short requests that name the party: "party mode", "let's party", "start fast party mode",
/// "relaxed party mode", "stop the party", "skru på festmodus".
pub fn parse(words: &[String]) -> Option<PartyCommand> {
    if words.len() > 8 || !words.iter().any(|w| PARTY_WORDS.contains(&w.as_str())) {
        return None;
    }
    let has = |list: &[&str]| words.iter().any(|w| list.contains(&w.as_str()));
    // "Is party mode on?" asks; it must not start the party.
    if ["is", "are", "did", "was", "if", "whether", "er", "står", "om"].contains(&crate::intent::question_word(words))
    {
        return Some(PartyCommand::Status);
    }
    // "Pause the party music" is the music's; only "pause the party" is the party's.
    if has(&["pause", "kill", "disable"]) && has(&["music", "playlist", "song", "songs", "track", "musikk"]) {
        return None;
    }
    // "Turn on party mode" holds "on", not "off"; "the party is over" holds "over".
    if has(OFF_WORDS) {
        return Some(PartyCommand::Off);
    }
    let paced = has(NORMAL_WORDS) || has(FAST_WORDS) || has(RELAXED_WORDS);
    // "Party please", "okay, party lights": short asks; in a longer sentence those words start nothing.
    let core: Vec<&str> =
        words.iter().map(String::as_str).filter(|w| !["please", "okay", "ok", "lights"].contains(w)).collect();
    let short = matches!(core.as_slice(), [w] if PARTY_WORDS.contains(w));
    if !has(START_WORDS) && !paced && !short && words.len() > 1 {
        return None;
    }
    let pace = if has(NORMAL_WORDS) {
        Pace::Normal
    } else if has(FAST_WORDS) {
        Pace::Fast
    } else if has(RELAXED_WORDS) {
        Pace::Relaxed
    } else {
        Pace::Normal
    };
    Some(PartyCommand::On(pace))
}

/// "Shop the party", "drop the party": "stop the party" misheard. While a party is on, it means
/// stop. Only known sound-alikes of "stop" count, so "join the party" or "enjoy the party" said
/// over the music leave it playing.
pub fn unclear(words: &[String]) -> bool {
    const MISHEARD_STOP: &[&str] = &[
        "shop", "slop", "slope", "drop", "top", "stomp", "stock", "stub", "stob", "stuck", "stopped", "stops", "stap",
        "stoppe", "stopper", "stup",
    ];
    let mut words: Vec<&str> = words.iter().map(String::as_str).collect();
    while words.last().is_some_and(|w| ["now", "please", "nå", "takk"].contains(w)) {
        words.pop();
    }
    matches!(words.as_slice(), [stop, "the", party] if MISHEARD_STOP.contains(stop) && PARTY_WORDS.contains(party))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn party(text: &str) -> Option<PartyCommand> {
        let words: Vec<String> = text.split_whitespace().map(crate::dialog::normalize).collect();
        parse(&words)
    }

    #[test]
    fn party_requests() {
        assert_eq!(party("Party mode."), Some(PartyCommand::On(Pace::Normal)));
        assert_eq!(party("Let's party!"), Some(PartyCommand::On(Pace::Normal)));
        assert_eq!(party("Start the party"), Some(PartyCommand::On(Pace::Normal)));
        assert_eq!(party("Turn on fast party mode"), Some(PartyCommand::On(Pace::Fast)));
        assert_eq!(party("Party mode fast"), Some(PartyCommand::On(Pace::Fast)));
        assert_eq!(party("Normal party mode"), Some(PartyCommand::On(Pace::Normal)));
        assert_eq!(party("Relaxed party mode"), Some(PartyCommand::On(Pace::Relaxed)));
        assert_eq!(party("Start a chill party"), Some(PartyCommand::On(Pace::Relaxed)));
        assert_eq!(party("Slow party mode"), Some(PartyCommand::On(Pace::Relaxed)));
        assert_eq!(party("Rolig festmodus"), Some(PartyCommand::On(Pace::Relaxed)));
        assert_eq!(party("Stop the party"), Some(PartyCommand::Off));
        assert_eq!(party("Stop the relaxed party"), Some(PartyCommand::Off));
        assert_eq!(party("Party's over."), Some(PartyCommand::Off));
        assert_eq!(party("Turn off party mode"), Some(PartyCommand::Off));
        assert_eq!(party("Skru på festmodus"), Some(PartyCommand::On(Pace::Normal)));
        assert_eq!(party("Slå av festmodus"), Some(PartyCommand::Off));
        assert_eq!(party("Play party music by the Black Eyed Peas from their best album ever"), None);
        assert_eq!(party("Turn off the lights"), None);
    }

    /// Heard in the living room, 16 November 2026.
    #[test]
    fn a_misheard_stop_never_starts_the_party() {
        assert_eq!(party("Slope the party."), Some(PartyCommand::Off));
        assert_eq!(party("Who will stop the party?"), Some(PartyCommand::Off));
        assert_eq!(party("Shop the party."), None);
        assert_eq!(party("Party!"), Some(PartyCommand::On(Pace::Normal)));
        assert_eq!(party("Party time"), Some(PartyCommand::On(Pace::Normal)));
        assert_eq!(party("Have a party"), Some(PartyCommand::On(Pace::Normal)));
        for start in ["Get the party started", "Party please", "Party lights", "Enable party", "Activate the party"] {
            assert_eq!(party(start), Some(PartyCommand::On(Pace::Normal)), "{start}");
        }
        for stop in ["Pause the party please", "Kill the party lights", "Disable party mode"] {
            assert_eq!(party(stop), Some(PartyCommand::Off), "{stop}");
        }
        for start in ["Party lights please", "Okay party please", "Please party", "Get the party going"] {
            assert_eq!(party(start), Some(PartyCommand::On(Pace::Normal)), "{start}");
        }
        assert_eq!(party("Pause the party music"), None);
        assert_eq!(party("Kill the party playlist"), None);
        for talk in ["Shop the party please", "Get rid of the party", "I didn't get invited to the party"] {
            assert_eq!(party(talk), None, "{talk}");
        }
    }

    /// While a party is on, these mean stop; they start nothing either way.
    #[test]
    fn unclear_party_requests() {
        let unclear = |text: &str| super::unclear(&crate::dialog::words(text));
        for stop in ["Shop the party.", "Shop the party please", "Drop the party", "Slop the party now"] {
            assert!(unclear(stop), "{stop}");
        }
        for not in [
            "Stop the party",
            "Party mode",
            "Start the party",
            "Is party mode on?",
            "Pause the party music",
            "I loved that party",
            "I didn't get invited to the party",
            "Turn off the lights",
            "Get rid of the party",
            // Cheering in the follow-up window.
            "Great party",
            "The party",
            "Party people",
            "Party all night",
            "This party rocks",
            "Love this party",
            "Best party ever",
            "Party hats",
            "Love the party",
            // Other verbs said over the music.
            "Join the party",
            "Enjoy the party",
            "Rock the party",
            "Save the party",
            "Feel the party",
            "Plan the party",
            "Leave the party",
        ] {
            assert!(!unclear(not), "{not}");
        }
    }
}
