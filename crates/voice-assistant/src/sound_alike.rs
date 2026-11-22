//! Ask when unsure: a look-up topic that sounds like a skill word. The look-up takes every "what is
//! ..." no other rule takes, so a misheard skill word went to Wikipedia ("What's therapy?" said
//! right after a temperature question). The transcriber gives one transcript, so the only sign of
//! doubt is a topic that sounds like a skill word: then the assistant asks which one was meant.
//! English only for now.

use crate::dialog::words;

/// A skill word that is easy to mishear, and how the assistant asks for and answers it.
#[derive(Debug, PartialEq, Eq)]
pub struct SkillWord {
    /// The skill word as the reply may name it ("temperature", "shopping list").
    pub word: &'static str,
    /// As the question says it: "Did you mean the temperature?"
    pub said: &'static str,
    /// The plain request for the skill, which the rules take ("what's the temperature").
    pub request: &'static str,
    /// What the transcriber writes for the word, as normalised words. An explicit list rather than
    /// an edit distance or a sound key: those also matched real topics ("tumor" and "timer", "tome"
    /// and "time", "leather" and "weather"), and a question nobody needed annoys more than a rare
    /// wrong look-up. The word itself is not in it: "what is music" said as heard is a real topic.
    heard: &'static [&'static str],
}

/// The skill words, each with the topics heard for it: from the logs ("therapy"), and homophones.
pub const SKILL_WORDS: &[SkillWord] = &[
    SkillWord {
        word: "temperature",
        said: "the temperature",
        request: "what's the temperature",
        // "therapy": 21 Nov, right after a temperature question.
        heard: &["therapy", "temprature", "tempature", "temperture"],
    },
    SkillWord { word: "time", said: "the time", request: "what's the time", heard: &["thyme", "tyme"] },
    SkillWord { word: "weather", said: "the weather", request: "what's the weather", heard: &["whether", "wether"] },
    SkillWord { word: "news", said: "the news", request: "what's the news", heard: &["noose", "gnus", "nooz"] },
    SkillWord {
        word: "timer",
        said: "the timer",
        request: "how long is left on the timer",
        heard: &["tymer", "timar"],
    },
    SkillWord { word: "alarm", said: "the alarm", request: "when is my alarm", heard: &["allarm", "alarum"] },
    SkillWord { word: "lights", said: "the lights", request: "which lights are on", heard: &["lites", "lytes"] },
    SkillWord { word: "music", said: "the music", request: "what's playing", heard: &["musik", "musick"] },
    SkillWord {
        word: "shopping list",
        said: "the shopping list",
        request: "what's on the shopping list",
        // Most mishearings keep "shopping" or "list", which the list's own rules take.
        heard: &["chopping lisp"],
    },
];

/// Openings that ask what a thing is: a misheard "what's the temperature" keeps its "what". "Who
/// is", "tell me about" and "look up" name a topic on purpose.
const WHAT: &[&str] = &["what's", "whats", "what is", "what are", "what was"];
/// Words before the opening that do not change the question.
const LEAD: &[&str] = &["so", "oh", "okay", "ok", "hey", "well", "um", "uh", "and", "but", "please"];

/// The skill word a look-up request sounds like, and the topic as heard, when `text` asks "what is"
/// a topic heard for exactly one skill word.
pub fn sounds_like_skill(text: &str) -> Option<(&'static SkillWord, String)> {
    let said = words(text);
    let lead = said.iter().take_while(|w| LEAD.contains(&w.as_str())).count();
    let joined = said[lead..].join(" ");
    WHAT.iter().find(|o| joined.starts_with(&format!("{o} ")))?;
    let asked: Vec<&str> = text.split_whitespace().skip(lead).collect();
    let topic = crate::lookup::topic(&asked.join(" "))?;
    let heard = words(&topic).join(" ");
    let mut matches = SKILL_WORDS.iter().filter(|skill| skill.heard.contains(&heard.as_str()));
    let skill = matches.next()?;
    // Close to two skill words: the question could not say which.
    matches.next().is_none().then_some((skill, topic))
}

/// "Did you mean the temperature? Or should I look up therapy?": the fixed sentence first, so the
/// speech cache keeps it, and the topic in its own.
pub fn question(skill: &SkillWord, topic: &str) -> String {
    format!("{} Or should I look up {topic}?", fixed_question(skill))
}

fn fixed_question(skill: &SkillWord) -> String {
    format!("Did you mean {}?", skill.said)
}

/// The fixed sentences of the questions, for rendering them ahead.
pub fn fixed_questions() -> Vec<String> {
    SKILL_WORDS.iter().map(fixed_question).collect()
}

/// What a reply to the question chose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Choice {
    /// The skill word: "the temperature", "the first one".
    Skill,
    /// The look-up: "look it up", "therapy", "the second one", "Wikipedia".
    Lookup,
    /// "No", "never mind", "neither": the question ends.
    Neither,
}

/// Words around a reply that do not change it: "the first one please", "I meant the temperature".
const FILLER: &[&str] =
    &["the", "one", "please", "i", "meant", "mean", "oh", "um", "uh", "well", "just", "so", "okay", "ok", "yeah"];

/// The choice `reply` makes, if it is one; anything else is a new request.
pub fn choice(reply: &str, skill: &SkillWord, topic: &str) -> Option<Choice> {
    let all = words(reply);
    if matches!(
        all.iter().map(String::as_str).collect::<Vec<_>>().as_slice(),
        ["no"]
            | ["nope"]
            | ["no", "thanks" | "thank"]
            | ["never", "mind"]
            | ["neither"]
            | ["neither", "of", "them"]
            | ["cancel"]
            | ["forget", "it"]
            | ["nothing"]
    ) {
        return Some(Choice::Neither);
    }
    let mut said: Vec<&str> = all.iter().map(String::as_str).filter(|w| !FILLER.contains(w)).collect();
    // "No, the temperature": the "no" turns down the other reading.
    if said.len() > 1 && said[0] == "no" {
        said.remove(0);
    }
    let said = said.join(" ");
    let topic = words(topic).join(" ");
    let skill_word = skill.word;
    if said == skill_word || said == "first" {
        return Some(Choice::Skill);
    }
    let lookup = ["second", "look it up", "look up", "look that up", "wikipedia", "look it up on wikipedia"];
    if said == topic || lookup.contains(&said.as_str()) || said == format!("look up {topic}") {
        return Some(Choice::Lookup);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::intent::{parse, Intent};

    fn skill_of(text: &str) -> Option<&'static str> {
        sounds_like_skill(text).map(|(skill, _)| skill.word)
    }

    #[test]
    fn a_topic_heard_for_a_skill_word_asks() {
        assert_eq!(skill_of("What's therapy?"), Some("temperature"));
        assert_eq!(skill_of("what is thyme"), Some("time"));
        assert_eq!(skill_of("What's whether?"), Some("weather"));
        assert_eq!(skill_of("what's the noose"), Some("news"));
        assert_eq!(skill_of("so what's the chopping lisp"), Some("shopping list"));
        assert_eq!(skill_of("What is the temprature?"), Some("temperature"));
        let (skill, topic) = sounds_like_skill("What's therapy?").unwrap();
        assert_eq!(topic, "therapy");
        assert_eq!(question(skill, &topic), "Did you mean the temperature? Or should I look up therapy?");
    }

    #[test]
    fn real_topics_do_not_ask() {
        for text in [
            "what is a black hole",
            "who is Ada Lovelace",
            "what is thermodynamics",
            "what is a timeline",
            "what is the weather channel",
            "what is a tumor",
            "what is a tome",
            "what is leather",
            "what is a team",
            "what is time",
            "what is music",
            "what is a temperature",
            "what is weather",
            "what is news",
            "what is an alarm clock",
            "what is light",
            "what is a list",
            "what is shopping",
            "what is the timer function",
            "what is temperament",
            "what is tempura",
            "what is a tempo",
            "what is a therapist",
            "what is physical therapy",
            "what is therapy dog",
            "what is a newsletter",
            "what is New York",
            "what is a noose knot",
            "what is weather forecasting",
            "what is a lighthouse",
            "what is musical theatre",
            "what is a timer circuit",
            "what is a gnu",
            "what is the Thames",
            "what is a theremin",
            "what is the theory of relativity",
            "what is a thimble",
            "who is Tim Berners-Lee",
            "tell me about therapy",
            "look up thyme",
            "who is whether",
            "what's the capital of France",
            "turn on the lights",
            "what's the weather",
            "what time is it",
        ] {
            assert_eq!(skill_of(text), None, "{text}");
        }
    }

    #[test]
    fn each_request_is_taken_by_its_skill() {
        for skill in SKILL_WORDS {
            let intent = parse(skill.request);
            assert!(!matches!(intent, Intent::Unknown | Intent::Lookup(_)), "{}: {intent:?}", skill.request);
            for heard in skill.heard {
                assert_ne!(*heard, skill.word, "the word itself is a real topic");
                let text = format!("what's {heard}");
                assert!(matches!(parse(&text), Intent::Lookup(_)), "{text}: {:?}", parse(&text));
                assert_eq!(skill_of(&text), Some(skill.word), "{text}");
            }
        }
    }

    #[test]
    fn reads_the_reply() {
        let skill = &SKILL_WORDS[0];
        let pick = |reply: &str| choice(reply, skill, "therapy");
        for reply in
            ["the temperature", "temperature", "the first one", "the first", "first", "I meant the temperature"]
        {
            assert_eq!(pick(reply), Some(Choice::Skill), "{reply}");
        }
        assert_eq!(pick("no, the temperature"), Some(Choice::Skill));
        for reply in ["look it up", "therapy", "the second one", "the second", "Wikipedia", "look up therapy", "second"]
        {
            assert_eq!(pick(reply), Some(Choice::Lookup), "{reply}");
        }
        for reply in ["no", "never mind", "neither", "No thanks.", "Neither of them."] {
            assert_eq!(pick(reply), Some(Choice::Neither), "{reply}");
        }
        for reply in ["what's the weather", "turn on the lights", "yes", "the third one", "", "the"] {
            assert_eq!(pick(reply), None, "{reply}");
        }
        let list = SKILL_WORDS.iter().find(|s| s.word == "shopping list").unwrap();
        assert_eq!(choice("the shopping list", list, "chopping lisp"), Some(Choice::Skill));
        assert_eq!(choice("chopping lisp", list, "chopping lisp"), Some(Choice::Lookup));
    }
}
