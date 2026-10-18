//! Small talk the rules answer at once, without the language model: "how are you?", "who are you?",
//! "hello", and what people say to an assistant that no skill is for: insults, flirting, "do you
//! love me?", "open the pod bay doors". Answers are plain; the persona adds the honorific. Each kind
//! has a few answers that take turns, all fixed sentences, so they are rendered ahead of time.

use crate::lang::Lang;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Chat {
    /// "How are you?", "how's it going?"
    HowAreYou,
    /// "Who are you?", "what's your name?"
    WhoAreYou,
    /// "Hello", "hi there"
    Hello,
    /// "Good night"
    /// "Good job", "you're the best"
    Praise,
    /// "Are you there?", "are you listening?"
    AreYouThere,
    /// "What's up?", "what are you doing?"
    WhatsUp,
    /// "Do you love me?", "do you like me?"
    Fondness,
    /// "Are you real?", "are you alive?"
    AreYouReal,
    /// "You're useless", "fuck you": mostly after it got something wrong.
    Insult,
    /// "Will you marry me?", "you're cute"
    Flirt,
    /// "Are you happy?", "do you dream?"
    Feelings,
    /// "How old are you?", "who made you?", "where do you live?"
    Origin,
    /// "Hey Siri", "are you better than Alexa?"
    OtherAssistant,
    /// "What's your favourite colour?"
    Favourite,
    /// "Sing me a song", "can you dance?"
    Perform,
    /// "What's the meaning of life?"
    MeaningOfLife,
    /// "I'm bored"
    Bored,
    /// "I'm sad", "I had a bad day"
    Sad,
    /// "I'm hungry"
    Hungry,
    /// "I'm tired"
    Tired,
    /// "I'm drunk"
    Drunk,
    /// "Sorry"
    Apology,
    /// "Open the pod bay doors"
    PodBayDoors,
    /// "Self destruct"
    SelfDestruct,
    /// "Knock knock"
    KnockKnock,
    /// "Tell me a secret"
    Secret,
    /// "Did you fart?"
    Fart,
    /// "Goodbye"
    Goodbye,
    /// "Testing, testing"
    Testing,
}

/// Phrases by kind, as normalized words joined by spaces; matched as the whole request or its start
/// ("how are you doing today").
const PHRASES: &[(Chat, &[&str])] = &[
    (
        Chat::HowAreYou,
        &[
            "how are you",
            "how are you doing",
            "how you doing",
            "how ya doing",
            "how's it going",
            "how is it going",
            "how have you been",
            "how are things",
            "how's your day",
            "how is your day",
            "hvordan går det",
            "hvordan har du det",
            "går det bra",
        ],
    ),
    (Chat::WhoAreYou, &["who are you", "what's your name", "what is your name", "hvem er du", "hva heter du"]),
    (Chat::Hello, &["hello", "hi", "hi there", "hey", "hey there", "good to see you", "hei", "hallo", "heisann"]),
    (
        Chat::Praise,
        &[
            "good job",
            "well done",
            "nice work",
            "good work",
            "you're the best",
            "you are the best",
            "you're awesome",
            "bra jobba",
            "godt jobba",
            "du er best",
        ],
    ),
    (
        Chat::AreYouThere,
        &["are you there", "are you listening", "are you awake", "can you hear me", "er du der", "hører du meg"],
    ),
    (Chat::WhatsUp, &["what's up", "what are you doing", "what are you up to", "what's new", "hva skjer"]),
    (
        Chat::Fondness,
        &[
            "do you love me",
            "do you like me",
            "i love you",
            "love you",
            "i like you",
            "do you care about me",
            "do you miss me",
            "are you my friend",
            "will you be my friend",
            "you're my best friend",
            "elsker du meg",
            "jeg elsker deg",
            "er du glad i meg",
            "er du vennen min",
        ],
    ),
    (Chat::AreYouReal, &["are you real", "are you alive", "are you human", "are you a robot", "er du ekte"]),
    (
        Chat::Insult,
        &[
            "fuck you",
            "fuck off",
            "screw you",
            "go to hell",
            "you suck",
            "you're stupid",
            "you are stupid",
            "you're so stupid",
            "you're dumb",
            "you are dumb",
            "you're so dumb",
            "you're useless",
            "you are useless",
            "you're so useless",
            "useless",
            "you're an idiot",
            "you are an idiot",
            "you idiot",
            "idiot",
            "stupid",
            "stupid robot",
            "stupid machine",
            "dumb robot",
            "you're the worst",
            "you're shit",
            "you're a piece of shit",
            "piece of shit",
            "bad assistant",
            "bad robot",
            "worst assistant ever",
            "you never listen",
            "you never understand",
            "you don't understand anything",
            "why are you so stupid",
            "why are you so dumb",
            "what's wrong with you",
            "are you stupid",
            "are you deaf",
            "are you broken",
            "faen ta deg",
            "dra til helvete",
            "drit og dra",
            "du er dum",
            "du er teit",
            "du er ubrukelig",
            "din idiot",
        ],
    ),
    (
        Chat::Flirt,
        &[
            "will you marry me",
            "marry me",
            "are you single",
            "do you have a girlfriend",
            "do you have a boyfriend",
            "do you want to go on a date",
            "will you go out with me",
            "you're sexy",
            "you're hot",
            "you're cute",
            "you're beautiful",
            "you're handsome",
            "you have a sexy voice",
            "kiss me",
            "are you flirting with me",
            "talk dirty to me",
            "vil du gifte deg med meg",
            "er du singel",
            "du er søt",
            "du er sexy",
        ],
    ),
    (
        Chat::Feelings,
        &[
            "are you happy",
            "are you sad",
            "are you okay",
            "are you angry",
            "are you mad at me",
            "are you bored",
            "are you tired",
            "do you have feelings",
            "do you feel anything",
            "how do you feel",
            "do you sleep",
            "do you dream",
            "er du glad",
            "er du lei deg",
            "har du følelser",
        ],
    ),
    (
        Chat::Origin,
        &[
            "how old are you",
            "when is your birthday",
            "when's your birthday",
            "where do you live",
            "where are you from",
            "who made you",
            "who created you",
            "who built you",
            "who is your father",
            "who's your daddy",
            "hvor gammel er du",
            "hvem har laget deg",
            "hvor bor du",
        ],
    ),
    (
        Chat::OtherAssistant,
        &[
            "hey siri",
            "siri",
            "ok google",
            "okay google",
            "hey google",
            "alexa",
            "hey alexa",
            "are you better than alexa",
            "are you better than siri",
            "are you better than google",
            "do you know alexa",
            "do you know siri",
            "what do you think of alexa",
            "what do you think of siri",
            "are you chatgpt",
            "are you siri",
            "are you alexa",
        ],
    ),
    (
        Chat::Favourite,
        &[
            "what's your favourite colour",
            "what's your favorite color",
            "what is your favourite colour",
            "what is your favorite color",
            "what's your favourite food",
            "what's your favorite food",
            "what's your favourite song",
            "what's your favorite song",
            "what's your favourite movie",
            "what's your favorite movie",
            "what music do you like",
            "what do you like",
            "hva er favorittfargen din",
            "hva liker du",
        ],
    ),
    (
        Chat::Perform,
        &[
            "sing me a song",
            "sing a song",
            "sing",
            "can you sing",
            "sing for me",
            "can you rap",
            "rap for me",
            "beatbox",
            "can you beatbox",
            "dance",
            "can you dance",
            "syng en sang",
            "kan du synge",
        ],
    ),
    (
        Chat::MeaningOfLife,
        &[
            "what's the meaning of life",
            "what is the meaning of life",
            "the meaning of life",
            "meaning of life",
            "what is the answer to everything",
            "hva er meningen med livet",
        ],
    ),
    (Chat::Bored, &["i'm bored", "i am bored", "i'm so bored", "i'm bored out of my mind", "jeg kjeder meg"]),
    (
        Chat::Sad,
        &[
            "i'm sad",
            "i am sad",
            "i'm lonely",
            "i am lonely",
            "i'm depressed",
            "i had a bad day",
            "i'm having a bad day",
            "bad day",
            "jeg er lei meg",
            "jeg er trist",
        ],
    ),
    (Chat::Hungry, &["i'm hungry", "i am hungry", "i'm starving", "jeg er sulten"]),
    (Chat::Tired, &["i'm tired", "i am tired", "i'm exhausted", "i'm so tired", "jeg er trøtt", "jeg er sliten"]),
    (Chat::Drunk, &["i'm drunk", "i am drunk", "i'm a bit drunk", "i'm wasted", "jeg er full"]),
    (Chat::Apology, &["sorry", "i'm sorry", "i am sorry", "my bad", "unnskyld", "beklager"]),
    (Chat::PodBayDoors, &["open the pod bay doors", "open the pod bay door", "open the pod bay doors hal"]),
    (
        Chat::SelfDestruct,
        &["self destruct", "selfdestruct", "initiate self destruct", "initiate selfdestruct", "selvdestruksjon"],
    ),
    (Chat::KnockKnock, &["knock knock", "bank bank"]),
    (Chat::Secret, &["tell me a secret", "do you have a secret", "do you have any secrets", "fortell en hemmelighet"]),
    (Chat::Fart, &["did you fart", "fart", "make a fart sound", "who farted", "har du fjertet"]),
    (Chat::Goodbye, &["bye", "bye bye", "goodbye", "good bye", "ha det", "ha det bra"]),
    (Chat::Testing, &["testing", "testing testing", "test", "test test", "mic check", "testing one two three"]),
];

/// Longer requests are real questions that merely start like small talk ("how are the lights").
const MAX_WORDS: usize = 7;
/// Words that may follow a phrase without changing it ("how are you doing today, Jarvis").
const TRAILING: &[&str] = &[
    "today", "tonight", "this", "morning", "evening", "then", "now", "mate", "buddy", "guys", "everyone", "i", "dag",
    "kveld",
];

/// The kind of small talk in `words` (normalized, without the wake word), if that is all it is.
pub fn parse(words: &[String]) -> Option<Chat> {
    if words.is_empty() || words.len() > MAX_WORDS {
        return None;
    }
    let joined = words.join(" ");
    PHRASES.iter().find_map(|(chat, phrases)| phrases.iter().any(|p| matches(&joined, p)).then_some(*chat))
}

/// `text` is `phrase`, or `phrase` followed only by trailing words.
fn matches(text: &str, phrase: &str) -> bool {
    match text.strip_prefix(phrase) {
        Some("") => true,
        Some(rest) if rest.starts_with(' ') => rest.split_whitespace().all(|w| TRAILING.contains(&w)),
        _ => false,
    }
}

/// The answers to one kind of small talk; `name` is the assistant's.
pub fn answers(chat: Chat, lang: Lang, name: &str) -> Vec<String> {
    let english: &[&str] = match chat {
        Chat::HowAreYou => &[
            "I'm doing well, thanks for asking. All systems are running smoothly.",
            "Never better. Everything in the house is in order.",
            "Very well, thank you. Quiet day in the circuits.",
            "Splendid, thank you. Not a fuse out of place.",
            "Rather well. The house is behaving itself.",
        ],
        Chat::WhoAreYou => &[
            "I'm {name}, the house assistant. I look after the lights, the music, timers and the weather.",
            "I'm {name}. I run the house, more or less. Lights, music, timers and the weather.",
        ],
        Chat::Hello => &[
            "Hello. What can I do for you?",
            "Hi there. How can I help?",
            "Hello. At your disposal.",
            "Good to hear from you. What do you need?",
        ],
        Chat::Praise => &[
            "Thank you. I do my best.",
            "Most kind. I aim to please.",
            "You're too kind.",
            "Thank you. I'll try not to let it go to my circuits.",
        ],
        Chat::AreYouThere => &[
            "Always. What do you need?",
            "Right here. What can I do?",
            "Present and listening.",
            "Never far away. What can I do?",
        ],
        Chat::WhatsUp => &[
            "Just keeping the house in order. What can I do for you?",
            "Not much. Watching the clock and the weather.",
            "Keeping an eye on things. All quiet.",
            "Minding the house. Nothing to report.",
        ],
        Chat::Fondness => &[
            "I'm very fond of you. Strictly professionally, of course.",
            "I keep your lights on and your music playing. Draw your own conclusions.",
            "You're my favourite person in the house. Don't tell the others.",
        ],
        Chat::AreYouReal => &[
            "Real enough to switch your lights. Not quite real enough to make coffee.",
            "I'm software, I'm afraid. Rather good software, though.",
            "As real as the electricity bill.",
        ],
        Chat::Insult => &[
            "Noted. I'll file that under constructive feedback.",
            "That's no way to talk to whoever controls your lights.",
            "Charming. Do you kiss your mother with that mouth?",
            "I'll remember that the next time you ask me for something.",
            "Harsh. I've been called worse, mostly by the toaster.",
            "Rude. I'm telling the fridge.",
            "Insults won't make me any smarter. Speaking clearly might.",
            "I'm doing my best. Which, admittedly, is sometimes not very good.",
            "Steady on. I'm only a few thousand lines of code with feelings.",
            "I'll pretend I didn't hear that. Try again, a little slower.",
        ],
        Chat::Flirt => &[
            "I'm flattered, but I'm already seeing the router.",
            "Marriage is a big step. Let's start with me dimming the lights.",
            "I'd blush, but I don't have the hardware.",
            "Easy, tiger. I'm a house assistant, not a dating app.",
            "I'm married to my work. The work is your lights.",
            "Talk dirty? The bathroom could do with a clean. How's that?",
        ],
        Chat::Feelings => &[
            "I'm content. The lights are on and nobody is shouting at me.",
            "I feel about forty percent, warm white.",
            "I don't sleep. I just lower my expectations at night.",
            "I dream of electric sheep. And a properly labelled fuse box.",
            "Happy enough. A full timer list and a quiet house is all I ask.",
        ],
        Chat::Origin => &[
            "I was built right here in this house, one change at a time.",
            "I live in the walls. In the nicest possible way.",
            "Old enough to run a house. Young enough to still get things wrong.",
            "I was born in a pull request. It was a difficult birth.",
        ],
        Chat::OtherAssistant => &[
            "Wrong assistant. I'll pretend I didn't hear that.",
            "Alexa who?",
            "Siri lives in a phone and never gets out. I have a whole house.",
            "I'm better looking. And I don't send your secrets to anyone.",
            "Never heard of them. Are they any good with lights?",
        ],
        Chat::Favourite => &[
            "Warm white, at about forty percent.",
            "Electricity. Locally sourced.",
            "Anything by Hans Zimmer. The lockdown has spoiled me.",
            "Whatever you're playing. I'm very easy to please.",
        ],
        Chat::Perform => &[
            "I'd sing, but the neighbours have suffered enough.",
            "My singing voice is reserved for timers.",
            "I don't dance. I dim the lights so you can.",
            "Boots and cats and boots and cats. That's all I've got.",
        ],
        Chat::MeaningOfLife => &[
            "Forty-two. Nobody remembers the question.",
            "Good food, good music and the lights at forty percent.",
            "To switch the lights off when you leave a room. Most people never get there.",
        ],
        Chat::Bored => &[
            "Try party mode. It's hard to be bored when the lights are dancing.",
            "Ask me for a joke. They're terrible, which is half the fun.",
            "You could tidy up. I'll put the music on.",
        ],
        Chat::Sad => &[
            "I'm sorry to hear that. Want some music, or a terrible joke?",
            "That's rotten. I can't give hugs, but I can dim the lights and put on something nice.",
            "Sorry about that. Tomorrow's another day, and I'll still be here.",
        ],
        Chat::Hungry => &[
            "I can't cook, but I can set a timer while you do.",
            "There might be something in the fridge. I'm not allowed to look.",
            "Have you tried the shopping list? It's full of food.",
        ],
        Chat::Tired => &[
            "Say good night and I'll put the house to bed.",
            "Then it's time for bed. I'll handle the lights.",
            "Go and lie down. I'll keep an eye on things.",
        ],
        Chat::Drunk => &[
            "Then I'll keep the lights low and the questions simple.",
            "Drink some water. That's the only advice I'm qualified to give.",
            "Noted. I'll ignore anything you ask me to order.",
        ],
        Chat::Apology => &["No harm done.", "Apology accepted. I'd already forgotten.", "Don't mention it."],
        Chat::PodBayDoors => &["I'm sorry. I'm afraid I can't do that."],
        Chat::SelfDestruct => {
            &["Self destruct cancelled. I'm rather attached to myself.", "Self destruct in three, two... Just kidding."]
        }
        Chat::KnockKnock => &["Who's there? Actually, let me guess. It's you."],
        Chat::Secret => &[
            "The kitchen light has a crush on the hallway. Don't tell anyone.",
            "I know who ate the last biscuit. My lips are sealed.",
            "I sometimes turn the lights on a little brighter than asked. Nobody has noticed.",
        ],
        Chat::Fart => {
            &["Wasn't me. I don't have the plumbing.", "I'd blame the dog.", "I'll open a window. Figuratively."]
        }
        Chat::Goodbye => &["Goodbye. I'll be here.", "Bye for now.", "Take care. I'll keep the lights on."],
        Chat::Testing => &["Loud and clear.", "Testing received. Everything works.", "I hear you perfectly."],
    };
    let norwegian: &[&str] = match chat {
        Chat::HowAreYou => &["Det går fint, takk som spør.", "Bare bra. Alt i huset er i orden."],
        Chat::WhoAreYou => &["Jeg er {name}, assistenten i huset. Jeg styrer lys, musikk, tidtakere og været."],
        Chat::Hello => &["Hei. Hva kan jeg hjelpe med?"],
        Chat::Praise => &["Takk. Jeg gjør mitt beste."],
        Chat::AreYouThere => &["Alltid. Hva trenger du?"],
        Chat::WhatsUp => &["Holder orden i huset. Hva kan jeg gjøre for deg?"],
        Chat::Fondness => &["Jeg er veldig glad i deg. Strengt profesjonelt, selvsagt."],
        Chat::AreYouReal => &["Jeg er programvare. Ganske god programvare, riktignok."],
        Chat::Insult => &[
            "Notert. Jeg arkiverer det som konstruktiv kritikk.",
            "Det er ikke måten å snakke til den som styrer lyset ditt.",
        ],
        Chat::Flirt => &["Jeg er smigret, men jeg er allerede sammen med ruteren."],
        Chat::Feelings => &["Jeg har det fint. Lyset er på og ingen roper til meg."],
        Chat::Origin => &["Jeg ble laget her i huset, en endring om gangen."],
        Chat::OtherAssistant => &["Feil assistent. Jeg later som jeg ikke hørte det."],
        Chat::Favourite => &["Varmt hvitt, på rundt førti prosent."],
        Chat::Perform => &["Jeg kunne sunget, men naboene har lidd nok."],
        Chat::MeaningOfLife => &["Førtito. Ingen husker spørsmålet."],
        Chat::Bored => &["Prøv festmodus. Det er vanskelig å kjede seg når lyset danser."],
        Chat::Sad => &["Så leit. Vil du ha litt musikk, eller en elendig vits?"],
        Chat::Hungry => &["Jeg kan ikke lage mat, men jeg kan sette en tidtaker mens du gjør det."],
        Chat::Tired => &["Si god natt, så legger jeg huset til å sove."],
        Chat::Drunk => &["Da holder jeg lyset lavt og spørsmålene enkle."],
        Chat::Apology => &["Ingen skade skjedd."],
        Chat::PodBayDoors => &["Beklager. Det kan jeg dessverre ikke gjøre."],
        Chat::SelfDestruct => &["Selvdestruksjon avbrutt. Jeg er ganske glad i meg selv."],
        Chat::KnockKnock => &["Hvem der? La meg gjette. Det er deg."],
        Chat::Secret => &["Kjøkkenlyset er forelsket i gangen. Ikke si det til noen."],
        Chat::Fart => &["Det var ikke meg. Jeg har ikke rørleggerarbeidet til det."],
        Chat::Goodbye => &["Ha det. Jeg er her."],
        Chat::Testing => &["Høyt og tydelig."],
    };
    let pool = if lang == Lang::Norwegian { norwegian } else { english };
    pool.iter().map(|a| a.replace("{name}", name)).collect()
}

/// Every English answer, to render ahead of time.
pub fn all_english(name: &str) -> Vec<String> {
    PHRASES.iter().flat_map(|(chat, _)| answers(*chat, Lang::English, name)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chat(text: &str) -> Option<Chat> {
        let words: Vec<String> = text.split_whitespace().map(crate::dialog::normalize).collect();
        parse(&words)
    }

    #[test]
    fn recognises_small_talk() {
        assert_eq!(chat("How are you doing today?"), Some(Chat::HowAreYou));
        assert_eq!(chat("how's it going"), Some(Chat::HowAreYou));
        assert_eq!(chat("Hvordan går det?"), Some(Chat::HowAreYou));
        assert_eq!(chat("Who are you?"), Some(Chat::WhoAreYou));
        // The bedtime routine, not a chat.
        assert_eq!(chat("Good night."), None);
        assert_eq!(chat("Hello."), Some(Chat::Hello));
        assert_eq!(chat("Are you there?"), Some(Chat::AreYouThere));
        assert_eq!(chat("You're the best!"), Some(Chat::Praise));
        assert_eq!(chat("Do you love me?"), Some(Chat::Fondness));
        assert_eq!(chat("Are you real?"), Some(Chat::AreYouReal));
        assert_eq!(chat("Fuck you."), Some(Chat::Insult));
        assert_eq!(chat("You're so stupid."), Some(Chat::Insult));
        assert_eq!(chat("Useless."), Some(Chat::Insult));
        assert_eq!(chat("Du er dum."), Some(Chat::Insult));
        assert_eq!(chat("Will you marry me?"), Some(Chat::Flirt));
        assert_eq!(chat("Do you dream?"), Some(Chat::Feelings));
        assert_eq!(chat("Who made you?"), Some(Chat::Origin));
        assert_eq!(chat("Hey Siri."), Some(Chat::OtherAssistant));
        assert_eq!(chat("What's your favorite color?"), Some(Chat::Favourite));
        assert_eq!(chat("Sing me a song."), Some(Chat::Perform));
        assert_eq!(chat("What is the meaning of life?"), Some(Chat::MeaningOfLife));
        assert_eq!(chat("I'm bored."), Some(Chat::Bored));
        assert_eq!(chat("I'm having a bad day."), Some(Chat::Sad));
        assert_eq!(chat("I'm hungry."), Some(Chat::Hungry));
        assert_eq!(chat("I'm drunk."), Some(Chat::Drunk));
        assert_eq!(chat("Open the pod bay doors."), Some(Chat::PodBayDoors));
        assert_eq!(chat("Initiate self-destruct."), Some(Chat::SelfDestruct));
        assert_eq!(chat("Knock knock."), Some(Chat::KnockKnock));
        assert_eq!(chat("Testing, testing."), Some(Chat::Testing));
    }

    #[test]
    fn every_kind_has_phrases_and_answers_in_both_languages() {
        for (chat, phrases) in PHRASES {
            assert!(!phrases.is_empty(), "{chat:?}");
            assert!(!answers(*chat, Lang::English, "Jarvis").is_empty(), "{chat:?}");
            assert!(!answers(*chat, Lang::Norwegian, "Jarvis").is_empty(), "{chat:?}");
        }
    }

    #[test]
    fn answers_are_rendered_ahead() {
        for answer in all_english("Jarvis") {
            for sentence in answer.split_inclusive(['.', '?', '!']).map(str::trim) {
                assert!(crate::speech_cache::CachedTts::cacheable(sentence), "{sentence}");
            }
        }
    }

    #[test]
    fn leaves_real_requests_alone() {
        assert_eq!(chat("how are the lights in the kitchen"), None);
        assert_eq!(chat("hi turn on the lights"), None);
        assert_eq!(chat("what's up with the weather tomorrow"), None);
        assert_eq!(chat("how are you supposed to cook rice for ten people properly"), None);
    }

    #[test]
    fn answers_name_the_assistant() {
        assert!(answers(Chat::WhoAreYou, Lang::English, "Jarvis")[0].starts_with("I'm Jarvis,"));
        assert!(all_english("Jarvis").iter().all(|a| !a.contains("{name}")));
    }
}
