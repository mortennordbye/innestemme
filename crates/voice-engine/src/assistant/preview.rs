//! Typed requests from the web page and the hub, answered or planned without acting.

use tracing::{info, warn};
use voice_assistant::ha::{self, Light};
use voice_assistant::house::{self, HouseCommand, Routine, Switch};
use voice_assistant::intent::{self, Intent};
use voice_assistant::lang::Lang;
use voice_assistant::memory::MemoryCommand;
use voice_assistant::party::PartyCommand;
use voice_assistant::shopping::ListCommand;
use voice_assistant::{plan, tv};

use super::home::{bedtime_dimmed, home_target, in_bedroom};
use super::lights::LightChange;
use super::{Preview, Worker};

/// Whether any lamp of `ids` is on, a group through its own state.
fn plan_lit(lamps: &[plan::Lamp], ids: &[String]) -> bool {
    lamps.iter().any(|l| l.on && ids.contains(&l.id))
}

impl Worker {
    /// The answer to a typed request, for requests that only read: they are answered for real
    /// (weather, departures, what is on), the rest are not run.
    pub(super) fn preview(&mut self, text: &str) -> Preview {
        let request = self.config.wake.strip(text).unwrap_or_else(|| text.to_owned());
        let intent = intent::parse(&request);
        let reads = matches!(
            intent,
            Intent::Weather { .. }
                | Intent::Time
                | Intent::Date
                | Intent::Joke
                | Intent::Transit(_)
                | Intent::Power(_)
                | Intent::LightsStatus { .. }
                | Intent::WhosHome(_)
                | Intent::WhereIs { .. }
                | Intent::Convert(_)
                | Intent::Memory(MemoryCommand::Recall(_) | MemoryCommand::List)
                | Intent::Briefing
                | Intent::News
                | Intent::Lookup(_)
                | Intent::Calculate(_)
                | Intent::Distance(_)
                | Intent::Thanks
                | Intent::SmallTalk(_)
                | Intent::Cancel
                | Intent::ShoppingList(ListCommand::Read)
                | Intent::House(
                    HouseCommand::Status
                        | HouseCommand::Sun(_)
                        | HouseCommand::PhoneBattery(_)
                        | HouseCommand::GuestWifi(Switch::Status)
                        | HouseCommand::Camera(Switch::Status)
                        | HouseCommand::Alarm(Switch::Status | Switch::Off)
                        | HouseCommand::Climate(..)
                        | HouseCommand::Sensors(_)
                )
        );
        let answer = match intent {
            _ if reads => match self.compose(&request, Lang::English) {
                Some(answer) => answer,
                None => return Preview::Silent,
            },
            Intent::Unknown if self.config.llm.is_none() => "Sorry, I didn't catch that.".to_owned(),
            Intent::Unknown => {
                return Preview::NotRun("The language model would decide; it is not asked from the page.")
            }
            _ => return Preview::NotRun("Answering this switches or plays something, so the page does not run it."),
        };
        let answer = match &mut self.persona {
            Some(persona) => persona.style(&answer, jiff::Zoned::now().hour() as u8),
            None => answer,
        };
        let mut audio = Vec::new();
        for sentence in answer.split_inclusive(['.', '?', '!']).map(str::trim).filter(|s| !s.is_empty()) {
            if let Err(error) = self.tts.speak(sentence, Lang::English, &mut |pcm| audio.extend_from_slice(pcm)) {
                warn!(%error, "speech synthesis failed");
            }
        }
        info!(request, answer, "preview for the web page");
        Preview::Spoken { text: answer, audio }
    }

    /// What a typed request would do to each light, worked out by the code that runs it, without
    /// running it: `{"request", "lights": {id: {kind, text}}, "also": [...]}`, or `{"reason"}` when
    /// it would switch nothing.
    pub(super) fn plan(&mut self, text: &str) -> serde_json::Value {
        let request = self.config.wake.strip(text).unwrap_or_else(|| text.to_owned());
        let intent = intent::parse(&request);
        let reason = |reason: &str| serde_json::json!({ "request": request, "reason": reason });
        let lamps = match &self.home_assistant {
            None => return reason("Home Assistant isn't connected yet."),
            Some(ha) => match plan::lamps(ha) {
                Ok(lamps) => lamps,
                Err(error) => return reason(&format!("Home Assistant didn't answer: {error:#}")),
            },
        };
        match self.plan_of(&intent, &lamps) {
            Ok(mut plan) => {
                for (id, effect) in &mut plan.lights {
                    if effect.kind == "off" && ha::power_cycled(id) {
                        effect.text = "off, power back on".into();
                    }
                }
                info!(request, lights = plan.lights.len(), "light preview for the hub");
                serde_json::json!({ "request": request, "lights": plan.lights, "also": plan.also })
            }
            Err(why) => reason(&why),
        }
    }

    fn plan_of(&mut self, intent: &Intent, lamps: &[plan::Lamp]) -> Result<plan::Plan, String> {
        let mut plan = plan::Plan::default();
        let target = |this: &mut Self, request: &str| match this.light_target(request, Lang::English) {
            Ok(Some(target)) => Ok(target),
            Ok(None) => Err("It would ask which room.".to_owned()),
            Err(answer) => Err(answer),
        };
        let here = self.room.clone();
        let failed = |error: anyhow::Error| format!("Home Assistant didn't answer: {error:#}");
        match intent {
            Intent::Lights { on, target: request } => {
                let found = target(self, request)?;
                plan.set(lamps, &found.ids, |_| plan::Effect::switch(*on));
            }
            Intent::LightLevel { level, target: request } => {
                let found = target(self, request)?;
                plan.set(lamps, &found.ids, |l| plan::Effect::level(level, l.on));
            }
            Intent::Undo => match self.recent_lights() {
                Some((found, LightChange::Switched(on))) => plan.set(lamps, &found.ids, |_| plan::Effect::switch(!on)),
                Some((found, LightChange::Level)) => {
                    plan.set(lamps, &found.ids, |_| plan::Effect::new("scene", "as before"))
                }
                None => return Err("There's nothing to undo.".into()),
            },
            Intent::Scene(request) => {
                let ha = self.home_assistant.as_mut().ok_or("Home Assistant isn't connected yet.")?;
                let scenes = ha.scenes().map_err(failed)?.to_vec();
                let scene = match ha::resolve_scene(&scenes, request, here.as_deref()) {
                    ha::SceneMatch::Found(scene) => scene.clone(),
                    ha::SceneMatch::WhichRoom => return Err("It would ask which room.".into()),
                    ha::SceneMatch::NotFound => return Err("There is no such scene.".into()),
                };
                // Hue scenes do not list their lights in Home Assistant: the scene's room.
                let room = ha.find(&scene.area).map_err(failed)?.map(|t| t.ids).unwrap_or_default();
                plan.set(lamps, &room, |_| plan::Effect::new("scene", scene.name.clone()));
            }
            Intent::Party(command) => {
                let ha = self.home_assistant.as_ref().ok_or("Home Assistant isn't connected yet.")?;
                match command {
                    PartyCommand::On { .. } if !self.config.party.configured() => {
                        return Err("Party mode needs a Hue bridge.".into())
                    }
                    // The bridge streams to its colour lights; the rest keep what they show.
                    PartyCommand::On { .. } => {
                        let (colour, _) = house::lockdown_lights(ha).map_err(failed)?;
                        plan.set(lamps, &colour, |_| plan::Effect::new("party", "follows the music"));
                    }
                    PartyCommand::Status => {}
                    PartyCommand::Off => {
                        let ids = house::snapshot_lights(ha, house::PARTY_SCENE).map_err(failed)?;
                        let ids = ids.ok_or("There's no party on.")?;
                        plan.set(lamps, &ids, |_| plan::Effect::new("restore", "put back"));
                    }
                }
            }
            Intent::Routine(routine) => {
                let config = &self.config;
                let ha = self.home_assistant.as_mut().ok_or("Home Assistant isn't connected yet.")?;
                match routine {
                    Routine::Leaving | Routine::Bedtime => {
                        let bedtime = *routine == Routine::Bedtime;
                        let on = ha.lights_on().map_err(failed)?;
                        let off = house::to_switch_off(on.clone(), |l| bedtime && in_bedroom(l));
                        let ids: Vec<&str> = off.iter().map(|l| l.id.as_str()).collect();
                        plan.set(lamps, &ids, |_| plan::Effect::switch(false));
                        if bedtime {
                            let bedroom: Vec<Light> = on.into_iter().filter(in_bedroom).collect();
                            let text = format!("{} %, warm", config.bedtime_light);
                            plan.set(lamps, &bedtime_dimmed(&bedroom), |_| plan::Effect::new("level", text.clone()));
                        }
                        let snapshot = house::snapshot(ha).map_err(failed)?;
                        if !snapshot.playing.is_empty() {
                            plan.also.push("The music pauses.".into());
                        }
                        if !tv::on_tvs(ha).map_err(failed)?.is_empty() {
                            plan.also.push("The TV turns off.".into());
                        }
                    }
                    Routine::Home => {
                        let found = home_target(ha, config, here.as_deref()).map_err(failed)?;
                        let found = found.ok_or("Coming home would switch no lights now.")?;
                        let lit = plan_lit(lamps, &found.ids);
                        plan.set(lamps, &found.ids, |l| match lit {
                            true => plan::Effect::new("same", if l.on { "already on" } else { "left off" }),
                            false => plan::Effect::new("on", "on"),
                        });
                        if let Some(music) = &config.home_music {
                            plan.also.push(format!("Music starts: {music}."));
                        }
                    }
                    Routine::Lockdown => {
                        let (red, off) = house::lockdown_lights(ha).map_err(failed)?;
                        plan.set(lamps, &red, |_| plan::Effect::new("red", "red, pulsing"));
                        plan.set(lamps, &off, |_| plan::Effect::switch(false));
                        plan.also.push(format!(
                            "Plays {} at {} % or louder.",
                            config.lockdown_music, config.lockdown_volume
                        ));
                    }
                    Routine::StandDown => {
                        let ids = house::snapshot_lights(ha, house::LOCKDOWN_SCENE).map_err(failed)?;
                        let ids = ids.ok_or("There is no lockdown to end.")?;
                        plan.set(lamps, &ids, |_| plan::Effect::new("restore", "put back"));
                    }
                }
            }
            Intent::Unknown => return Err("Not understood without the language model.".into()),
            _ => return Err("This request doesn't switch any lights.".into()),
        }
        Ok(plan)
    }
}
