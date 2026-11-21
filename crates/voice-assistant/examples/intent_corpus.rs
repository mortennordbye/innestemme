//! What the text rules make of each heard text, for a golden comparison of two builds
//! (`scripts/corpus/compare.sh`): the request as the worker passes it on (the name stripped from
//! its start and its end), the `Debug` form of `intent::parse`, of `parse_without_notes` where it
//! differs, of each part `split_commands` finds, the catalogue skill, and whether a draft that is
//! the request is acted on at once.
//!
//! cargo run -p voice-assistant --example intent_corpus -- [--wake Jarvis] [--catalogue] [file]
//!
//! Input lines are `lang<TAB>text` (`English` or `Norwegian`, as the "heard" log line has it) or a
//! bare text, which is English; from `file`, else stdin. `--catalogue` adds every catalogue example.

use std::io::{BufRead, Write};

use voice_assistant::catalogue::{skill_of, SKILLS};
use voice_assistant::dialog::WakeWord;
use voice_assistant::intent;

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let (mut wake, mut catalogue, mut file) = ("Jarvis".to_owned(), false, None);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--wake" => wake = args.next().ok_or_else(|| anyhow::anyhow!("--wake needs a name"))?,
            "--catalogue" => catalogue = true,
            _ => file = Some(arg),
        }
    }
    let wake = WakeWord::new(&wake);
    let input: Box<dyn BufRead> = match &file {
        Some(path) => Box::new(std::io::BufReader::new(std::fs::File::open(path)?)),
        None => Box::new(std::io::stdin().lock()),
    };
    let mut lines: Vec<(String, String)> = Vec::new();
    for line in input.lines() {
        let line = line?;
        let (lang, text) = match line.split_once('\t') {
            Some((lang, text)) => (lang.to_owned(), text.to_owned()),
            None => ("English".to_owned(), line),
        };
        if !text.trim().is_empty() {
            lines.push((lang, text));
        }
    }
    if catalogue {
        for skill in SKILLS {
            lines.extend(skill.examples.iter().map(|e| ("English".to_owned(), (*e).to_owned())));
            lines.extend(skill.norwegian.iter().map(|e| ("Norwegian".to_owned(), (*e).to_owned())));
        }
    }
    let mut out = std::io::BufWriter::new(std::io::stdout().lock());
    for (lang, text) in lines {
        // As `Worker::heard`: a text without the name is taken whole in a follow-up window.
        let (request, addressed) = match wake.strip(&text) {
            Some(rest) => (rest, true),
            None => (text.clone(), false),
        };
        let request = wake.drop_trailing(&request);
        let parsed = intent::parse(&request);
        writeln!(out, "{lang}\t{text}")?;
        writeln!(out, "  request ({}): {request}", if addressed { "named" } else { "unnamed" })?;
        writeln!(out, "  parse: {parsed:?}")?;
        let without = intent::parse_without_notes(&request);
        if without != parsed {
            writeln!(out, "  without notes: {without:?}")?;
        }
        if let Some(parts) = intent::split_commands(&request) {
            for part in parts {
                writeln!(out, "  part: {part} => {:?}", intent::parse(&part))?;
            }
        }
        writeln!(out, "  skill: {}", skill_of(&parsed).map_or("-", |s| s.id))?;
        if parsed.meta().complete_at_once {
            writeln!(out, "  at once")?;
        }
    }
    Ok(())
}
