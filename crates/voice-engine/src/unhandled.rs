//! Requests the assistant could not handle, one JSON object per line (`--unhandled-log`), to see
//! what people ask for that no skill does yet. The web page lists the latest ones.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

use serde_json::json;
use tracing::warn;

/// Why a request counts as unhandled.
#[derive(Debug, Clone, Copy)]
pub enum Reason {
    /// Woken, but the words matched no skill and there is no language model.
    NotUnderstood,
    /// No skill matched; the rules said what they can do instead.
    Unknown,
    /// A skill answered with an apology ("Sorry, I couldn't find ...").
    Apologised,
    /// Said in a follow-up without the wake word and matched no skill, so it was ignored.
    FollowUpIgnored,
    /// No skill matched and the language model answered in its own words.
    LanguageModel,
    /// The language model decided the words were not meant for the assistant.
    LanguageModelIgnored,
}

impl Reason {
    fn as_str(self) -> &'static str {
        match self {
            Reason::NotUnderstood => "not_understood",
            Reason::Unknown => "unknown",
            Reason::Apologised => "apologised",
            Reason::FollowUpIgnored => "follow_up_ignored",
            Reason::LanguageModel => "language_model",
            Reason::LanguageModelIgnored => "language_model_ignored",
        }
    }
}

pub struct UnhandledLog {
    path: PathBuf,
    /// One writer at a time, so lines are never interleaved.
    lock: Mutex<()>,
}

impl UnhandledLog {
    pub fn new(path: PathBuf) -> Self {
        Self { path, lock: Mutex::new(()) }
    }

    /// Appends one entry; `audio` is the recording's file name, when utterances are kept.
    pub fn record(&self, reason: Reason, heard: &str, answer: Option<&str>, audio: Option<&str>) {
        let line = json!({
            "time": jiff::Zoned::now().strftime("%Y-%m-%d %H:%M:%S").to_string(),
            "reason": reason.as_str(),
            "heard": heard,
            "answer": answer,
            "audio": audio,
        });
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let result = self
            .path
            .parent()
            .filter(|dir| !dir.as_os_str().is_empty())
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| OpenOptions::new().create(true).append(true).open(&self.path))
            .and_then(|mut file| writeln!(file, "{line}"));
        if let Err(error) = result {
            warn!(%error, path = %self.path.display(), "could not log an unhandled request");
        }
    }

    /// The latest `n` entries, newest first.
    pub fn recent(&self, n: usize) -> Vec<serde_json::Value> {
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let text = std::fs::read_to_string(&self.path).unwrap_or_default();
        text.lines().rev().filter_map(|line| serde_json::from_str(line).ok()).take(n).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appends_and_reads_back_newest_first() {
        let dir = std::env::temp_dir().join(format!("unhandled-{}", std::process::id()));
        let log = UnhandledLog::new(dir.join("unhandled.jsonl"));
        log.record(Reason::NotUnderstood, "do the thing", Some("Sorry, I didn't catch that."), None);
        log.record(Reason::FollowUpIgnored, "and the other thing", None, Some("utterance-1.wav"));
        let recent = log.recent(10);
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0]["heard"], "and the other thing");
        assert_eq!(recent[0]["reason"], "follow_up_ignored");
        assert_eq!(recent[1]["answer"], "Sorry, I didn't catch that.");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
