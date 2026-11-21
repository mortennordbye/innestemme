//! What a Home Assistant skill hands back to the engine: the answer, and the changes to the
//! conversation it asks for. The skills here have no conversation of their own (the engine keeps
//! which lights "them" means, what "undo" goes back to and the questions asked back), so they
//! say what changed and the engine applies it.

use serde_json::Value;

use crate::ha::Target;

/// The last change to the lights, for "undo".
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LightChange {
    /// Switched on or off.
    Switched(bool),
    /// A brightness or colour change; how they were before is kept in
    /// [`UNDO_SCENE`](crate::lights::UNDO_SCENE).
    Level,
}

/// What an answer did to the lights "them" and "undo" refer to.
#[derive(Debug, Clone, Default, PartialEq)]
pub enum Lights {
    /// Nothing the engine keeps.
    #[default]
    Same,
    /// These lights changed so: the last lights now.
    Changed(Target, LightChange),
    /// A level change was undone: the look from before it is gone, so a second "undo" has
    /// nothing to go back to.
    Undone,
    /// A scene changed the lights: changes before it cannot be undone, though "them" still means
    /// the lights last named.
    UndoGone,
}

/// A question asked back, for the reply that answers it.
#[derive(Debug, Clone, PartialEq)]
pub enum Ask {
    /// "Which room?" for switching the lights on or off.
    LightsRoom(bool),
    /// "Which room?" for this scene request.
    SceneRoom(String),
}

/// A skill's answer and what the engine does with it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Done {
    pub answer: String,
    pub lights: Lights,
    pub ask: Option<Ask>,
    /// A screen module and its data, shown for a moment.
    pub moment: Option<(&'static str, Value)>,
}

impl Done {
    /// Only an answer.
    pub fn say(answer: impl Into<String>) -> Self {
        Self { answer: answer.into(), ..Self::default() }
    }
}
