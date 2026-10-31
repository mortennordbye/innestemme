//! The shopping list, and the replies that add to it while it is open.

use std::time::{Duration, Instant};

use tracing::info;
use voice_assistant::lang::Lang;
use voice_assistant::shopping::{Change, ListCommand};

use super::{unreachable, Worker};

/// After an item is added the list stays open this long: each short reply is more items.
pub(super) const LIST_OPEN: Duration = Duration::from_secs(60);
/// A reply longer than this while the list is open is the room talking, not items.
pub(super) const LIST_REPLY_WORDS: usize = 6;

impl Worker {
    /// `brief`: one of several items said one after the other.
    pub(super) fn shopping_list(&mut self, command: ListCommand, lang: Lang, brief: bool) -> String {
        let no = lang == Lang::Norwegian;
        let Some(ha) = &self.home_assistant else {
            return if no { "Home Assistant er ikke koblet til ennå." } else { "Home Assistant isn't connected yet." }
                .into();
        };
        let adding = matches!(command, ListCommand::Add(_));
        self.list_open = adding.then(Instant::now);
        self.shopping.brief = brief;
        match self.shopping.run(ha, &command, lang) {
            Ok((answer, change)) => {
                info!(?command, ?change, "shopping list");
                let added = matches!(change, Some(Change::Added(_)));
                if let Some(change) = change {
                    self.last_list = Some((change, Instant::now()));
                }
                // The first item of a few: say that more may follow without "add".
                match (added && !brief, no) {
                    (true, false) => format!("{answer} Anything else?"),
                    (true, true) => format!("{answer} Noe mer?"),
                    (false, _) => answer,
                }
            }
            Err(error) => unreachable(&error, no),
        }
    }

    /// The items in a reply while the list is open. Said alone, a Norwegian item is better heard by
    /// Whisper told the language, when it is set up; otherwise the listener's words.
    pub(super) fn items_heard(&mut self, text: &str) -> Vec<String> {
        let norwegian = self.config.norwegian_groceries.as_ref().filter(|_| !self.request_audio.is_empty());
        if let Some(norwegian) = norwegian {
            let start = Instant::now();
            let heard = norwegian.transcribe(&self.request_audio);
            info!(heard, english = text, took = ?start.elapsed(), "items heard in norwegian");
            let items = voice_assistant::shopping::items_of(&heard);
            if !items.is_empty() {
                return items;
            }
        }
        voice_assistant::shopping::items_of(text)
    }

    pub(super) fn list_is_open(&self) -> bool {
        self.list_open.is_some_and(|at| at.elapsed() < LIST_OPEN)
    }
}

#[cfg(test)]
mod tests {
    use voice_assistant::intent::{self, Intent};

    /// Replies to the open list reach it only when nothing else claims them.
    #[test]
    fn open_list_replies_are_no_other_request() {
        for reply in ["remove milk", "actually remove the milk", "add milk", "take the eggs off", "milk and eggs"] {
            assert_eq!(intent::parse(reply), Intent::Unknown, "{reply}");
        }
    }
}
