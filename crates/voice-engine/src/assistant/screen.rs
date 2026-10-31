//! What the TV screen shows.

use std::time::{Duration, Instant};

use tracing::warn;
use voice_assistant::lang::Lang;
use voice_assistant::tv;

use super::{AssistantEvent, Worker};

/// How long the TV screen shows asked-for data (weather, departures) before the face is back.
const SCREEN_DATA: Duration = Duration::from_secs(120);
/// Music shows longer: it is often left on while it plays.
const SCREEN_MUSIC: Duration = Duration::from_secs(300);

/// A scene, a joke or coming home: a moment, not something to read.
pub(super) const SCREEN_MOMENT: Duration = Duration::from_secs(25);

impl Worker {
    /// The TV screen with `module` on it, for pages showing it now and any that open later.
    pub(super) fn show(&mut self, module: &str, data: serde_json::Value, ttl: Option<Duration>) {
        let message = self.screen.show(module, data, ttl);
        self.emit(AssistantEvent::Show(Some(message)));
    }

    pub(super) fn hide(&mut self) {
        self.screen.hide();
        self.emit(AssistantEvent::Show(None));
    }

    /// "Show the weather on the TV": the data on the screen, and the screen opened in the TV's
    /// browser when no page shows it yet.
    pub(super) fn show_screen(&mut self, screen: tv::Screen, lang: Lang) -> String {
        let no = lang == Lang::Norwegian;
        let shown = match screen {
            tv::Screen::Jarvis => {
                self.hide();
                Ok(if no { "Ok, her er jeg." } else { "Okay, I'm on the TV." })
            }
            tv::Screen::Weather => self.screen_weather(no),
            tv::Screen::Timers => self.screen_timers(no),
            tv::Screen::Music => self.screen_music(no),
            tv::Screen::Departures => self.screen_departures(no),
        };
        let done = match shown {
            Ok(done) => done,
            Err(answer) => return answer,
        };
        if self.screen.watched() {
            return done.into();
        }
        let Some(url) = self.config.screen_url.clone() else {
            return if no {
                "TV-skjermen er ikke satt opp. Den trenger innstillingen screen-url."
            } else {
                "The TV screen isn't set up. It needs the screen-url setting."
            }
            .into();
        };
        let Some(ha) = &self.home_assistant else {
            return if no { "Home Assistant er ikke koblet til ennå." } else { "Home Assistant isn't connected yet." }
                .into();
        };
        match self.tv.open_page(ha, &url) {
            Ok(()) => done.into(),
            Err(error) => {
                warn!(error = format!("{error:#}"), "could not open the screen on the tv");
                if no {
                    "Beklager, jeg fikk ikke åpnet skjermen på TV-en."
                } else {
                    "Sorry, I couldn't open the screen on the TV."
                }
                .into()
            }
        }
    }

    fn screen_weather(&mut self, no: bool) -> Result<&'static str, String> {
        let Some(home) = self.home_location.clone() else {
            return Err(if no {
                "Jeg vet ikke hvor hjemme er. Den trenger innstillingen address."
            } else {
                "I don't know where home is. It needs the address setting."
            }
            .into());
        };
        match self.weather.screen(&home) {
            Ok(Some(data)) => {
                self.show("weather", data, Some(SCREEN_DATA));
                Ok(if no { "Ok, været er på TV-en." } else { "Okay, the weather is on the TV." })
            }
            Ok(None) => Err(if no { "Jeg fikk ingen værmelding." } else { "I got no forecast." }.into()),
            Err(error) => {
                warn!(error = format!("{error:#}"), "weather for the screen failed");
                Err(if no {
                    "Beklager, jeg får ikke hentet været nå."
                } else {
                    "Sorry, I couldn't get the forecast right now."
                }
                .into())
            }
        }
    }

    fn screen_timers(&mut self, no: bool) -> Result<&'static str, String> {
        let now = Instant::now();
        let timers: Vec<serde_json::Value> = self
            .timers
            .list()
            .iter()
            .map(|t| {
                serde_json::json!({
                    "id": format!("timer-{}", t.id),
                    "name": t.name.clone().or_else(|| t.reminder.clone()).unwrap_or_default(),
                    "left": t.left(now).as_secs_f32().ceil() as u32,
                    "total": t.total.as_secs(),
                    "active": t.is_running(),
                })
            })
            .collect();
        if timers.is_empty() {
            return Err(if no { "Ingen tidtakere går nå." } else { "There are no timers running." }.into());
        }
        // Until the last timer is gone: the page counts down and drops them as they end.
        self.show("timers", serde_json::json!({ "timers": timers }), None);
        Ok(if no { "Ok, tidtakerne er på TV-en." } else { "Okay, the timers are on the TV." })
    }

    fn screen_music(&mut self, no: bool) -> Result<&'static str, String> {
        let not_connected = || -> String {
            if no { "Home Assistant er ikke koblet til ennå." } else { "Home Assistant isn't connected yet." }.into()
        };
        let Some(ha) = &self.home_assistant else { return Err(not_connected()) };
        let state = self.player.check(ha).and_then(|entity| ha.state(&entity));
        let state = match state {
            Ok(state) => state,
            Err(error) => {
                warn!(error = format!("{error:#}"), "music for the screen failed");
                return Err(if no {
                    "Beklager, jeg finner ikke musikken nå."
                } else {
                    "Sorry, I couldn't find the music right now."
                }
                .into());
            }
        };
        let a = &state["attributes"];
        if !["playing", "paused"].contains(&state["state"].as_str().unwrap_or_default()) || a["media_title"].is_null() {
            return Err(if no { "Ingenting spilles nå." } else { "Nothing is playing." }.into());
        }
        let data = serde_json::json!({
            "title": a["media_title"],
            "artist": a["media_artist"],
            "album": a["media_album_name"],
            "position": a["media_position"],
            "updated": a["media_position_updated_at"],
            "duration": a["media_duration"],
            "playing": state["state"] == "playing",
        });
        self.show("music", data, Some(SCREEN_MUSIC));
        Ok(if no { "Ok, musikken er på TV-en." } else { "Okay, the music is on the TV." })
    }

    fn screen_departures(&mut self, no: bool) -> Result<&'static str, String> {
        if !self.transit.is_configured() {
            return Err(if no {
                "Jeg vet ikke hvor hjemme er. Den trenger innstillingen address."
            } else {
                "I don't know where home is. It needs the address setting."
            }
            .into());
        }
        match self.transit.board() {
            Ok(Some(data)) => {
                self.show("departures", data, Some(SCREEN_DATA));
                Ok(if no { "Ok, avgangene er på TV-en." } else { "Okay, the departures are on the TV." })
            }
            Ok(None) => {
                Err(if no { "Jeg fant ingen avganger i nærheten." } else { "I found no departures nearby." }.into())
            }
            Err(error) => {
                warn!(error = format!("{error:#}"), "departures for the screen failed");
                Err(if no {
                    "Beklager, jeg får ikke hentet avgangene nå."
                } else {
                    "Sorry, I couldn't get the departures right now."
                }
                .into())
            }
        }
    }
}
