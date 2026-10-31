//! Home Assistant answers about the house, routines and safety alerts.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use tracing::{info, warn};
use voice_assistant::alerts::{self, Watch};
use voice_assistant::ha::{self, HomeAssistant, Light, Target};
use voice_assistant::house::{self, HouseCommand, Routine, Switch};
use voice_assistant::intent::MusicCommand;
use voice_assistant::lang::Lang;

use super::lights::LightChange;
use super::screen::SCREEN_MOMENT;
use super::{unreachable, AssistantConfig, Job, Worker};

/// How often Home Assistant's safety sensors are checked.
const ALERT_POLL: Duration = Duration::from_secs(5);

/// Checks the safety sensors every few seconds and hands new warnings to the worker, which speaks
/// them even while listening is paused.
pub(super) fn watch_alerts(ha: HomeAssistant, jobs: mpsc::Sender<Job>) {
    let mut watch = Watch::default();
    let mut failing = false;
    loop {
        std::thread::sleep(ALERT_POLL);
        match alerts::active(&ha) {
            Ok(active) => {
                failing = false;
                for alert in watch.due(&active, Instant::now()) {
                    info!(id = alert.id, "safety sensor on");
                    if jobs.send(Job::Alert(alerts::sentence(alert, Lang::English))).is_err() {
                        return;
                    }
                }
            }
            // Once per outage: Home Assistant restarts now and then.
            Err(error) if !failing => {
                failing = true;
                warn!(error = format!("{error:#}"), "could not check the safety sensors");
            }
            Err(_) => {}
        }
    }
}

fn house_answer(
    ha: &mut HomeAssistant,
    command: &HouseCommand,
    here: Option<&str>,
    lang: Lang,
    name: &str,
) -> anyhow::Result<String> {
    use serde_json::json;
    let no = lang == Lang::Norwegian;
    let say = |english: &str, norwegian: &str| if no { norwegian.to_owned() } else { english.to_owned() };
    Ok(match command {
        HouseCommand::Status => {
            let alerts = alerts::active(ha)?;
            let lights: Vec<Light> = ha.lights_on()?.into_iter().filter(|l| !l.status_led()).collect();
            house::status_answer(&alerts, &lights, &house::snapshot(ha)?, lang)
        }
        HouseCommand::Sun(event) => house::sun_answer(&ha.state("sun.sun")?, *event, lang)?,
        HouseCommand::FindPhone(who) => {
            let phones = house::phones(ha)?;
            let picked = house::pick_phones(&phones, who.as_deref());
            for phone in &picked {
                house::ring(ha, phone, name)?;
                info!(phone = phone.id, "ringing phone");
            }
            house::rang_answer(&picked, who.as_deref(), lang)
        }
        HouseCommand::PhoneBattery(who) => {
            let phones = house::phones(ha)?;
            house::battery_answer(&house::pick_phones(&phones, who.as_deref()), who.as_deref(), lang)
        }
        HouseCommand::GuestWifi(switch) => {
            let Some(wifi) = house::entities(ha, "switch", "guest")?.into_iter().next() else {
                return Ok(say(
                    "I can't find a guest Wi-Fi in Home Assistant.",
                    "Jeg finner ikke gjestenettet i Home Assistant.",
                ));
            };
            match switch {
                Switch::On | Switch::Off => {
                    let on = *switch == Switch::On;
                    ha.service("switch", if on { "turn_on" } else { "turn_off" }, json!({ "entity_id": wifi.id }))?;
                    match on {
                        true => say("Okay, the guest Wi-Fi is on.", "Ok, gjestenettet er på."),
                        false => say("Okay, the guest Wi-Fi is off.", "Ok, gjestenettet er av."),
                    }
                }
                Switch::Status if wifi.state != "on" => say("The guest Wi-Fi is off.", "Gjestenettet er av."),
                Switch::Status => {
                    let clients = match (wifi.clients.parse::<u32>(), no) {
                        (Ok(0), false) => " Nobody is connected.".to_owned(),
                        (Ok(0), true) => " Ingen er koblet til.".to_owned(),
                        (Ok(1), false) => " One device is connected.".to_owned(),
                        (Ok(1), true) => " Én enhet er koblet til.".to_owned(),
                        (Ok(n), false) => format!(" {n} devices are connected."),
                        (Ok(n), true) => format!(" {n} enheter er koblet til."),
                        (Err(_), _) => String::new(),
                    };
                    say("The guest Wi-Fi is on.", "Gjestenettet er på.") + &clients
                }
            }
        }
        HouseCommand::Camera(switch) => {
            let privacy = house::entities(ha, "switch", "privacy")?;
            if privacy.is_empty() {
                return Ok(say(
                    "I can't find a camera with a privacy mode.",
                    "Jeg finner ikke noe kamera med personvernmodus.",
                ));
            }
            let ids: Vec<&str> = privacy.iter().map(|p| p.id.as_str()).collect();
            match switch {
                Switch::On => {
                    ha.service("switch", "turn_off", json!({ "entity_id": ids }))?;
                    say("Okay, the camera is back on.", "Ok, kameraet er på igjen.")
                }
                Switch::Off => {
                    ha.service("switch", "turn_on", json!({ "entity_id": ids }))?;
                    say(
                        "Okay, privacy mode is on. The camera isn't recording.",
                        "Ok, personvernmodus er på. Kameraet tar ikke opp.",
                    )
                }
                Switch::Status if privacy.iter().any(|p| p.state == "on") => {
                    say("The camera is in privacy mode.", "Kameraet er i personvernmodus.")
                }
                Switch::Status if house::entities(ha, "camera", "")?.iter().any(|c| c.state == "recording") => {
                    say("The camera is recording.", "Kameraet tar opp.")
                }
                Switch::Status => say("The camera is on, but not recording.", "Kameraet er på, men tar ikke opp."),
            }
        }
        HouseCommand::Alarm(switch) => {
            let Some(panel) = house::entities(ha, "alarm_control_panel", "")?.into_iter().next() else {
                return Ok(say("There's no alarm in Home Assistant.", "Det er ingen alarm i Home Assistant."));
            };
            match switch {
                Switch::Off => say(
                    "I won't disarm the alarm by voice; anyone at the door could ask. Use the app instead.",
                    "Jeg slår ikke av alarmen med stemmen; hvem som helst ved døra kunne spurt. Bruk appen.",
                ),
                Switch::Status => house::alarm_sentence(&panel.state, lang)
                    .unwrap_or_else(|| say("I can't tell whether the alarm is on.", "Jeg vet ikke om alarmen er på.")),
                Switch::On if panel.state.starts_with("armed") => {
                    say("The alarm is already armed.", "Alarmen er allerede på.")
                }
                Switch::On => {
                    ha.service("alarm_control_panel", "alarm_arm_away", json!({ "entity_id": panel.id }))?;
                    info!(panel = panel.id, "alarm armed");
                    say("Okay, the alarm is armed.", "Ok, alarmen er på.")
                }
            }
        }
        HouseCommand::Climate(place, reading) => {
            house::climate_answer(&house::climate_readings(ha)?, place, *reading, here, lang)
        }
    })
}

/// Bedtime light colour: warm white.
const BEDTIME_KELVIN: u32 = 2700;

/// What bedtime leaves on.
pub(super) fn in_bedroom(light: &Light) -> bool {
    ha::is_room(&light.area, "bedroom") || ha::is_room(&light.name, "bedroom")
}

/// The bedroom lights that are on which bedtime dims. A Hue room is also its own lights: the
/// lights, or the room when that is all there is.
pub(super) fn bedtime_dimmed(bedroom: &[Light]) -> Vec<&str> {
    let single: Vec<&Light> = bedroom.iter().filter(|l| !l.group).collect();
    if single.is_empty() { bedroom.iter().collect() } else { single }.iter().map(|l| l.id.as_str()).collect()
}

/// The lights coming home switches on, when none of them is on yet.
pub(super) fn home_target(
    ha: &mut HomeAssistant,
    config: &AssistantConfig,
    here: Option<&str>,
) -> anyhow::Result<Option<Target>> {
    let room = match &config.home_room {
        Some(room) => Some(room.as_str()),
        None => here.filter(|_| ha.state("sun.sun").is_ok_and(|s| s["state"] == "below_horizon")),
    };
    Ok(room.map(|room| ha.find(room)).transpose()?.flatten())
}

/// A routine's answer, and the lights it switched for "undo".
fn routine_answer(
    ha: &mut HomeAssistant,
    routine: Routine,
    here: Option<&str>,
    config: &AssistantConfig,
    lang: Lang,
) -> anyhow::Result<(String, Option<(Target, bool)>)> {
    let no = lang == Lang::Norwegian;
    let mut parts = Vec::new();
    let mut switched_lights = None;
    match routine {
        Routine::Leaving | Routine::Bedtime => {
            let bedtime = routine == Routine::Bedtime;
            let keep = |l: &Light| bedtime && in_bedroom(l);
            let switched = house::everything_off(ha, keep)?;
            let except = if bedtime && !switched.lights.is_empty() {
                Some(if no { "soverommet" } else { "bedroom" })
            } else {
                None
            };
            parts.push(house::switched_answer(&switched, except, lang));
            if bedtime {
                let bedroom: Vec<Light> = ha.lights_on()?.into_iter().filter(|l| keep(l)).collect();
                let ids = bedtime_dimmed(&bedroom);
                if !ids.is_empty() {
                    let data = serde_json::json!({
                        "entity_id": ids,
                        "brightness_pct": config.bedtime_light,
                        "color_temp_kelvin": BEDTIME_KELVIN,
                    });
                    ha.send("light", "turn_on", data)?;
                    parts.push(
                        if no { "Lyset på soverommet er dempet." } else { "The bedroom lights are dimmed." }.into(),
                    );
                }
            }
            if !switched.lights.is_empty() {
                let ids = switched.lights.iter().map(|l| l.id.clone()).collect();
                let label = if no { "huset" } else { "house" }.to_owned();
                switched_lights = Some((Target { label, ids, area: true }, false));
            }
            parts.extend(alerts::active(ha)?.iter().map(|a| alerts::sentence(a, lang)));
            if !bedtime && house::snapshot(ha)?.alarm == "disarmed" {
                parts.push(
                    if no {
                        "Alarmen er av. Si slå på alarmen hvis du vil."
                    } else {
                        "The alarm is off. Say arm the alarm to set it."
                    }
                    .into(),
                );
            }
            parts.push(
                match (bedtime, no) {
                    (false, false) => "See you later.",
                    (false, true) => "Ha det bra.",
                    (true, false) => "Good night.",
                    (true, true) => "God natt.",
                }
                .into(),
            );
        }
        Routine::Lockdown | Routine::StandDown => unreachable!("the worker runs these itself"),
        Routine::Home => {
            parts.push(if no { "Velkommen hjem." } else { "Welcome home." }.into());
            if let Some(target) = home_target(ha, config, here)? {
                let on = ha.lights_on()?;
                if !on.iter().any(|l| target.ids.contains(&l.id)) {
                    ha.set(&target, true)?;
                    let label = target.label.to_lowercase();
                    parts.push(if no {
                        format!("Jeg har skrudd på lyset i {label}.")
                    } else {
                        format!("I've turned on the {label} lights.")
                    });
                    switched_lights = Some((target, true));
                }
            }
            parts.extend(alerts::active(ha)?.iter().map(|a| alerts::sentence(a, lang)));
            if house::snapshot(ha)?.alarm.starts_with("armed") {
                parts.push(if no { "Alarmen er fortsatt på." } else { "The alarm is still armed." }.into());
            }
        }
    }
    Ok((parts.join(" "), switched_lights))
}

/// The question asked before a change that a misheard request must not make on its own: the
/// camera's privacy mode ("stop the timer" was heard as "Stop the camera.") and arming the alarm.
pub(super) fn confirm_question(command: &HouseCommand, lang: Lang) -> Option<&'static str> {
    let no = lang == Lang::Norwegian;
    Some(match command {
        HouseCommand::Camera(Switch::Off) if no => "Skal jeg slå på personvernmodus?",
        HouseCommand::Camera(Switch::Off) => "Turn on privacy mode?",
        HouseCommand::Camera(Switch::On) if no => "Skal kameraet ta opp igjen?",
        HouseCommand::Camera(Switch::On) => "Turn privacy mode off?",
        HouseCommand::Alarm(Switch::On) if no => "Skal jeg slå på alarmen?",
        HouseCommand::Alarm(Switch::On) => "Arm the alarm?",
        _ => return None,
    })
}

/// "Yes" or "no" to a question, if the reply is one.
pub(super) fn confirmation(text: &str) -> Option<bool> {
    const YES: &[&str] = &[
        "yes", "yeah", "yep", "yup", "sure", "ok", "okay", "please", "correct", "do", "it", "ja", "jepp", "gjerne",
        "takk", "sir",
    ];
    const NO: &[&str] =
        &["no", "nope", "nah", "don't", "dont", "not", "cancel", "never", "mind", "nei", "ikke", "la", "være"];
    let words: Vec<String> =
        text.split_whitespace().map(voice_assistant::dialog::normalize).filter(|w| !w.is_empty()).collect();
    let all = |list: &[&str]| !words.is_empty() && words.len() <= 4 && words.iter().all(|w| list.contains(&w.as_str()));
    let first = |list: &[&str]| words.first().is_some_and(|w| list[..4].contains(&w.as_str()));
    if all(NO) || first(NO) {
        Some(false)
    } else if all(YES) {
        Some(true)
    } else {
        None
    }
}

impl Worker {
    pub(super) fn house(&mut self, command: &HouseCommand, lang: Lang) -> String {
        let no = lang == Lang::Norwegian;
        let name = self.config.wake.name().to_owned();
        let here = self.room.clone();
        let Some(ha) = &mut self.home_assistant else {
            return if no { "Home Assistant er ikke koblet til ennå." } else { "Home Assistant isn't connected yet." }
                .into();
        };
        match house_answer(ha, command, here.as_deref(), lang, &name) {
            Ok(answer) => {
                info!(?command, "house");
                answer
            }
            Err(error) => unreachable(&error, no),
        }
    }

    /// Leaving, coming home and bedtime. Lights a routine switched count as the last lights, so
    /// "undo" or "turn them back on" reverses it.
    pub(super) fn routine(&mut self, routine: Routine, lang: Lang) -> String {
        let no = lang == Lang::Norwegian;
        let here = self.room.clone();
        let Some(ha) = &mut self.home_assistant else {
            return if no { "Home Assistant er ikke koblet til ennå." } else { "Home Assistant isn't connected yet." }
                .into();
        };
        let mut answer = match routine_answer(ha, routine, here.as_deref(), &self.config, lang) {
            Ok((answer, switched)) => {
                if let Some((target, on)) = switched {
                    self.last_lights = Some((target, LightChange::Switched(on), Instant::now()));
                }
                answer
            }
            Err(error) => return unreachable(&error, no),
        };
        if let (Routine::Home, Some(music)) = (routine, &self.config.home_music) {
            let command = if music.eq_ignore_ascii_case("liked") {
                MusicCommand::Liked
            } else {
                MusicCommand::Playlist(music.clone())
            };
            let started = self.player.run(self.home_assistant.as_ref(), &command, lang);
            answer = format!("{answer} {started}");
        }
        info!(?routine, answer, "routine");
        if routine == Routine::Home {
            self.show("welcome", serde_json::json!({ "text": answer }), Some(SCREEN_MOMENT));
        }
        answer
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asks_before_privacy_mode_and_the_alarm() {
        assert_eq!(confirm_question(&HouseCommand::Camera(Switch::Off), Lang::English), Some("Turn on privacy mode?"));
        assert_eq!(confirm_question(&HouseCommand::Alarm(Switch::On), Lang::English), Some("Arm the alarm?"));
        assert_eq!(confirm_question(&HouseCommand::Camera(Switch::Status), Lang::English), None);
        assert_eq!(confirmation("Yes."), Some(true));
        assert_eq!(confirmation("Yes, please do it."), Some(true));
        assert_eq!(confirmation("Ja takk"), Some(true));
        assert_eq!(confirmation("No."), Some(false));
        assert_eq!(confirmation("No, leave it."), Some(false));
        assert_eq!(confirmation("Nei"), Some(false));
        assert_eq!(confirmation("What time is it?"), None);
        assert_eq!(confirmation("Do you love me?"), None);
    }
}
