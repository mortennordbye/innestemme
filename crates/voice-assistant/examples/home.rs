//! Runs one home request (shopping list, scene, script, lights on, light level, who's home,
//! electricity prices) against the real Home
//! Assistant, without audio: `cargo run -p voice-assistant --example home -- "what's on the shopping list"`.
//! Reads HA_URL and HA_TOKEN from the environment (`set -a; . ./.env`). Scenes and scripts are only
//! looked up unless RUN=1 (light levels too); shopping list changes are real. VOICE_ROOM plays the satellite's room.

use voice_assistant::ha::{self, HomeAssistant};
use voice_assistant::intent::{self, Intent};
use voice_assistant::lang::Lang;
use voice_assistant::shopping::ShoppingList;

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_env_filter("info").init();
    let text = std::env::args().skip(1).collect::<Vec<_>>().join(" ");
    let lang = if std::env::var_os("NORWEGIAN").is_some() { Lang::Norwegian } else { Lang::English };
    let mut ha = HomeAssistant::new(&std::env::var("HA_URL")?, &std::env::var("HA_TOKEN")?);
    let room = std::env::var("VOICE_ROOM").ok();
    let run = std::env::var_os("RUN").is_some();
    let start = std::time::Instant::now();
    match intent::parse(&text) {
        Intent::ShoppingList(command) => {
            let (answer, change) = ShoppingList::default().run(&ha, &command, lang)?;
            println!("{command:?} -> {answer:?} change {change:?}");
        }
        Intent::Scene(request) => {
            let scenes = ha.scenes()?.to_vec();
            let found = ha::resolve_scene(&scenes, &request, room.as_deref());
            println!("scene {request:?} (room {room:?}) -> {found:?}");
            if let (true, ha::SceneMatch::Found(scene)) = (run, &found) {
                ha.activate(scene)?;
            }
        }
        Intent::Script(request) => {
            let scripts = ha.scripts()?.to_vec();
            let found = ha::resolve_script(&scripts, &request);
            println!("script {request:?} -> {found:?}");
            if let (true, Some(script)) = (run, found) {
                ha.activate(script)?;
            }
        }
        Intent::LightLevel { level, target } => {
            let found = ha.find(&target)?;
            println!("{level:?} for {target:?} -> {found:?}");
            if let (true, Some(found)) = (run, &found) {
                ha.set_level(found, &level)?;
            }
        }
        Intent::WhosHome(name) => println!("{name:?}: {:?}", ha.people()?),
        Intent::Power(query) => {
            let area = std::env::var("VOICE_PRICE_AREA").unwrap_or_else(|_| "NO1".into());
            let answer = voice_assistant::power::Power::new("innestemme-example", &area).answer(query, lang)?;
            println!("{query:?} in {area} -> {answer:?}");
        }
        Intent::LightsStatus { target } => {
            let on: Vec<String> = ha.lights_on()?.into_iter().map(|l| format!("{} ({})", l.name, l.area)).collect();
            println!("{target:?}: on {on:?}");
        }
        other => {
            let scripts = ha.scripts()?.to_vec();
            let scenes = ha.scenes()?.to_vec();
            let exact = ha::exact(&scripts, &text).or_else(|| ha::exact(&scenes, &text));
            println!("{other:?}; by exact name: {exact:?}");
        }
    }
    println!("took {:?}", start.elapsed());
    Ok(())
}
