//! The shopping list: a Home Assistant to-do list (`todo.shopping_list` by default), read and
//! changed by voice. "Add milk and eggs to the shopping list", "take milk off the list", "what's
//! on the shopping list", "legg til melk på handlelista". Removing an item ticks it off, as in the
//! Home Assistant app, so nothing is lost and an item added again is reopened, not duplicated.

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::json;

use crate::dialog::normalize;
use crate::groceries::Groceries;
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
    /// "Check if we have eggs": whether these are on the list.
    Has(Vec<String>),
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
    "away", "bort", "vekk", "out", "ut",
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
    // "Shopping list", "handlelista": the list by itself, to add to it.
    let list_word = |w: &String| LIST_WORDS.contains(&w.as_str()) || NORWEGIAN_LIST.contains(&w.as_str());
    let filler = |w: &String| w.is_empty() || POLITE.contains(&w.as_str()) || EDGE_FILLER.contains(&w.as_str());
    if words.iter().all(|w| list_word(w) || filler(w) || ["open", "åpne"].contains(&w.as_str())) {
        return Some(ListCommand::Add(Vec::new()));
    }
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

/// Said before a reply's verb or items: "*actually* remove the milk", "*oh*, and bread".
const REPLY_LEAD: &[&str] =
    &["actually", "oh", "um", "uh", "er", "well", "then", "wait", "sorry", "faktisk", "forresten"];
/// Said before "that's all", or as all of it, to "Anything else?": "*no*, that's it", "nope".
const DONE_LEAD: &[&str] = &["no", "nope", "nah", "nei", "ok", "okay", "and", "og", "i", "think", "guess"];
/// Said after "that's all": "that's all *for now*", "that's it, *thanks*".
const DONE_TAIL: &[&str] = &["for now", "for today", "thanks", "thank you", "then", "for nå", "takk", "da"];
/// Replies that close the list while it is open.
const DONE: &[&str] = &[
    "that's all",
    "that's it",
    "that is all",
    "that is it",
    "that's everything",
    "that'll be all",
    "that'll do",
    "that's enough",
    "done",
    "i'm done",
    "we're done",
    "all done",
    "nothing",
    "nothing else",
    "nothing more",
    "no more",
    "finished",
    "det var alt",
    "det er alt",
    "det holder",
    "ferdig",
    "ingenting",
    "ikke noe mer",
    "ikke mer",
];

/// Whether a reply while the list is open closes it: "that's all", "no, that's it", "nope",
/// "that's all for now".
pub fn closes_list(text: &str) -> bool {
    let mut said = text.split_whitespace().map(normalize).filter(|w| !w.is_empty()).collect::<Vec<_>>().join(" ");
    let mut trimmed = false;
    loop {
        let before = said.len();
        for tail in DONE_TAIL {
            if said == *tail {
                said.clear();
            } else if let Some(rest) = said.strip_suffix(&format!(" {tail}")) {
                said = rest.to_owned();
            }
        }
        if let Some((first, rest)) = said.split_once(' ') {
            if DONE_LEAD.contains(&first) || REPLY_LEAD.contains(&first) {
                said = rest.to_owned();
            }
        } else if DONE_LEAD.contains(&said.as_str()) {
            said.clear();
        }
        if said.len() == before {
            break;
        }
        trimmed = true;
    }
    DONE.contains(&said.as_str()) || (trimmed && said.is_empty())
}

/// A change said while the list is open, without naming the list: "remove milk", "actually take
/// the milk off", "add bread". `None` for a reply that is only items, or something else.
pub fn open_list_command(text: &str) -> Option<ListCommand> {
    open_list_commands(text).into_iter().next()
}

/// Every change in a reply while the list is open: "remove milk and add bread" is two. Empty for a
/// reply that is only items, or something else.
pub fn open_list_commands(text: &str) -> Vec<ListCommand> {
    let tokens: Vec<&str> = text.split_whitespace().collect();
    let words: Vec<String> = tokens.iter().map(|t| normalize(t)).collect();
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(verb_at) = (from..words.len()).find(|&i| {
        let w = words[i].as_str();
        !w.is_empty()
            && !POLITE.contains(&w)
            && !REPLY_LEAD.contains(&w)
            && !YES_NO.contains(&w)
            && w != "and"
            && w != "og"
    }) {
        // The next change starts at a verb after "and" or a comma: "remove milk *and add* bread".
        let end = (verb_at + 1..words.len())
            .find(|&i| {
                let joined = ["and", "og"].contains(&words[i - 1].as_str()) || tokens[i - 1].ends_with(',');
                joined && (ADD_VERBS.contains(&words[i].as_str()) || REMOVE_VERBS.contains(&words[i].as_str()))
            })
            .unwrap_or(words.len());
        let verb = words[verb_at].as_str();
        let next = words.get(verb_at + 1).map(String::as_str).unwrap_or_default();
        let mut skip = verb_at + 1;
        // "Check if we have eggs", "sjekk om vi har egg": a question, not a change.
        let asks = ["check", "sjekk"].contains(&verb) && ["if", "whether", "om"].contains(&next);
        if asks {
            skip += 1;
            skip += words[skip.min(end)..end].iter().take_while(|w| HAVE_LEAD.contains(&w.as_str())).count();
        }
        // "Ta med melk" (bring milk) adds it.
        let brings = verb == "ta" && next == "med";
        if brings {
            skip += 1;
        }
        let found = items(&tokens[skip.min(end)..end]);
        let command = if asks {
            ListCommand::Has(found.clone())
        } else if brings || ADD_VERBS.contains(&verb) {
            ListCommand::Add(found.clone())
        } else if REMOVE_VERBS.contains(&verb) {
            // "Take the milk", "check the milk": an item that is not on the list is answered as such.
            ListCommand::Remove(found.clone())
        } else {
            break;
        };
        if found.is_empty() {
            break;
        }
        out.push(command);
        from = end;
    }
    out
}

/// Yes before a reply: "yes, bread".
const YES: &[&str] = &["yes", "yeah", "yep", "ja", "jo"];
/// No before a correction: "no, remove the milk".
const NO: &[&str] = &["no", "nope", "nah", "nei"];
const YES_NO: &[&str] = &["yes", "yeah", "yep", "ja", "jo", "no", "nope", "nah", "nei"];
/// Between "check if" and the items: "check if *we have* eggs", "sjekk om *vi har* egg".
const HAVE_LEAD: &[&str] = &[
    "we", "i", "you", "have", "got", "need", "there's", "there", "is", "are", "any", "vi", "jeg", "har", "trenger",
    "det", "er", "noe",
];
/// "No milk", "not milk", "ikke melk", "don't add milk": nothing to add.
const NEGATION: &[&str] = &["not", "don't", "dont", "never", "ikke", "aldri"];
/// Words a sentence opens with that make it a request or a question, never an item.
const NOT_ITEMS: &[&str] = &[
    "check", "take", "remove", "delete", "is", "are", "do", "does", "did", "have", "has", "if", "what", "what's",
    "how", "turn", "switch", "er", "har", "hva", "hvor", "vet", "sjekk", "skru", "slå", "ta", "fjern",
];

/// The items in a reply that is only items ("milk and eggs", "og brunost"), as to "what should I
/// add?".
pub fn items_of(text: &str) -> Vec<String> {
    let tokens: Vec<&str> = text.split_whitespace().collect();
    let words: Vec<String> = tokens.iter().map(|t| normalize(t)).collect();
    if words.iter().any(|w| NEGATION.contains(&w.as_str())) {
        return Vec::new();
    }
    let lead = |w: &String| {
        ADD_VERBS.contains(&w.as_str())
            || POLITE.contains(&w.as_str())
            || REPLY_LEAD.contains(&w.as_str())
            || YES.contains(&w.as_str())
    };
    let mut skip = words.iter().take_while(|w| lead(w)).count();
    // "No, bread" or "no milk" corrects or refuses; only "no, add bread" adds.
    if words.get(skip).is_some_and(|w| NO.contains(&w.as_str())) {
        if !words.get(skip + 1).is_some_and(|w| ADD_VERBS.contains(&w.as_str())) {
            return Vec::new();
        }
        skip += 1;
        skip += words[skip..].iter().take_while(|w| lead(w)).count();
    }
    // "Check if we have eggs", "take the milk": a request, never items.
    if words.get(skip).is_some_and(|w| NOT_ITEMS.contains(&w.as_str()) || REMOVE_VERBS.contains(&w.as_str())) {
        return Vec::new();
    }
    items(&tokens[skip..])
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
    /// Items are written under their Norwegian names (`groceries`).
    norwegian: bool,
    /// "Added milk." rather than "Added milk to the shopping list.", while items come one by one.
    pub brief: bool,
}

/// What changed, for "undo".
#[derive(Debug, Clone, PartialEq)]
pub enum Change {
    Added(Vec<String>),
    Removed(Vec<String>),
}

impl ShoppingList {
    /// Items said in English or misheard are written under their Norwegian names.
    pub fn with_norwegian_names(mut self) -> Self {
        self.norwegian = true;
        self
    }

    /// What is still to buy, in list order.
    pub fn open_items(&mut self, ha: &HomeAssistant) -> Result<Vec<String>> {
        let entity = self.entity(ha)?;
        Ok(get_items(ha, &entity)?.into_iter().filter(Item::open).map(|i| i.summary).collect())
    }

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
                // What has been on the list counts as a name too: brands, the household's own words.
                let groceries = self.norwegian.then(|| Groceries::new(items.iter().map(|i| i.summary.as_str())));
                for heard in new {
                    let name = &groceries.as_ref().map_or_else(|| heard.clone(), |g| g.norwegian(heard));
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
                let answer = added_answer(&added, &already, self.brief, lang);
                (answer, (!added.is_empty()).then_some(Change::Added(added)))
            }
            ListCommand::Remove(names) => {
                let open: Vec<&Item> = items.iter().filter(|i| i.open()).collect();
                let known = crate::names::Names::new(open.iter().map(|i| i.summary.as_str()));
                let (mut removed, mut missing) = (Vec::new(), Vec::new());
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
                    } else {
                        missing.push(name.clone());
                    }
                }
                let answer = removed_answer(&removed, &missing, lang);
                (answer, (!removed.is_empty()).then_some(Change::Removed(removed)))
            }
            ListCommand::Has(names) => {
                let open: Vec<&Item> = items.iter().filter(|i| i.open()).collect();
                let known = crate::names::Names::new(open.iter().map(|i| i.summary.as_str()));
                let (on, off): (Vec<String>, Vec<String>) = names.iter().cloned().partition(|name| {
                    open.iter().any(|i| i.summary.to_lowercase() == name.to_lowercase()) || known.best(name).is_some()
                });
                (has_answer(&on, &off, lang), None)
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

/// "Added bread. Milk is already on the list.": what was added, then what was there already.
fn added_answer(added: &[String], already: &[String], brief: bool, lang: Lang) -> String {
    let no = lang == Lang::Norwegian;
    let mut parts = Vec::new();
    if !added.is_empty() {
        let list = join(&lower(added), lang);
        parts.push(match (no, brief) {
            (false, true) => format!("Added {list}."),
            (true, true) => format!("La til {list}."),
            (false, false) => format!("Added {list} to the shopping list."),
            (true, false) => format!("La til {list} på handlelista."),
        });
    }
    if !already.is_empty() {
        let list = capitalize(&join(&lower(already), lang));
        let is = if already.len() > 1 { "are" } else { "is" };
        parts.push(if no {
            format!("{list} står allerede på lista.")
        } else {
            format!("{list} {is} already on the list.")
        });
    }
    parts.join(" ")
}

/// "Okay, milk is off the list. Eggs isn't on the list.": what was ticked off, then what was not there.
fn removed_answer(removed: &[String], missing: &[String], lang: Lang) -> String {
    let no = lang == Lang::Norwegian;
    let mut parts = Vec::new();
    if !removed.is_empty() {
        let list = join(&lower(removed), lang);
        let is = if removed.len() > 1 { "are" } else { "is" };
        parts.push(if no { format!("Ok, {list} er krysset av.") } else { format!("Okay, {list} {is} off the list.") });
    }
    if !missing.is_empty() {
        let list = capitalize(&join(&lower(missing), lang));
        let isnt = if missing.len() > 1 { "aren't" } else { "isn't" };
        parts.push(if no { format!("{list} står ikke på lista.") } else { format!("{list} {isnt} on the list.") });
    }
    parts.join(" ")
}

/// "Yes, eggs are on the list." / "No, eggs aren't on the list."
fn has_answer(on: &[String], off: &[String], lang: Lang) -> String {
    let no = lang == Lang::Norwegian;
    let mut parts = Vec::new();
    if !on.is_empty() {
        let list = join(&lower(on), lang);
        let is = if on.len() > 1 { "are" } else { "is" };
        parts.push(match (no, off.is_empty()) {
            (false, true) => format!("Yes, {list} {is} on the list."),
            (false, false) => format!("{} {is} on the list.", capitalize(&list)),
            (true, true) => format!("Ja, {list} står på lista."),
            (true, false) => format!("{} står på lista.", capitalize(&list)),
        });
    }
    if !off.is_empty() {
        let list = join(&lower(off), lang);
        let isnt = if off.len() > 1 { "aren't" } else { "isn't" };
        parts.push(match (no, on.is_empty()) {
            (false, true) => format!("No, {list} {isnt} on the list."),
            (false, false) => format!("{} {isnt} on the list.", capitalize(&list)),
            (true, true) => format!("Nei, {list} står ikke på lista."),
            (true, false) => format!("{} står ikke på lista.", capitalize(&list)),
        });
    }
    parts.join(" ")
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
    fn replies_that_close_the_open_list() {
        for done in [
            "That's all.",
            "that's all for now",
            "No, that's it.",
            "no that's it thanks",
            "No.",
            "Nope.",
            "No thanks.",
            "Nothing else.",
            "Okay, I think that's everything.",
            "Nei takk.",
            "Det var alt for nå.",
        ] {
            assert!(closes_list(done), "{done}");
        }
        for items in ["milk", "no milk", "eggs and bread", "remove milk", "brunost", "nothing bundt cake"] {
            assert!(!closes_list(items), "{items}");
        }
    }

    #[test]
    fn changes_said_while_the_list_is_open() {
        let remove = |v: &[&str]| Some(ListCommand::Remove(v.iter().map(|s| s.to_string()).collect()));
        assert_eq!(open_list_command("remove milk"), remove(&["Milk"]));
        assert_eq!(open_list_command("Actually, remove the milk."), remove(&["Milk"]));
        assert_eq!(open_list_command("take the eggs off"), remove(&["Eggs"]));
        assert_eq!(open_list_command("fjern melk"), remove(&["Melk"]));
        assert_eq!(open_list_command("add milk"), add(&["Milk"]));
        assert_eq!(open_list_command("oh, and add bread"), add(&["Bread"]));
        assert_eq!(open_list_command("milk and eggs"), None);
        assert_eq!(open_list_command("remove"), None);
        assert_eq!(items_of("actually bread"), ["Bread"]);
        // A yes or no before the correction is not an item.
        assert_eq!(open_list_command("No, remove the milk"), remove(&["Milk"]));
        assert_eq!(open_list_command("Oh no, take the eggs off"), remove(&["Eggs"]));
        assert_eq!(open_list_command("Nei, fjern melk"), remove(&["Melk"]));
        assert_eq!(items_of("Yes, bread"), ["Bread"]);
        assert!(items_of("No, bread").is_empty());
        assert!(items_of("no milk").is_empty());
        assert!(items_of("no, not milk").is_empty());
        assert!(items_of("nei, ikke melk").is_empty());
        assert_eq!(items_of("no, add bread"), ["Bread"]);
        for request in ["Check if we have eggs", "ta med melk", "take the milk", "check the milk", "is milk on it"] {
            assert!(items_of(request).is_empty(), "{request}");
        }
        assert_eq!(open_list_command("take out the milk"), remove(&["Milk"]));
        assert_eq!(open_list_command("take the milk out"), remove(&["Milk"]));
        assert_eq!(open_list_command("ta ut melka"), remove(&["Melka"]));
        assert_eq!(
            open_list_commands("add bread and put milk on"),
            [ListCommand::Add(vec!["Bread".into()]), ListCommand::Add(vec!["Milk".into()])]
        );
        // Two changes in one reply; neither swallows the other.
        assert_eq!(
            open_list_commands("Remove milk and add bread"),
            [ListCommand::Remove(vec!["Milk".into()]), ListCommand::Add(vec!["Bread".into()])]
        );
        // Questions and bringing are not removals.
        assert_eq!(open_list_command("Check if we have eggs"), Some(ListCommand::Has(vec!["Eggs".into()])));
        assert_eq!(open_list_command("ta med melk"), add(&["Melk"]));
        assert_eq!(open_list_command("take the milk"), remove(&["Milk"]));
        assert_eq!(open_list_command("check the milk"), remove(&["Milk"]));
        assert_eq!(open_list_command("check off the eggs"), remove(&["Eggs"]));
        assert_eq!(open_list_command("ta egg av"), remove(&["Egg"]));
    }

    #[test]
    fn mixed_answers_name_both_kinds() {
        let items = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let en = Lang::English;
        assert_eq!(added_answer(&items(&["Bread"]), &[], true, en), "Added bread.");
        assert_eq!(
            added_answer(&items(&["Bread"]), &items(&["Milk"]), false, en),
            "Added bread to the shopping list. Milk is already on the list."
        );
        assert_eq!(added_answer(&[], &items(&["Milk", "Eggs"]), true, en), "Milk and eggs are already on the list.");
        assert_eq!(removed_answer(&items(&["Milk"]), &[], en), "Okay, milk is off the list.");
        assert_eq!(
            removed_answer(&items(&["Milk"]), &items(&["Caviar"]), en),
            "Okay, milk is off the list. Caviar isn't on the list."
        );
        assert_eq!(
            removed_answer(&items(&["Melk"]), &items(&["Kaviar"]), Lang::Norwegian),
            "Ok, melk er krysset av. Kaviar står ikke på lista."
        );
    }

    #[test]
    fn items_are_joined_for_speech() {
        let items = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(join(&items(&["milk"]), Lang::English), "milk");
        assert_eq!(join(&items(&["milk", "eggs", "bread"]), Lang::English), "milk, eggs and bread");
        assert_eq!(join(&items(&["melk", "egg"]), Lang::Norwegian), "melk og egg");
    }
}
