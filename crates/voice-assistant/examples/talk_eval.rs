//! How often the chat model's answers end with a question, on requests from the logs: each reply
//! asked back keeps a conversation going, with the TV too (11 of 14 on 27 Nov).
//! `cargo run --release -p voice-assistant --example talk_eval -- http://127.0.0.1:11434 gemma4:e2b-it-qat scripts/talk/requests.txt`
//! Requests in a block (a comment line starts the next) are one conversation: each is asked with the
//! answers before it, at most the engine's four, and after a lead-in ("I see, sir."), as live.
//! ROUNDS sets how often each request is asked (default 3), THREADS the model's threads (default 4).

use std::sync::atomic::AtomicBool;
use std::time::Duration;

use anyhow::{bail, Result};
use voice_assistant::lang::Lang;
use voice_assistant::llm::Turn;
use voice_assistant::talk::{leads_for, system_prompt, Talk, TalkContext};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [url, model, requests] = args.as_slice() else { bail!("usage: talk_eval <url> <model> <requests.txt>") };
    let env = |name: &str, default: u32| std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default);
    let (rounds, threads) = (env("ROUNDS", 3), env("THREADS", 4));
    let talk = Talk::new(url, model, threads, Duration::from_secs(120));
    let system = system_prompt(&TalkContext {
        name: "Jarvis",
        home: Some("Oslo"),
        room: Some("living room"),
        today: "Friday 27 November 2026",
        honorific: Some("sir"),
        ..Default::default()
    });
    let requests = std::fs::read_to_string(requests)?;
    let lines: Vec<&str> = requests.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    let (mut asked, mut questions) = (0, 0);
    for round in 0..rounds {
        let mut history: Vec<Turn> = Vec::new();
        for request in &lines {
            if request.starts_with('#') {
                history.clear();
                continue;
            }
            let leads = leads_for(request);
            let lead = format!("{}, sir.", leads[asked % leads.len()]);
            let stop = AtomicBool::new(false);
            let answered = talk.answer(&system, &history, request, Some(&lead), Lang::English, &stop, &mut |_| true)?;
            history.push(Turn { user: (*request).to_owned(), assistant: format!("{lead} {}", answered.raw) });
            if history.len() > 4 {
                history.remove(0);
            }
            let question = answered.said.trim_end().ends_with('?');
            asked += 1;
            questions += usize::from(question);
            println!("{round}\t{}\t{request}\t{}", if question { "?" } else { "." }, answered.said);
        }
    }
    println!("{questions} of {asked} answers end with a question");
    Ok(())
}
