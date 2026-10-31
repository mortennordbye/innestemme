//! What the language model decides for requests the rules miss, how often that is right, and how
//! long it takes.
//! `cargo run --release -p voice-assistant --example llm_eval -- http://127.0.0.1:11434/v1 qwen3:1.7b`
//!
//! Each case names what counts as right: `say`, `ignore`, or words that must all appear in the
//! decided skill (`Weather+bergen+tomorrow`); alternatives are separated by ` or `.

use std::time::{Duration, Instant};

use voice_assistant::lang::Lang;
use voice_assistant::llm::{system_prompt, Decision, Llm, Turn};

fn right(decision: &Decision, want: &str) -> bool {
    let got = format!("{decision:?}").to_lowercase();
    want.split(" or ").any(|alternative| match alternative {
        "say" => matches!(decision, Decision::Say(_)),
        "ignore" => matches!(decision, Decision::Ignore),
        skill => skill.split('+').all(|word| got.contains(&word.to_lowercase())),
    })
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [url, model] = args.as_slice() else { anyhow::bail!("usage: llm_eval <base url> <model>") };
    let llm = Llm::new(url, model, std::env::var("LLM_KEY").ok(), Duration::from_secs(60));
    let system = system_prompt("Homie", Some("Oslo"), Some("Living Room"), "Thursday 24 September 2026");
    let weather = [Turn {
        user: "what's the weather in Bergen?".into(),
        assistant: "It's 12 degrees and rain in Bergen, with a high of 14.".into(),
    }];
    let time = [Turn { user: "what time is it?".into(), assistant: "It's 18:04.".into() }];
    let phrased: &[(&str, &[Turn], &str)] = &[
        ("what's the capital of France?", &[], "say"),
        ("and what about tomorrow?", &weather, "Weather+bergen+tomorrow"),
        ("it's too dark in the kitchen", &[], "kitchen+on or kitchen+brighter"),
        ("put on some Beatles", &[], "Music+beatles"),
        ("I'm bored, cheer me up", &[], "Joke or say"),
        ("how many centimetres are there in an inch?", &[], "say"),
        ("shut everything off in the living room", &[], "Lights+living room+on: false"),
        ("is it going to rain in Oslo this weekend?", &[], "Weather+oslo"),
        ("Ingrid, did you remember to feed the cat?", &weather, "ignore"),
        ("hvem skrev Peer Gynt?", &[], "say"),
        ("could you lower the music a bit", &[], "Music+quieter or Music+volumeby(-"),
        ("put on that Rumours record from Fleetwood Mac", &[], "Music+rumours"),
        ("can you silence the speaker for a sec", &[], "Music+mute or Music+pause"),
        ("what day is it today?", &[], "say"),
        ("tell me when the eggs are done in 7 minutes", &[], "Timer+420"),
        ("don't let me forget the laundry in half an hour", &[], "Timer+1800"),
        ("give the pizza another five minutes", &[], "Timer+300"),
        ("we're out of coffee and oat milk", &[], "ShoppingList+coffee"),
        ("do we need anything from the store?", &[], "ShoppingList+read"),
        ("make it cosy in here", &[], "Scene+relax or LightLevel+warm"),
        ("I want to read in the bedroom, set the mood", &[], "Scene+read"),
        ("it's a bit too bright in here", &[], "LightLevel+dimmer"),
        ("should I run the dishwasher now or later tonight?", &[], "Power+cheapest"),
        ("is my girlfriend Ingrid back yet?", &[], "WhosHome+ingrid"),
        ("anything happening in the world today?", &[], "News"),
        ("I wonder who Edvard Munch was", &[], "Lookup+munch or say"),
        ("is Trondheim a long drive from here?", &[], "Distance+trondheim"),
        ("give the bedroom a romantic purple glow", &[], "LightLevel+purple"),
        ("can you get Netflix going on the telly", &[], "Tv+netflix"),
        ("throw the bus times up on the big screen", &[], "Tv+departures or Tv+transit"),
        ("I fancy watching Exit, it's on NRK I think", &[], "Tv+exit"),
        ("let's watch some Forrest Gump from the Plex server", &[], "Tv+forrest gump"),
        ("I've lost my phone somewhere in the flat", &[], "FindPhone"),
        ("alright, off to work, see you later", &[], "Routine+leaving or Routine+away"),
        ("everything fine back home?", &[], "House+status"),
        ("I'm knackered, heading to bed now", &[], "Routine+bed or Routine+night"),
        ("honey, daddy made it back from work", &[], "Routine+home"),
        ("time to hit the hay", &[], "Routine+bed or Routine+night"),
        ("seal the place up, nobody gets in", &[], "lockdown"),
        ("okay the threat is gone, back to normal", &[], "standdown or stand_down"),
        ("is the bedroom nice and toasty for tonight?", &[], "Climate+bedroom"),
        ("the air feels a bit stuffy in here, how's the moisture", &[], "Climate+humidity"),
    ];
    // What the rules did not understand on the Voice PE, 24-31 October 2026: mishearings, the room,
    // the TV, and the end of an answer heard back.
    let heard: &[(&str, &[Turn], &str)] = &[
        ("Oh, I'm home.", &[], "Routine+home"),
        ("Why is your name Darby?", &[], "say"),
        ("You were dumb.", &[], "say or ignore"),
        ("Yeah.", &time, "ignore or say"),
        ("Well", &[], "ignore or say"),
        ("I'm gonna", &[], "ignore or say"),
        ("And he should", &time, "ignore"),
        ("Hold on.", &[], "ignore or say"),
        ("Goodbye. Goodbye.", &[], "ignore or say"),
        ("United", &[], "ignore or say"),
        ("To the body.", &time, "ignore"),
        ("I will see Wandering also tomorrow.", &time, "ignore"),
        ("Do something.", &[], "say or Joke"),
        ("What's the clap shoe?", &[], "say or ignore"),
        ("Put the percentage at 70 percent.", &[], "Music+70 or LightLevel+70"),
        ("Read the sound.", &[], "Music or say"),
        ("Sir, what do you have to make?", &[], "say or ignore"),
        ("With a 40 year tariff for murdering his barn it 40 stop", &time, "ignore"),
        (
            "Those guys could really use the lung. And all the animals show them with you on the team weekend.",
            &time,
            "ignore",
        ),
        ("I need. I mean, Stan's grandma lost that mold, dude.", &time, "ignore"),
    ];
    llm.warm_up(&system)?;
    let mut times = Vec::new();
    for (group, cases) in [("phrased", phrased), ("heard", heard)] {
        let mut score = 0;
        for (request, history, want) in cases {
            let start = Instant::now();
            let lang = if request.starts_with("hvem") { Lang::Norwegian } else { Lang::English };
            let decision = llm.decide(&system, history, request, lang);
            let took = start.elapsed();
            times.push(took);
            let ok = decision.as_ref().is_ok_and(|d| right(d, want));
            score += usize::from(ok);
            let mark = if ok { "ok  " } else { "MISS" };
            println!("{mark} {:>5} ms  {request:<45} want {want:<30} got {decision:?}", took.as_millis());
        }
        println!("{group}: {score}/{} right", cases.len());
    }
    times.sort();
    let ms = |d: Duration| d.as_millis();
    println!(
        "{model}: p50 {} ms, p90 {} ms, mean {} ms",
        ms(times[times.len() / 2]),
        ms(times[times.len() * 9 / 10]),
        ms(times.iter().sum::<Duration>()) / times.len() as u128
    );
    Ok(())
}
