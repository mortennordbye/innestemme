//! Party mode: the Hue lights follow the music ("party mode", "fast party mode", "stop the
//! party"). The engine runs it; this is the rules' side.

/// What was asked: on in a mode, or off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PartyCommand {
    On { fast: bool },
    Off,
}

const PARTY_WORDS: &[&str] = &["party", "party's", "partymode", "festmodus", "fest", "disco"];
const FAST_WORDS: &[&str] = &["fast", "faster", "strobe", "rave", "rask", "raskt", "raskere"];
const OFF_WORDS: &[&str] = &["stop", "end", "off", "over", "cancel", "enough", "stopp", "slutt", "av", "ferdig"];
const NORMAL_WORDS: &[&str] = &["normal", "slow", "slower", "calm", "rolig", "vanlig"];

/// Short requests that name the party: "party mode", "let's party", "start fast party mode",
/// "stop the party", "skru på festmodus".
pub fn parse(words: &[String]) -> Option<PartyCommand> {
    if words.len() > 8 || !words.iter().any(|w| PARTY_WORDS.contains(&w.as_str())) {
        return None;
    }
    let has = |list: &[&str]| words.iter().any(|w| list.contains(&w.as_str()));
    // "Turn on party mode" holds "on", not "off"; "the party is over" holds "over".
    if has(OFF_WORDS) {
        return Some(PartyCommand::Off);
    }
    Some(PartyCommand::On { fast: has(FAST_WORDS) && !has(NORMAL_WORDS) })
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
        assert_eq!(party("Party mode."), Some(PartyCommand::On { fast: false }));
        assert_eq!(party("Let's party!"), Some(PartyCommand::On { fast: false }));
        assert_eq!(party("Start the party"), Some(PartyCommand::On { fast: false }));
        assert_eq!(party("Turn on fast party mode"), Some(PartyCommand::On { fast: true }));
        assert_eq!(party("Party mode fast"), Some(PartyCommand::On { fast: true }));
        assert_eq!(party("Normal party mode"), Some(PartyCommand::On { fast: false }));
        assert_eq!(party("Stop the party"), Some(PartyCommand::Off));
        assert_eq!(party("Party's over."), Some(PartyCommand::Off));
        assert_eq!(party("Turn off party mode"), Some(PartyCommand::Off));
        assert_eq!(party("Skru på festmodus"), Some(PartyCommand::On { fast: false }));
        assert_eq!(party("Slå av festmodus"), Some(PartyCommand::Off));
        assert_eq!(party("Play party music by the Black Eyed Peas from their best album ever"), None);
        assert_eq!(party("Turn off the lights"), None);
    }
}
