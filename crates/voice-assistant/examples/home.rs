//! Runs one home request (shopping list, scene, script, lights on, light level, who's home,
//! electricity prices, house status, sun, phones, TV) against the real Home
//! Assistant, without audio: `cargo run -p voice-assistant --example home -- "what's on the shopping list"`.
//! Reads HA_URL and HA_TOKEN from the environment (`set -a; . ./.env`). Scenes and scripts are only
//! looked up unless RUN=1 (light levels, phones and the TV too); shopping list changes are real. VOICE_ROOM plays the satellite's room.
//! "Initiate lockdown" with RUN=1 pulses the lights for BLINK_SECONDS (10, at most one alert of 15), then stands down.
//! "Show ... on the TV" with RUN=1 and VOICE_SCREEN_URL opens that page in the TV's browser.

use voice_assistant::ha::{self, HomeAssistant};
use voice_assistant::house::{self, HouseCommand};
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
        Intent::House(HouseCommand::Status) => {
            let alerts = voice_assistant::alerts::active(&ha)?;
            let lights: Vec<ha::Light> = ha.lights_on()?.into_iter().filter(|l| !l.status_led()).collect();
            println!("{}", house::status_answer(&alerts, &lights, &house::snapshot(&ha)?, lang));
        }
        Intent::House(HouseCommand::Sun(event)) => {
            println!("{}", house::sun_answer(&ha.state("sun.sun")?, event, lang)?)
        }
        Intent::House(HouseCommand::Climate(place, reading)) => {
            let rooms = house::climate_readings(&ha)?;
            println!("{rooms:?}");
            let here = std::env::var("VOICE_ROOM").ok();
            println!("{}", house::climate_answer(&rooms, &place, reading, here.as_deref(), lang));
        }
        Intent::House(HouseCommand::PhoneBattery(name)) => {
            let phones = house::phones(&ha)?;
            println!("{}", house::battery_answer(&house::pick_phones(&phones, name.as_deref()), name.as_deref(), lang));
        }
        Intent::House(HouseCommand::FindPhone(name)) => {
            let phones = house::phones(&ha)?;
            let picked = house::pick_phones(&phones, name.as_deref());
            println!("{name:?} -> {picked:?}");
            if run {
                for phone in &picked {
                    house::ring(&ha, phone, "Homie")?;
                }
                println!("{}", house::rang_answer(&picked, name.as_deref(), lang));
            }
        }
        Intent::House(other) => {
            for (domain, word) in
                [("switch", "guest"), ("switch", "privacy"), ("camera", ""), ("alarm_control_panel", "")]
            {
                println!("{other:?}: {domain} {word:?} -> {:?}", house::entities(&ha, domain, word)?);
            }
        }
        Intent::Routine(house::Routine::Lockdown) if run => {
            // The engine's lockdown lights and pulse, without the music; stood down after BLINK_SECONDS.
            let red = house::lockdown(&ha)?;
            let targets = house::alarm_targets(&ha, &red)?;
            println!("{} red lights, pulsing {targets:?}", red.len());
            house::alarm_pulse(&ha, &targets)?;
            let seconds = std::env::var("BLINK_SECONDS").ok().and_then(|s| s.parse().ok()).unwrap_or(10);
            std::thread::sleep(std::time::Duration::from_secs(seconds));
            println!("stood down: {}", house::stand_down(&ha)?);
        }
        Intent::Tv(command) => {
            println!("{command:?}: TVs on {:?}", voice_assistant::tv::on_tvs(&ha)?);
            let plex = std::env::var("VOICE_PLEX_URL").ok().zip(std::env::var("VOICE_PLEX_TOKEN").ok());
            let plex = plex.map(|(url, token)| voice_assistant::tv::Plex { url, token });
            let mut tv = voice_assistant::tv::Tv::new(None).with_plex(plex);
            match (&command, std::env::var("VOICE_SCREEN_URL")) {
                // The engine puts the data on its screen; here only the page is opened.
                (voice_assistant::tv::TvCommand::Show(_), Ok(url)) if run => tv.open_page(&ha, &url)?,
                _ if run => println!("{}", tv.run(Some(&ha), &command, lang)),
                _ => {}
            }
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
