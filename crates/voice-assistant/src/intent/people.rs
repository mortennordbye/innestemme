//! People: who is home, where someone is and when they come home. The names are matched
//! against Home Assistant's people later.

use super::*;

/// "Who's home", "is anyone home", "is Ingrid home", "hvem er hjemme", "er Ingrid hjemme".
pub(super) fn whos_home(words: &[String]) -> Option<Option<String>> {
    let at = words.iter().position(|w| w == "home" || w == "hjemme")?;
    let first = words.first()?.as_str();
    if ["who's", "whos", "who", "hvem"].contains(&first) && at <= 3 {
        return Some(None);
    }
    if !["is", "er"].contains(&first) || at == 1 || at > 3 || at + 2 < words.len() {
        return None;
    }
    let name: Vec<&str> =
        words[1..at].iter().map(String::as_str).filter(|w| !["at", "still", "fortsatt", "på"].contains(w)).collect();
    let anyone = ["anyone", "anybody", "someone", "somebody", "noen", "everyone", "everybody", "alle"];
    match name.as_slice() {
        [] => None,
        [one] if anyone.contains(one) => Some(None),
        // Not a person: "is it cold at home".
        words if words.iter().any(|w| ["it", "the", "det", "den", "there", "anything", "noe"].contains(w)) => None,
        words => Some(Some(words.join(" "))),
    }
}

/// Words that make a "where is" request about something other than a person.
const NOT_A_PERSON: &[&str] = &[
    "the",
    "a",
    "an",
    "my",
    "your",
    "our",
    "it",
    "this",
    "that",
    "there",
    "here",
    "you",
    "i",
    "me",
    "we",
    "he",
    "she",
    "they",
    "everyone",
    "everybody",
    "anyone",
    "anybody",
    "someone",
    "somebody",
    "nearest",
    "closest",
    "den",
    "det",
    "de",
    "min",
    "mi",
    "mitt",
    "mine",
    "din",
    "du",
    "jeg",
    "meg",
    "vi",
    "han",
    "hun",
    "dere",
    "alle",
    "noen",
    "nærmeste",
];

/// A name of one or two words that could be a person's.
fn person_name(words: &[String]) -> Option<String> {
    let ok = !words.is_empty()
        && words.len() <= 2
        && words.iter().all(|w| !NOT_A_PERSON.contains(&w.as_str()) && !w.chars().any(|c| c.is_ascii_digit()));
    ok.then(|| words.iter().map(|w| w.trim_end_matches("'s")).collect::<Vec<_>>().join(" "))
}

/// "Where is Ingrid", "where's Ingrid", "is Ingrid at work", "when will Ingrid be home", "hvor er
/// Ingrid", "er Ingrid på jobb", "når kommer Ingrid hjem".
pub(super) fn where_is(words: &[String]) -> Option<Intent> {
    let w: Vec<&str> = words.iter().map(String::as_str).collect();
    let found = |name: String, zone: Option<String>, arriving: bool| Intent::WhereIs { name, zone, arriving };
    match w.as_slice() {
        ["where's" | "wheres", rest @ ..] | ["where", "is" | "are", rest @ ..] | ["hvor", "er", rest @ ..] => {
            let rest: Vec<String> =
                rest.iter().filter(|w| !["now", "right", "nå"].contains(w)).map(|w| (*w).to_owned()).collect();
            if let [one] = rest.as_slice() {
                if ["everyone", "everybody", "alle"].contains(&one.as_str()) {
                    return Some(Intent::WhosHome(None));
                }
            }
            if w[1] == "are" {
                return None;
            }
            return person_name(&rest).map(|name| found(name, None, false));
        }
        ["when" | "når", ..] => {
            let homeward = ["home", "hjem", "hjemme", "back", "tilbake"];
            if !w.iter().any(|x| homeward.contains(x)) || w.len() > 7 {
                return None;
            }
            let verbs = ["will", "is", "does", "did", "should", "kommer", "er", "blir", "skal"];
            let start = w.iter().position(|x| verbs.contains(x))? + 1;
            let ends = ["be", "get", "gets", "come", "coming", "back", "home", "hjem", "hjemme", "komme", "tilbake"];
            let end = start + w[start..].iter().position(|x| ends.contains(x))?;
            let name: Vec<String> = w[start..end].iter().map(|w| (*w).to_owned()).collect();
            return person_name(&name).map(|name| found(name, None, true));
        }
        _ => {}
    }
    // "Is Ingrid at work", "er Ingrid på jobb": a short question, a zone after the name.
    if !["is", "er"].contains(w.first()?) || w.len() > 6 {
        return None;
    }
    let at = w.iter().position(|x| ["at", "på"].contains(x))?;
    let name: Vec<String> = w[1..at].iter().map(|w| (*w).to_owned()).collect();
    let zone: Vec<&str> = w[at + 1..].iter().copied().filter(|x| !["the", "her", "his", "sin"].contains(x)).collect();
    let home = ["home", "hjemme", "hjem"];
    // "Is dinner at seven", "is Kiwi at Storo open": a time or a state, not a zone.
    let not_a_zone = [
        "one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten", "eleven", "twelve", "noon",
        "midnight", "o'clock", "open", "closed", "åpen", "åpent", "stengt", "ett", "tre", "fem", "seks", "sju", "syv",
        "åtte", "ni", "ti", "elleve", "tolv",
    ];
    if zone.is_empty()
        || zone.len() > 2
        || zone.iter().any(|z| home.contains(z) || not_a_zone.contains(z) || z.chars().any(|c| c.is_ascii_digit()))
    {
        return None;
    }
    person_name(&name).map(|name| found(name, Some(zone.join(" ")), false))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn where_someone_is() {
        let at = |name: &str, zone: Option<&str>, arriving: bool| Intent::WhereIs {
            name: name.into(),
            zone: zone.map(str::to_owned),
            arriving,
        };
        // As the transcriber wrote it on the Voice PE; the name is matched by sound later.
        assert_eq!(parse("where is Jon?"), at("jon", None, false));
        assert_eq!(parse("where's Kari"), at("kari", None, false));
        assert_eq!(parse("where is Ingrid right now"), at("ingrid", None, false));
        assert_eq!(parse("hvor er Ingrid"), at("ingrid", None, false));
        assert_eq!(parse("is Ingrid at work"), at("ingrid", Some("work"), false));
        assert_eq!(parse("er Ola på jobb"), at("ola", Some("jobb"), false));
        assert_eq!(parse("is Ola at the fire station"), at("ola", Some("fire station"), false));
        assert_eq!(parse("when will Ingrid be home"), at("ingrid", None, true));
        assert_eq!(parse("when is Ola coming home?"), at("ola", None, true));
        assert_eq!(parse("når kommer Ingrid hjem"), at("ingrid", None, true));
        assert_eq!(parse("where is everyone"), Intent::WhosHome(None));
        assert_eq!(parse("is Ingrid at home"), Intent::WhosHome(Some("ingrid".into())));
        assert_eq!(parse("where's my phone"), Intent::House(HouseCommand::FindPhone(None)));
        for other in [
            "where is the remote",
            "where are my keys",
            "is the dinner at 6",
            "is dinner at seven",
            "is the dinner at seven",
            "is Kiwi at Storo open",
            "where do you live",
            "when is the next bus home",
        ] {
            assert!(!matches!(parse(other), Intent::WhereIs { .. }), "{other}");
        }
    }
}
