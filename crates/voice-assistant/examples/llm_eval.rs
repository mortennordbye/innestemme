//! What the language model decides for requests the rules miss, and how long it takes.
//! `cargo run --release -p voice-assistant --example llm_eval -- http://127.0.0.1:11434/v1 qwen3:1.7b`

use std::time::{Duration, Instant};

use voice_assistant::lang::Lang;
use voice_assistant::llm::{system_prompt, Llm, Turn};

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [url, model] = args.as_slice() else { anyhow::bail!("usage: llm_eval <base url> <model>") };
    let llm = Llm::new(url, model, std::env::var("LLM_KEY").ok(), Duration::from_secs(60));
    let system = system_prompt("Homie", Some("Oslo"), Some("Living Room"), "Thursday 24 September 2026");
    let weather = [Turn {
        user: "what's the weather in Bergen?".into(),
        assistant: "It's 12 degrees and rain in Bergen, with a high of 14.".into(),
    }];
    let cases: [(&str, &[Turn], &str); 25] = [
        ("what's the capital of France?", &[], "say"),
        ("and what about tomorrow?", &weather, "weather Bergen tomorrow"),
        ("it's too dark in the kitchen", &[], "lights kitchen on"),
        ("put on some Beatles", &[], "music play The Beatles"),
        ("I'm bored, cheer me up", &[], "joke or say"),
        ("how many centimetres are there in an inch?", &[], "say"),
        ("shut everything off in the living room", &[], "lights living room off"),
        ("is it going to rain in Oslo this weekend?", &[], "weather Oslo"),
        ("Ingrid, did you remember to feed the cat?", &weather, "ignore"),
        ("hvem skrev Peer Gynt?", &[], "say (Norwegian)"),
        ("could you lower the music a bit", &[], "music quieter"),
        ("what day is it today?", &[], "say"),
        ("tell me when the eggs are done in 7 minutes", &[], "timer start 7 min eggs"),
        ("don't let me forget the laundry in half an hour", &[], "timer remind 30 min laundry"),
        ("give the pizza another five minutes", &[], "timer add 5 min pizza"),
        ("we're out of coffee and oat milk", &[], "shopping_list add coffee, oat milk"),
        ("do we need anything from the store?", &[], "shopping_list read"),
        ("make it cosy in here", &[], "scene relax"),
        ("I want to read in the bedroom, set the mood", &[], "scene read bedroom"),
        ("it's a bit too bright in here", &[], "light_settings dimmer"),
        ("should I run the dishwasher now or later tonight?", &[], "electricity_price cheapest tonight"),
        ("is my girlfriend Ingrid back yet?", &[], "who_is_home Ingrid"),
        ("anything happening in the world today?", &[], "news"),
        ("I wonder who Edvard Munch was", &[], "lookup Edvard Munch"),
        ("give the bedroom a romantic purple glow", &[], "light_settings bedroom purple"),
    ];
    llm.warm_up(&system)?;
    let mut total = Duration::ZERO;
    for (request, history, expected) in cases {
        let start = Instant::now();
        let lang = if request.starts_with("hvem") { Lang::Norwegian } else { Lang::English };
        let decision = llm.decide(&system, history, request, lang);
        let took = start.elapsed();
        total += took;
        println!("{:>5} ms  {request:<45} want {expected:<25} got {decision:?}", took.as_millis());
    }
    println!("mean {} ms", total.as_millis() / cases.len() as u128);
    Ok(())
}
