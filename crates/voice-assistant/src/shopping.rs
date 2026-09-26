//! The shopping list: a Home Assistant to-do list (`todo.shopping_list` by default), read and
//! changed by voice. "Add milk and eggs to the shopping list", "take milk off the list", "what's
//! on the shopping list", "legg til melk på handlelista". Removing an item ticks it off, as in the
//! Home Assistant app, so nothing is lost and an item added again is reopened, not duplicated.

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::json;

use crate::dialog::normalize;
use crate::ha::HomeAssistant;
use crate::lang::Lang;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListCommand {
    /// Empty when the request named nothing: "add to the shopping list" -> "What should I add?"
    Add(Vec<String>),
    Remove(Vec<String>),
    Read,
    /// Ticks off every open item.
    Clear,
}

/// Words that name the list. "liste"/"lista"/"listen" alone count only after a Norwegian
/// preposition ("på lista"), since "listen" is also English.
const LIST_WORDS: &[&str] = &[
    "list",
    "shopping",
    "grocery",
    "groceries",
    "handleliste",
    "handlelista",
    "handlelisten",
    "handlelisa",
    "handlelist",
    "innkjøpsliste",
    "innkjøpslista",
    "innkjøpslisten",
];
const NORWEGIAN_LIST: &[&str] = &["liste", "lista", "listen"];
const NORWEGIAN_PREPOSITIONS: &[&str] = &["på", "til", "fra", "av"];

const ADD_VERBS: &[&str] = &["add", "put", "legg", "legge", "sett", "sette", "skriv", "skrive", "føy"];
const REMOVE_VERBS: &[&str] = &[
    "remove", "delete", "take", "cross", "tick", "check", "fjern", "fjerne", "slett", "slette", "ta", "kryss", "stryk",
];
const CLEAR_VERBS: &[&str] = &["clear", "empty", "tøm", "tømme", "nullstill"];
const POLITE: &[&str] = &[
    "can", "could", "would", "will", "you", "please", "just", "ok", "okay", "also", "kan", "du", "vil", "vær", "så",
    "snill", "også",
];
/// Dropped around the items: "add *some* milk *to the* list", "legg *til* melk *på* lista".
const EDGE_FILLER: &[&str] = &[
    "to", "on", "onto", "the", "my", "our", "off", "from", "of", "some", "a", "an", "please", "til", "på", "opp",
    "inn", "fra", "av", "min", "mi", "vår", "noe", "litt", "en", "ei", "et", "og", "and", "also", "også", "i", "in",
];

/// A shopping-list request, or `None` when the text does not name the list.
pub fn parse(text: &str) -> Option<ListCommand> {
    let tokens: Vec<&str> = text.split_whitespace().collect();
    let words: Vec<String> = tokens.iter().map(|t| normalize(t)).collect();
    let list_at = words.iter().enumerate().position(|(i, w)| {
        LIST_WORDS.contains(&w.as_str())
            || (NORWEGIAN_LIST.contains(&w.as_str())
                && i > 0
                && (NORWEGIAN_PREPOSITIONS.contains(&words[i - 1].as_str())
                    || words.get(i.wrapping_sub(2)).is_some_and(|w| NORWEGIAN_PREPOSITIONS.contains(&w.as_str()))))
    })?;
    // "the shopping list" is two list words; items stop at the first.
    let verb_at = words.iter().position(|w| !w.is_empty() && !POLITE.contains(&w.as_str()))?;
    let verb = words[verb_at].as_str();
    let has = |w: &str| words.iter().any(|x| x == w);
    if CLEAR_VERBS.contains(&verb)
        || ((has("slett") || has("fjern")) && has("alt"))
        || (has("remove") && has("everything"))
    {
        return Some(ListCommand::Clear);
    }
    // Items sit between the verb and the list ("add milk to the list"), or after the verb when
    // the list comes first ("shopping list, add milk").
    let end = if list_at > verb_at { list_at } else { tokens.len() };
    let items = || items(&tokens[verb_at + 1..end]);
    if ADD_VERBS.contains(&verb) {
        return Some(ListCommand::Add(items()));
    }
    if REMOVE_VERBS.contains(&verb) {
        let found = items();
        return (!found.is_empty()).then_some(ListCommand::Remove(found));
    }
    // "What's on the shopping list", "read the list", "hva står på handlelista", "les opp lista".
    let read_words = ["what", "what's", "whats", "read", "tell", "hva", "les", "si", "anything", "noe"];
    if read_words.iter().any(|w| has(w)) {
        return Some(ListCommand::Read);
    }
    None
}

/// Splits "milk, eggs and bread" into items, dropping filler at both ends of each.
fn items(tokens: &[&str]) -> Vec<String> {
    let text = tokens.join(" ").to_lowercase();
    let text = text.replace(" and ", ",").replace(" og ", ",").replace(" & ", ",");
    text.split(',')
        .filter_map(|part| {
            let words: Vec<String> = part.split_whitespace().map(clean).filter(|w| !w.is_empty()).collect();
            let start = words.iter().position(|w| !EDGE_FILLER.contains(&w.as_str()))?;
            let end = words.iter().rposition(|w| !EDGE_FILLER.contains(&w.as_str()))?;
            Some(capitalize(&words[start..=end].join(" ")))
        })
        .collect()
}

/// Keeps letters, digits and the hyphen ("t-skjorte"); the transcriber's punctuation goes.
fn clean(word: &str) -> String {
    word.chars()
        .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '\'')
        .collect::<String>()
        .trim_matches('-')
        .to_owned()
}

fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

#[derive(Debug, Clone, Deserialize)]
struct Item {
    summary: String,
    uid: String,
    status: String,
}

impl Item {
    fn open(&self) -> bool {
        self.status == "needs_action"
    }
}

/// Every to-do list as `{"id", "name"}`.
const LISTS_TEMPLATE: &str = r#"[{% for s in states.todo %}{"id": {{ s.entity_id | tojson }}, "name": {{ s.name | tojson }}}{% if not loop.last %},{% endif %}{% endfor %}]"#;

#[derive(Default)]
pub struct ShoppingList {
    entity: Option<String>,
}

/// What changed, for "undo".
#[derive(Debug, Clone, PartialEq)]
pub enum Change {
    Added(Vec<String>),
    Removed(Vec<String>),
}

impl ShoppingList {
    /// Runs a command; the answer, and what changed on the list.
    pub fn run(&mut self, ha: &HomeAssistant, command: &ListCommand, lang: Lang) -> Result<(String, Option<Change>)> {
        let no = lang == Lang::Norwegian;
        let entity = self.entity(ha)?;
        let items = get_items(ha, &entity)?;
        Ok(match command {
            ListCommand::Add(new) if new.is_empty() => {
                (if no { "Hva skal jeg legge til?" } else { "What should I add?" }.into(), None)
            }
            ListCommand::Add(new) => {
                let mut added = Vec::new();
                let mut already = Vec::new();
                for name in new {
                    let key = name.to_lowercase();
                    match items.iter().find(|i| i.summary.to_lowercase() == key) {
                        Some(item) if item.open() => already.push(item.summary.clone()),
                        // Ticked off earlier: bring it back rather than adding a second one.
                        Some(item) => {
                            ha.service(
                                "todo",
                                "update_item",
                                json!({ "entity_id": entity, "item": item.uid, "status": "needs_action" }),
                            )?;
                            added.push(item.summary.clone());
                        }
                        None => {
                            ha.service("todo", "add_item", json!({ "entity_id": entity, "item": name }))?;
                            added.push(name.clone());
                        }
                    }
                }
                let answer = match (no, added.is_empty()) {
                    (false, false) => format!("Added {} to the shopping list.", join(&lower(&added), lang)),
                    (true, false) => format!("La til {} på handlelista.", join(&lower(&added), lang)),
                    (false, true) => format!("{} is already on the list.", capitalize(&join(&lower(&already), lang))),
                    (true, true) => format!("{} står allerede på lista.", capitalize(&join(&lower(&already), lang))),
                };
                (answer, (!added.is_empty()).then_some(Change::Added(added)))
            }
            ListCommand::Remove(names) => {
                let open: Vec<&Item> = items.iter().filter(|i| i.open()).collect();
                let known = crate::names::Names::new(open.iter().map(|i| i.summary.as_str()));
                let mut removed = Vec::new();
                for name in names {
                    let exact = open.iter().find(|i| i.summary.to_lowercase() == name.to_lowercase());
                    // The transcriber's spelling of an item the list already has.
                    let item = exact.copied().or_else(|| {
                        let found = known.best(name)?;
                        open.iter().find(|i| i.summary == found.name).copied()
                    });
                    if let Some(item) = item {
                        ha.service(
                            "todo",
                            "update_item",
                            json!({ "entity_id": entity, "item": item.uid, "status": "completed" }),
                        )?;
                        removed.push(item.summary.clone());
                    }
                }
                let answer = match (no, removed.is_empty()) {
                    (false, false) => format!("Okay, {} is off the list.", join(&lower(&removed), lang)),
                    (true, false) => format!("Ok, {} er krysset av.", join(&lower(&removed), lang)),
                    (false, true) => format!("{} isn't on the list.", capitalize(&join(&lower(names), lang))),
                    (true, true) => format!("{} står ikke på lista.", capitalize(&join(&lower(names), lang))),
                };
                (answer, (!removed.is_empty()).then_some(Change::Removed(removed)))
            }
            ListCommand::Read => {
                let open: Vec<String> = items.iter().filter(|i| i.open()).map(|i| i.summary.to_lowercase()).collect();
                // A spoken list longer than this is not useful; the app has the rest.
                const SPOKEN: usize = 10;
                let answer = match (no, open.len()) {
                    (false, 0) => "The shopping list is empty.".into(),
                    (true, 0) => "Handlelista er tom.".into(),
                    (false, n) if n > SPOKEN => {
                        format!("{n} things, starting with {}.", join(&open[..SPOKEN], lang))
                    }
                    (true, n) if n > SPOKEN => format!("{n} ting, blant annet {}.", join(&open[..SPOKEN], lang)),
                    (false, _) => format!("On the shopping list: {}.", join(&open, lang)),
                    (true, _) => format!("På handlelista: {}.", join(&open, lang)),
                };
                (answer, None)
            }
            ListCommand::Clear => {
                let open: Vec<&Item> = items.iter().filter(|i| i.open()).collect();
                for item in &open {
                    ha.service(
                        "todo",
                        "update_item",
                        json!({ "entity_id": entity, "item": item.uid, "status": "completed" }),
                    )?;
                }
                let removed: Vec<String> = open.iter().map(|i| i.summary.clone()).collect();
                let answer = match (no, removed.is_empty()) {
                    (false, true) => "The shopping list is already empty.",
                    (true, true) => "Handlelista er allerede tom.",
                    (false, false) => "Okay, the shopping list is empty.",
                    (true, false) => "Ok, handlelista er tom.",
                };
                (answer.into(), (!removed.is_empty()).then_some(Change::Removed(removed)))
            }
        })
    }

    /// Reverses a change: added items are ticked off, ticked-off items reopened.
    pub fn undo(&mut self, ha: &HomeAssistant, change: &Change) -> Result<()> {
        let entity = self.entity(ha)?;
        let (names, status) = match change {
            Change::Added(names) => (names, "completed"),
            Change::Removed(names) => (names, "needs_action"),
        };
        for name in names {
            ha.service("todo", "update_item", json!({ "entity_id": entity, "item": name, "status": status }))?;
        }
        Ok(())
    }

    /// The list to use: one whose id or name says shopping, else the only one there is.
    fn entity(&mut self, ha: &HomeAssistant) -> Result<String> {
        if let Some(entity) = &self.entity {
            return Ok(entity.clone());
        }
        #[derive(Deserialize)]
        struct List {
            id: String,
            name: String,
        }
        let lists: Vec<List> = serde_json::from_str(&ha.template(LISTS_TEMPLATE)?)
            .context("parsing the to-do lists from Home Assistant")?;
        let shopping = |l: &&List| {
            let text = format!("{} {}", l.id, l.name).to_lowercase();
            ["shopping", "grocer", "handle", "innkjøp"].iter().any(|w| text.contains(w))
        };
        let entity = match lists.iter().find(shopping) {
            Some(list) => list.id.clone(),
            None if lists.len() == 1 => lists[0].id.clone(),
            None => anyhow::bail!("no shopping list in Home Assistant (to-do lists: {})", lists.len()),
        };
        tracing::info!(entity, "shopping list");
        self.entity = Some(entity.clone());
        Ok(entity)
    }
}

fn get_items(ha: &HomeAssistant, entity: &str) -> Result<Vec<Item>> {
    let mut response = ha.service_response("todo", "get_items", json!({ "entity_id": entity }))?;
    serde_json::from_value(response[entity]["items"].take()).context("parsing the shopping list items")
}

fn lower(items: &[String]) -> Vec<String> {
    items.iter().map(|i| i.to_lowercase()).collect()
}

/// "milk", "milk and eggs", "milk, eggs and bread".
fn join(items: &[String], lang: Lang) -> String {
    let and = if lang == Lang::Norwegian { "og" } else { "and" };
    match items {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} {and} {last}", rest.join(", ")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn add(items: &[&str]) -> Option<ListCommand> {
        Some(ListCommand::Add(items.iter().map(|s| s.to_string()).collect()))
    }

    #[test]
    fn english_requests() {
        assert_eq!(parse("Add milk to the shopping list."), add(&["Milk"]));
        assert_eq!(parse("add milk, eggs and bread to my shopping list"), add(&["Milk", "Eggs", "Bread"]));
        assert_eq!(parse("Could you put some bananas on the grocery list?"), add(&["Bananas"]));
        assert_eq!(parse("also add toilet paper to the list"), add(&["Toilet paper"]));
        assert_eq!(parse("add to the shopping list"), add(&[]));
        assert_eq!(parse("take milk off the shopping list"), Some(ListCommand::Remove(vec!["Milk".into()])));
        assert_eq!(parse("remove the eggs from the list"), Some(ListCommand::Remove(vec!["Eggs".into()])));
        assert_eq!(parse("What's on the shopping list?"), Some(ListCommand::Read));
        assert_eq!(parse("read me the shopping list"), Some(ListCommand::Read));
        assert_eq!(parse("clear the shopping list"), Some(ListCommand::Clear));
    }

    #[test]
    fn norwegian_requests() {
        assert_eq!(parse("Legg til melk på handlelista."), add(&["Melk"]));
        assert_eq!(parse("legg melk og egg på handlelisten"), add(&["Melk", "Egg"]));
        assert_eq!(parse("kan du legge til brød på lista"), add(&["Brød"]));
        assert_eq!(parse("sett kaffe på lista"), add(&["Kaffe"]));
        assert_eq!(parse("skriv opp bakepapir på handlelista"), add(&["Bakepapir"]));
        assert_eq!(parse("fjern melk fra handlelista"), Some(ListCommand::Remove(vec!["Melk".into()])));
        assert_eq!(parse("ta egg av lista"), Some(ListCommand::Remove(vec!["Egg".into()])));
        assert_eq!(parse("Hva står på handlelista?"), Some(ListCommand::Read));
        assert_eq!(parse("tøm handlelista"), Some(ListCommand::Clear));
    }

    #[test]
    fn other_requests_are_not_the_list() {
        assert_eq!(parse("listen to this"), None);
        assert_eq!(parse("add five minutes to the timer"), None);
        assert_eq!(parse("what's the weather"), None);
        assert_eq!(parse("play my shopping playlist"), None);
    }

    #[test]
    fn items_are_joined_for_speech() {
        let items = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(join(&items(&["milk"]), Lang::English), "milk");
        assert_eq!(join(&items(&["milk", "eggs", "bread"]), Lang::English), "milk, eggs and bread");
        assert_eq!(join(&items(&["melk", "egg"]), Lang::Norwegian), "melk og egg");
    }
}
