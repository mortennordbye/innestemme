//! Everything the assistant understands, for people: each skill with what it does, where its data
//! comes from, and phrases that reach it. The engine's web page shows it. Every example is parsed
//! in the tests, so the catalogue cannot promise a phrase the rules do not take.

use serde::Serialize;

use crate::intent::Intent;

#[derive(Debug, Serialize)]
pub struct Skill {
    pub id: &'static str,
    pub name: &'static str,
    /// "Home", "Information" or "Everyday", for grouping on the page.
    pub group: &'static str,
    pub icon: &'static str,
    /// What happens, in a sentence.
    pub does: &'static str,
    /// Where the data comes from or what is called.
    pub source: &'static str,
    /// Home Assistant services or HTTP endpoints it calls.
    pub calls: &'static [&'static str],
    pub examples: &'static [&'static str],
    pub norwegian: &'static [&'static str],
}

pub const SKILLS: &[Skill] = &[
    Skill {
        id: "lights",
        name: "Lights",
        group: "Home",
        icon: "💡",
        does: "Switches lights by room or name. Without a room, the room the device is in. \"Them\" means the lights used last.",
        source: "Home Assistant",
        calls: &["light.turn_on", "light.turn_off"],
        examples: &["Turn off the living room lights", "Switch on the kitchen light", "Turn them back on"],
        norwegian: &["Skru av lyset i stua", "Slå på lyset på kjøkkenet"],
    },
    Skill {
        id: "light_level",
        name: "Brightness and colour",
        group: "Home",
        icon: "🎨",
        does: "Dims, brightens or colours lights.",
        source: "Home Assistant",
        calls: &["light.turn_on (brightness_pct, color_name, color_temp)"],
        examples: &["Dim the living room to 30 percent", "Make the bedroom lights red", "Brighter"],
        norwegian: &["Demp stua til 30 prosent"],
    },
    Skill {
        id: "lights_status",
        name: "What's on",
        group: "Home",
        icon: "🔎",
        does: "Tells which lights are on, or whether one is.",
        source: "Home Assistant states",
        calls: &["GET /api/states"],
        examples: &["Which lights are on?", "Is the kitchen light on?"],
        norwegian: &["Hvilke lys er på?"],
    },
    Skill {
        id: "scene",
        name: "Scenes",
        group: "Home",
        icon: "🎬",
        does: "Activates a Home Assistant scene, by room and name.",
        source: "Home Assistant scenes",
        calls: &["scene.turn_on"],
        examples: &["Set the living room to relax", "Activate the reading scene"],
        norwegian: &["Aktiver lesescenen"],
    },
    Skill {
        id: "script",
        name: "Scripts",
        group: "Home",
        icon: "📜",
        does: "Runs a Home Assistant script by its name.",
        source: "Home Assistant scripts",
        calls: &["script.turn_on"],
        examples: &["Run movie time"],
        norwegian: &["Kjør filmkveld"],
    },
    Skill {
        id: "undo",
        name: "Undo",
        group: "Home",
        icon: "↩️",
        does: "Reverses the last lights or shopping list change.",
        source: "The assistant's memory (two minutes)",
        calls: &[],
        examples: &["Undo", "Reverse that", "Switch it back"],
        norwegian: &["Angre"],
    },
    Skill {
        id: "weather",
        name: "Weather",
        group: "Information",
        icon: "🌦️",
        does: "Today's or tomorrow's forecast, at home or any named place.",
        source: "MET Norway Locationforecast; places through Open-Meteo geocoding",
        calls: &["api.met.no/weatherapi/locationforecast/2.0", "geocoding-api.open-meteo.com"],
        examples: &["What's the weather tomorrow?", "Will it rain in Bergen tomorrow?", "How's the weather?"],
        norwegian: &["Hvordan blir været i morgen?"],
    },
    Skill {
        id: "transit",
        name: "Departures",
        group: "Information",
        icon: "🚌",
        does: "The next departures from the stops near home, by mode and destination.",
        source: "Entur journey planner",
        calls: &["api.entur.io/journey-planner/v3/graphql"],
        examples: &["When's the next bus?", "When does the next metro leave?"],
        norwegian: &["Når går neste buss?"],
    },
    Skill {
        id: "power",
        name: "Electricity prices",
        group: "Information",
        icon: "⚡",
        does: "The price now, and the cheapest or dearest hours today or tomorrow, for the home's price area.",
        source: "hvakosterstrommen.no",
        calls: &["hvakosterstrommen.no/api/v1/prices"],
        examples: &["What's the electricity price now?", "When is electricity cheapest tomorrow?"],
        norwegian: &["Hva koster strømmen nå?"],
    },
    Skill {
        id: "music",
        name: "Music",
        group: "Everyday",
        icon: "🎵",
        does: "Plays liked songs, playlists or a song search, and controls playback and volume.",
        source: "Music Assistant through Home Assistant",
        calls: &["music_assistant.search", "music_assistant.play_media", "media_player.*"],
        examples: &["Play my liked songs", "Play Hello by Adele", "Pause the music", "Next song", "Turn the music up"],
        norwegian: &["Spill musikk", "Neste sang"],
    },
    Skill {
        id: "timer",
        name: "Timers and reminders",
        group: "Everyday",
        icon: "⏱️",
        does: "Named timers and spoken reminders. A Voice PE counts down on its LED ring.",
        source: "Kept by the assistant",
        calls: &[],
        examples: &[
            "Set a timer for 10 minutes",
            "Remind me in 20 minutes to check the oven",
            "How much time is left?",
            "Cancel the timer",
        ],
        norwegian: &["Sett en timer på ti minutter"],
    },
    Skill {
        id: "shopping",
        name: "Shopping list",
        group: "Everyday",
        icon: "🛒",
        does: "Adds, removes, reads or clears the shopping list.",
        source: "Home Assistant to-do list",
        calls: &["todo.add_item", "todo.update_item", "todo.get_items"],
        examples: &["Add milk and eggs to the shopping list", "What's on the shopping list?", "Clear the shopping list"],
        norwegian: &["Legg melk på handlelista"],
    },
    Skill {
        id: "whos_home",
        name: "Who's home",
        group: "Home",
        icon: "🏠",
        does: "Says who is home, or whether one person is.",
        source: "Home Assistant people",
        calls: &["GET /api/states (person.*)"],
        examples: &["Who's home?"],
        norwegian: &["Hvem er hjemme?"],
    },
    Skill {
        id: "briefing",
        name: "Morning briefing",
        group: "Information",
        icon: "☀️",
        does: "The time, the weather at home, the shopping list and the latest headlines in a few sentences.",
        source: "Weather, shopping list and news together",
        calls: &[],
        examples: &["Good morning", "Brief me"],
        norwegian: &["God morgen"],
    },
    Skill {
        id: "time",
        name: "Time",
        group: "Information",
        icon: "🕒",
        does: "The time.",
        source: "The clock",
        calls: &[],
        examples: &["What time is it?"],
        norwegian: &["Hva er klokka?"],
    },
    Skill {
        id: "joke",
        name: "Jokes",
        group: "Everyday",
        icon: "😄",
        does: "A dad joke.",
        source: "icanhazdadjoke.com, with built-in ones as fallback",
        calls: &["icanhazdadjoke.com"],
        examples: &["Tell me a joke"],
        norwegian: &["Fortell en vits"],
    },
    Skill {
        id: "manners",
        name: "Thanks and never mind",
        group: "Everyday",
        icon: "🙏",
        does: "Answers thanks, or ends the conversation without a word.",
        source: "Built in",
        calls: &[],
        examples: &["Thank you", "Never mind"],
        norwegian: &["Takk", "Glem det"],
    },
    Skill {
        id: "news",
        name: "News",
        group: "Everyday",
        icon: "📰",
        does: "The three latest headlines; the morning briefing ends with them too.",
        source: "BBC News (English) and NRK (Norwegian) RSS feeds, or the `news-feed` settings",
        calls: &["feeds.bbci.co.uk", "nrk.no"],
        examples: &["What's the latest news?", "Read me the headlines"],
        norwegian: &["Hva er nyhetene?"],
    },
    Skill {
        id: "smalltalk",
        name: "Small talk",
        group: "Everyday",
        icon: "💬",
        does: "Answers how are you, who are you, hello and good night at once, without the language model.",
        source: "Built in",
        calls: &[],
        examples: &["How are you doing today?", "Who are you?", "Good night"],
        norwegian: &["Hvordan går det?", "Hvem er du?", "God natt"],
    },
];

/// The skill a parsed request belongs to; `None` for requests the rules do not take.
pub fn skill_of(intent: &Intent) -> Option<&'static Skill> {
    let id = match intent {
        Intent::Lights { .. } => "lights",
        Intent::LightLevel { .. } => "light_level",
        Intent::LightsStatus { .. } => "lights_status",
        Intent::Scene(_) => "scene",
        Intent::Script(_) => "script",
        Intent::Undo => "undo",
        Intent::Weather { .. } => "weather",
        Intent::Transit(_) => "transit",
        Intent::Power(_) => "power",
        Intent::Music(_) => "music",
        Intent::Timer(_) => "timer",
        Intent::ShoppingList(_) => "shopping",
        Intent::WhosHome(_) => "whos_home",
        Intent::Briefing => "briefing",
        Intent::News => "news",
        Intent::Time => "time",
        Intent::Joke => "joke",
        Intent::Thanks | Intent::Cancel => "manners",
        Intent::SmallTalk(_) => "smalltalk",
        Intent::Unknown => return None,
    };
    SKILLS.iter().find(|s| s.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::intent::parse;

    #[test]
    fn every_example_reaches_its_skill() {
        let mut wrong = Vec::new();
        for skill in SKILLS {
            for phrase in skill.examples.iter().chain(skill.norwegian) {
                let intent = parse(phrase);
                if skill_of(&intent).map(|s| s.id) != Some(skill.id) {
                    wrong.push(format!("{} -> {intent:?} (want {})", phrase, skill.id));
                }
            }
        }
        assert!(wrong.is_empty(), "{wrong:#?}");
    }

    #[test]
    fn every_skill_is_listed_once() {
        let mut ids: Vec<_> = SKILLS.iter().map(|s| s.id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), SKILLS.len());
    }
}
