//! Prints the language model's system prompt and tools as JSON, to count their tokens against a
//! server: `cargo run -p voice-assistant --example dump_prompt > prompt.json`.

fn main() {
    let system =
        voice_assistant::llm::system_prompt("Homie", Some("Oslo"), Some("Living Room"), "Saturday 26 September 2026");
    println!("{}", serde_json::json!({ "system": system, "tools": voice_assistant::llm::tools() }));
}
