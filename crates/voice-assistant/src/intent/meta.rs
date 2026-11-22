//! What the rest of the assistant needs to know about each kind of request, in one exhaustive
//! match: a new [`Intent`] variant does not compile until its facts are written here.

use super::*;
use crate::ha;
use crate::house::{Indoors, Switch};
use crate::memory::MemoryCommand;
use crate::sensors::SensorQuery;
use crate::smalltalk::Chat;

pub const LEADS_WEATHER: &[&str] = &["Checking the forecast", "Let me look at the weather", "Consulting the skies"];
pub const LEADS_TRANSIT: &[&str] =
    &["Checking the departures", "Let me look up the next departures", "Checking the timetable"];
pub const LEADS_POWER: &[&str] =
    &["Checking the electricity prices", "Let me look at the power prices", "Checking the price of power"];
pub const LEADS_LIGHTS: &[&str] = &["Let me check the lights", "Checking the lights", "Looking around the house"];
pub const LEADS_HOME: &[&str] = &["Let me see who's in", "Checking who's home", "Let me take a head count"];
pub const LEADS_THINK: &[&str] = &["Let me think", "Give me a moment", "Let me consider that"];
/// Before the chat model's answer: a person's first words, by what was said. A question.
pub const LEADS_TALK_QUESTION: &[&str] = &["Good question", "Let me think about that", "Now there's a question"];
/// Something asked for: "tell me a poem", "can you give me a recipe".
pub const LEADS_TALK_ASK: &[&str] = &["Of course", "Certainly", "With pleasure"];
/// Something said: "I'm tired", "I want a holiday".
pub const LEADS_TALK: &[&str] = &["I see", "Ah", "Hmm"];
/// When the chat model is still quiet after the lead-in: plain, no honorific.
pub const TALK_FILLERS: &[&str] = &["Let me see", "Now then", "Bear with me"];
pub const LEADS_NEWS: &[&str] = &["Fetching the headlines", "Let me check the news", "Checking the papers"];
pub const LEADS_LOOKUP: &[&str] = &["Let me look that up", "One moment", "Let me find out"];
pub const LEADS_HOUSE: &[&str] = &["Let me check the house", "Checking the house", "Doing the rounds"];
pub const LEADS_TV: &[&str] = &["Let me find it", "Looking it up", "Finding it for you"];
/// Every lead-in, for rendering them ahead.
pub const LEADS: &[&[&str]] = &[
    LEADS_WEATHER,
    LEADS_TRANSIT,
    LEADS_POWER,
    LEADS_LIGHTS,
    LEADS_HOME,
    LEADS_THINK,
    LEADS_TALK,
    LEADS_TALK_QUESTION,
    LEADS_TALK_ASK,
    TALK_FILLERS,
    LEADS_NEWS,
    LEADS_LOOKUP,
    LEADS_HOUSE,
    LEADS_TV,
];

/// The facts about one parsed request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Meta {
    /// The `catalogue::SKILLS` id; `None` for requests the rules do not take.
    pub skill: Option<&'static str>,
    /// The request only reads (weather, departures, what is on), so the web page answers it for
    /// real; the rest would switch or play something.
    pub read_only: bool,
    /// Sentences said first when the answer is slow to get or to say. For [`Intent::Unknown`] only
    /// when a language model answers.
    pub lead: Option<&'static [&'static str]>,
    /// Nothing said after it could change the command, so a draft that is one is acted on, 0.4 s
    /// before the utterance's own end. Not resume: "play" is also the start of "play Careless
    /// Whisper"; not louder or quieter: "increase the sound" may go on "by 25%"; not the date,
    /// weather, power prices or the sun: "what day is it" may go on "tomorrow", the weather also
    /// "in Oslo"; not a scene or the lights' status, which may go on to name a room; not undo,
    /// which may go on "to 50 percent"; not "good morning", often followed by a question; not
    /// "what's next" or "close that", which may go on "on my calendar" or "tab". When the speaker
    /// goes on after a draft acted on, the longer utterance is taken as a second request.
    pub complete_at_once: bool,
    /// The result is heard (the music stops, the next song starts), so it needs no words, which
    /// would only arrive a second or two later from the speaker.
    pub heard_by_itself: bool,
}

impl Meta {
    const fn of(skill: &'static str) -> Meta {
        Meta { skill: Some(skill), read_only: false, lead: None, complete_at_once: false, heard_by_itself: false }
    }

    const fn reads(self) -> Meta {
        Meta { read_only: true, ..self }
    }

    const fn with_lead(self, lead: &'static [&'static str]) -> Meta {
        Meta { lead: Some(lead), ..self }
    }

    const fn done(self, complete_at_once: bool) -> Meta {
        Meta { complete_at_once, ..self }
    }

    const fn heard(self) -> Meta {
        Meta { heard_by_itself: true, ..self }
    }
}

impl Intent {
    /// The facts about this request. No wildcard arms: each variant states its own.
    pub fn meta(&self) -> Meta {
        match self {
            Intent::Weather { .. } => Meta::of("weather").reads().with_lead(LEADS_WEATHER),
            Intent::Joke => Meta::of("joke").reads().done(true),
            Intent::Time => Meta::of("time").reads().done(true),
            Intent::Date => Meta::of("time").reads(),
            Intent::Music(command) => music(command),
            Intent::Timer(command) => timer(command),
            Intent::Transit(_) => Meta::of("transit").reads().with_lead(LEADS_TRANSIT),
            // "Turn off the kitchen lights", not "turn off the lights" (may go on "in the kitchen").
            Intent::Lights { target, .. } => Meta::of("lights").done(names_light_last(target)),
            Intent::LightsStatus { .. } => Meta::of("lights_status").reads().with_lead(LEADS_LIGHTS),
            // A level or colour ends the request; "dim the kitchen" may go on "to 30 percent".
            Intent::LightLevel { level, target } => Meta::of("light_level").done(match level {
                LightLevel::Percent(_) | LightLevel::Color(_) => ha::names_something(target),
                LightLevel::Brighter | LightLevel::Dimmer | LightLevel::Warm | LightLevel::Cool => false,
            }),
            Intent::Power(_) => Meta::of("power").reads().with_lead(LEADS_POWER),
            // The question ends on "home", the name before it: "is Ingrid home", "who's home".
            Intent::WhosHome(_) => Meta::of("whos_home").reads().with_lead(LEADS_HOME).done(true),
            Intent::WhereIs { .. } => Meta::of("whos_home").reads(),
            Intent::Convert(_) => Meta::of("convert").reads(),
            Intent::Memory(command) => Meta { read_only: memory_reads(command), ..Meta::of("memory") },
            Intent::Briefing => Meta::of("briefing").reads(),
            // The headlines take no place, day or name.
            Intent::News => Meta::of("news").reads().with_lead(LEADS_NEWS).done(true),
            Intent::Lookup(_) => Meta::of("lookup").reads().with_lead(LEADS_LOOKUP),
            Intent::Calculate(_) => Meta::of("calc").reads(),
            Intent::Distance(_) => Meta::of("distance").reads(),
            // Reading the list is the whole request; adding may go on to name more items.
            Intent::ShoppingList(command) => Meta {
                read_only: list_reads(command),
                complete_at_once: matches!(command, ListCommand::Read),
                ..Meta::of("shopping")
            },
            Intent::Scene(_) => Meta::of("scene"),
            Intent::Script(_) => Meta::of("script"),
            Intent::Undo => Meta::of("undo"),
            Intent::Tv(command) => tv(command),
            Intent::House(command) => house(command),
            Intent::Routine(routine) => Meta::of(match routine {
                Routine::Lockdown | Routine::StandDown => "lockdown",
                Routine::OpenSource => "open_source",
                Routine::Leaving | Routine::Home | Routine::Bedtime => "routines",
            })
            .done(true),
            Intent::Party(_) => Meta::of("party").done(true),
            Intent::Thanks | Intent::Cancel => Meta::of("manners").reads().done(true),
            // Small talk is a whole short phrase; a bare "hey" is often a request still to come.
            Intent::SmallTalk(chat) => Meta::of("smalltalk").reads().done(*chat != Chat::Hello),
            Intent::Unknown => Meta { skill: None, ..Meta::of("") }.with_lead(LEADS_THINK),
        }
    }
}

fn music(command: &MusicCommand) -> Meta {
    use MusicCommand::*;
    let meta = Meta::of("music");
    match command {
        Pause | Stop | Mute(_) | Next | Previous => meta.heard().done(true),
        Resume | Liked | Playlist(_) | Song(_) | Artist(_) | Album(_) => meta.heard(),
        NowPlaying | Shuffle(_) | Repeat(_) | VolumeBy(_) | VolumeTo(_) => meta.done(true),
        Louder | Quieter | UpNext | InRooms(..) | Queue(..) | MoveTo(..) => meta,
        // "Pause the music" is acted on at once even though it may go on "in the kitchen": the
        // command is the most common there is, and one that turns out to name another room is
        // undone by the engine. A room said ends it too.
        InRoom(_, inner) => meta.done(music(inner).complete_at_once),
    }
}

fn tv(command: &TvCommand) -> Meta {
    let tv = Meta::of("tv");
    match command {
        TvCommand::Pause | TvCommand::Resume => tv.heard().done(true),
        // There is one TV, so nothing said after "is the TV on" or "TV volume 20" names another.
        TvCommand::Power(_)
        | TvCommand::Mute(_)
        | TvCommand::Louder
        | TvCommand::Quieter
        | TvCommand::Status
        | TvCommand::Volume(_) => tv.done(true),
        // "Open YouTube" may go on "Kids".
        TvCommand::App(_) => tv,
        TvCommand::Nrk(_) | TvCommand::Plex(_) => tv.with_lead(LEADS_TV),
        TvCommand::Show(_) => Meta::of("tv_screen").done(true),
        TvCommand::Hide => Meta::of("tv_screen"),
    }
}

fn house(command: &HouseCommand) -> Meta {
    let status = |switch: &Switch| match switch {
        Switch::Status => true,
        Switch::On | Switch::Off => false,
    };
    match command {
        // The whole house, and one guest network: nothing to add.
        HouseCommand::Status => Meta::of("house_status").reads().with_lead(LEADS_HOUSE).done(true),
        HouseCommand::Sun(_) => Meta::of("sun").reads(),
        HouseCommand::FindPhone(_) => Meta::of("phone").done(true),
        HouseCommand::PhoneBattery(_) => Meta::of("phone").reads(),
        HouseCommand::GuestWifi(switch) => Meta { read_only: status(switch), ..Meta::of("guest_wifi").done(true) },
        HouseCommand::Camera(switch) => Meta { read_only: status(switch), ..Meta::of("camera") },
        // Disarming by voice is refused, so asking for it only reads.
        HouseCommand::Alarm(switch) => Meta {
            read_only: match switch {
                Switch::Status | Switch::Off => true,
                Switch::On => false,
            },
            ..Meta::of("alarm")
        },
        HouseCommand::Climate(indoors, _) => Meta::of("climate").reads().done(match indoors {
            Indoors::Room(_) => true,
            Indoors::Everywhere | Indoors::Here => false,
        }),
        HouseCommand::Sensors(SensorQuery::Battery(_)) => Meta::of("batteries").reads(),
        HouseCommand::Sensors(SensorQuery::Leaks) => Meta::of("leaks").reads().done(true),
    }
}

fn timer(command: &TimerCommand) -> Meta {
    let of = |alarm: bool| Meta::of(if alarm { "wake_alarm" } else { "timer" });
    match command {
        // A length is the end of the request: "set a timer for 5 minutes", "stop the timer".
        TimerCommand::Start { seconds, .. } => of(false).done(seconds.is_some()),
        TimerCommand::Remind { .. } | TimerCommand::Add { .. } => of(false),
        TimerCommand::Pause(_) | TimerCommand::Resume(_) => of(false).done(true),
        TimerCommand::Cancel(which) | TimerCommand::Remaining(which) => of(which.of == timer::Of::Alarms).done(true),
        TimerCommand::Alarm { .. } | TimerCommand::MoveAlarm(_) => of(true),
        TimerCommand::Snooze | TimerCommand::StopAlarm => of(true).done(true),
    }
}

fn memory_reads(command: &MemoryCommand) -> bool {
    match command {
        MemoryCommand::Recall(_) | MemoryCommand::List => true,
        MemoryCommand::Remember(_) | MemoryCommand::Forget(_) | MemoryCommand::ForgetAll => false,
    }
}

fn list_reads(command: &ListCommand) -> bool {
    match command {
        ListCommand::Read => true,
        ListCommand::Add(_) | ListCommand::Remove(_) | ListCommand::Clear | ListCommand::Has(_) => false,
    }
}

/// Whether a light request ends on what it switches: "the kitchen lights", "the kitchen".
fn names_light_last(target: &str) -> bool {
    let words: Vec<String> = target.split_whitespace().map(normalize).collect();
    let last = words.last().map(String::as_str).unwrap_or_default();
    ha::names_something(target) && (["light", "lights", "lamp", "lamps"].contains(&last) || ha::names_only_room(&words))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heard_by_itself() {
        let heard = |text: &str| parse(text).meta().heard_by_itself;
        assert!(heard("pause the music"));
        assert!(heard("next song"));
        assert!(!heard("turn it up"));
        assert!(heard("play some music"));
    }

    #[test]
    fn complete_at_once() {
        let complete = |text: &str| parse(text).meta().complete_at_once;
        // At once, though it may go on "in the kitchen": then the engine undoes it.
        assert!(complete("pause the music"));
        assert!(complete("next song"));
        assert!(complete("volume 30"));
        assert!(complete("what's playing"));
        assert!(complete("pause the music in the kitchen"));
        assert!(complete("next song in the bedroom"));
        assert!(!complete("play jazz in the kitchen"));
        assert!(complete("what time is it"));
        assert!(!complete("what day is it"));
        assert!(!complete("turn off the lights"));
        assert!(!complete("play"));
        assert!(complete("turn off the kitchen lights"));
        assert!(complete("turn on the kitchen"));
        assert!(!complete("turn on the lights in"));
        assert!(complete("set the living room to 30 percent"));
        assert!(!complete("dim the living room"));
        assert!(complete("put the weather on the tv"));
        assert!(complete("where is my phone"));
        assert!(complete("what's the temperature in the bedroom"));
        assert!(!complete("what's the temperature"));
        assert!(complete("good night"));
        assert!(complete("initiate lockdown"));
        assert!(complete("how you doing"));
        assert!(!complete("hey"));
    }

    #[test]
    fn complete_at_once_questions_without_a_tail() {
        let complete = |text: &str| parse(text).meta().complete_at_once;
        assert!(complete("is the TV on?"));
        assert!(complete("set the tv volume to 20"));
        assert!(complete("what's on the shopping list"));
        assert!(complete("who's home?"));
        assert!(complete("is Ingrid home?"));
        assert!(complete("any leaks?"));
        assert!(complete("status report"));
        assert!(complete("turn on the guest wifi"));
        assert!(complete("is the guest network on?"));
        assert!(complete("what's the latest news?"));
        // These may go on: "in Oslo", "tomorrow", "in the kitchen", "milk and eggs", "to 50 percent".
        assert!(!complete("what's the weather"));
        assert!(!complete("what's the power price"));
        assert!(!complete("when is sunset"));
        assert!(!complete("which lights are on?"));
        assert!(!complete("add milk to the shopping list"));
        assert!(!complete("undo"));
        assert!(!complete("open youtube on the tv"));
        // "... what's the weather", "... on my calendar", "... tab".
        assert!(!complete("good morning"));
        assert!(!complete("what's next?"));
        assert!(!complete("close that"));
    }
}
