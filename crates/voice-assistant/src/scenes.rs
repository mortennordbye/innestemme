//! Scenes and scripts: turning one on by name, and the answers. Finding the scene or script a
//! request names is [`ha::resolve_scene`] and [`ha::resolve_script`].

use anyhow::Result;
use tracing::info;

use crate::ha::{self, HomeAssistant};
use crate::lang::Lang;
use crate::skill::{Ask, Done, Lights};

/// "Set the living room to relax": the scene, or "Which room?" when it is in several rooms and
/// neither the request nor `room` (where the microphone is) says which.
pub fn scene(ha: &mut HomeAssistant, request: &str, room: Option<&str>, lang: Lang) -> Result<Done> {
    let no = lang == Lang::Norwegian;
    let scenes = ha.scenes()?.to_vec();
    let scene = match ha::resolve_scene(&scenes, request, room) {
        ha::SceneMatch::Found(scene) => scene.clone(),
        ha::SceneMatch::WhichRoom => {
            return Ok(Done {
                ask: Some(Ask::SceneRoom(request.to_owned())),
                ..Done::say(if no { "Hvilket rom?" } else { "Which room?" })
            });
        }
        ha::SceneMatch::NotFound if no => return Ok(Done::say("Jeg fant ikke den scenen.")),
        ha::SceneMatch::NotFound => return Ok(Done::say("I couldn't find that scene.")),
    };
    ha.activate(&scene)?;
    info!(scene = scene.id, request, "scene activated");
    let name = scene.name.to_lowercase();
    Ok(Done {
        // Undo would otherwise jump back past the scene, to the look before an earlier dimming.
        lights: Lights::UndoGone,
        moment: Some(("scene", serde_json::json!({ "name": scene.name }))),
        ..Done::say(if no { format!("Ok, {name}.") } else { format!("Okay, {name}.") })
    })
}

/// "Run movie time": the script whose name the request says.
pub fn script(ha: &mut HomeAssistant, request: &str, lang: Lang) -> Result<String> {
    let no = lang == Lang::Norwegian;
    let Some(script) = ha::resolve_script(ha.scripts()?, request).cloned() else {
        return Ok(if no { "Jeg fant ikke det skriptet." } else { "I couldn't find that script." }.into());
    };
    ha.activate(&script)?;
    info!(script = script.id, request, "script started");
    let name = script.name.to_lowercase();
    Ok(if no { format!("Ok, kjører {name}.") } else { format!("Okay, running {name}.") })
}

/// A script or scene whose whole name is the request.
#[derive(Debug, Clone, PartialEq)]
pub enum ByName {
    Script,
    /// The scene's name as Home Assistant has it.
    Scene(String),
}

/// The script, else the scene, whose whole name is the request ("movie time"); `None` when there
/// is none or Home Assistant cannot say.
pub fn by_exact_name(ha: &mut HomeAssistant, request: &str) -> Option<ByName> {
    if ha.scripts().ok().and_then(|scripts| ha::exact(scripts, request).cloned()).is_some() {
        return Some(ByName::Script);
    }
    let scene = ha.scenes().ok().and_then(|scenes| ha::exact(scenes, request).cloned())?;
    Some(ByName::Scene(scene.name))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Without Home Assistant a request fails, and a name alone is no script or scene.
    #[test]
    fn an_unreachable_home_assistant() {
        let mut ha = HomeAssistant::new("http://127.0.0.1:9", "test");
        assert!(scene(&mut ha, "relax", None, Lang::English).is_err());
        assert!(script(&mut ha, "run movie time", Lang::English).is_err());
        assert_eq!(by_exact_name(&mut ha, "movie time"), None);
    }
}
