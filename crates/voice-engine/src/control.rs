//! The dashboard's Home Assistant views (Lights, Climate, Media, House): every entity they show,
//! with its area, from one template call that all screens share, and the service calls the page
//! may make. The page has no login and the TV's browser opens it too, so it gets what a wall panel
//! in the living room should do and no more: lights, scenes, switches, players, selects, the
//! shopping list and arming the alarm. Not disarming it, not pressing buttons (restarts), not
//! installing updates, and of the network's switches only the guest Wi-Fi: switching off the
//! home's own Wi-Fi from here would take the page, and the house, offline.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use voice_assistant::ha::HomeAssistant;

use crate::web::Response;

/// Several screens poll every few seconds; Home Assistant renders the template at most this often.
const FRESH: Duration = Duration::from_secs(2);
/// Points a trend line is cut down to.
const TREND_POINTS: usize = 48;

/// Each entity the views use: its state, name, area, device, whether it is network gear (UniFi), and the
/// attributes the page reads (all of them would include dates, which a template cannot write as
/// JSON). Sensors only of the kinds the page draws.
const STATES_TEMPLATE: &str = r#"{% set keys = ['friendly_name', 'device_class', 'unit_of_measurement', 'brightness', 'color_mode', 'supported_color_modes', 'rgb_color', 'color_temp_kelvin', 'min_color_temp_kelvin', 'max_color_temp_kelvin', 'entity_id', 'media_title', 'media_artist', 'media_album_name', 'entity_picture', 'volume_level', 'is_volume_muted', 'source', 'source_list', 'options', 'min', 'max', 'step', 'installed_version', 'latest_version', 'title', 'temperature', 'temperature_unit', 'humidity', 'pressure', 'wind_speed', 'wind_speed_unit', 'supported_features', 'in_progress', 'mass_player_type'] %}
{% set domains = ['light', 'scene', 'switch', 'media_player', 'sensor', 'binary_sensor', 'weather', 'person', 'select', 'number', 'update', 'todo', 'camera', 'alarm_control_panel', 'input_boolean', 'automation', 'button'] %}
{% set kinds = ['temperature', 'humidity', 'pressure', 'battery', 'power', 'energy', 'illuminance'] %}
{% set network = integration_entities('unifi') %}
[{% for s in states if s.domain in domains and (s.domain != 'sensor' or s.attributes.device_class in kinds) %}{"id": {{ s.entity_id | tojson }}, "state": {{ s.state | tojson }}, "name": {{ s.name | tojson }}, "area": {{ (area_name(s.entity_id) or '') | tojson }}, "device": {{ (device_attr(s.entity_id, 'name_by_user') or device_attr(s.entity_id, 'name') or '') | tojson }}, "network": {{ (s.entity_id in network) | tojson }}, "a": { {%- for k in keys if s.attributes[k] is defined %}{% set v = s.attributes[k] %}{{ k | tojson }}: {{ (v | list | tojson) if (v is iterable and v is not string and v is not mapping) else (v | tojson) }}{% if not loop.last %}, {% endif %}{% endfor -%} }}{% if not loop.last %},{% endif %}{% endfor %}]"#;

/// The services the page may call, by domain.
const ALLOWED: &[(&str, &[&str])] = &[
    ("light", &["turn_on", "turn_off", "toggle"]),
    ("scene", &["turn_on"]),
    ("switch", &["turn_on", "turn_off", "toggle"]),
    ("input_boolean", &["turn_on", "turn_off", "toggle"]),
    ("automation", &["turn_on", "turn_off", "toggle"]),
    (
        "media_player",
        &[
            "media_play_pause",
            "media_play",
            "media_pause",
            "media_next_track",
            "media_previous_track",
            "volume_set",
            "volume_mute",
            "turn_on",
            "turn_off",
            "select_source",
        ],
    ),
    ("select", &["select_option"]),
    ("number", &["set_value"]),
    ("todo", &["add_item", "update_item"]),
    ("alarm_control_panel", &["alarm_arm_home", "alarm_arm_away"]),
];

/// What a call may pass besides the entity, and how each is read from the query.
enum Kind {
    Int,
    Float,
    Bool,
    Text,
    Rgb,
}
const PARAMETERS: &[(&str, Kind)] = &[
    ("brightness_pct", Kind::Int),
    ("color_temp_kelvin", Kind::Int),
    ("rgb_color", Kind::Rgb),
    ("volume_level", Kind::Float),
    ("is_volume_muted", Kind::Bool),
    ("source", Kind::Text),
    ("option", Kind::Text),
    ("value", Kind::Float),
    ("item", Kind::Text),
    ("status", Kind::Text),
];

pub struct Control {
    ha: Option<Mutex<HomeAssistant>>,
    last: Mutex<Option<(Instant, Value)>>,
}

impl Control {
    pub fn new(home_assistant: Option<(String, String)>) -> Self {
        Self {
            ha: home_assistant.map(|(url, token)| Mutex::new(HomeAssistant::new(&url, &token))),
            last: Mutex::new(None),
        }
    }

    fn ha(&self) -> Result<std::sync::MutexGuard<'_, HomeAssistant>> {
        let ha = self.ha.as_ref().context("Home Assistant isn't connected")?;
        Ok(ha.lock().unwrap_or_else(|e| e.into_inner()))
    }

    /// `/api/ha/states`.
    pub fn states(&self) -> Result<Value> {
        let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((_, value)) = last.as_ref().filter(|(at, _)| at.elapsed() < FRESH) {
            return Ok(value.clone());
        }
        let text = self.ha()?.template(STATES_TEMPLATE)?;
        let value: Value = serde_json::from_str(&text).context("parsing the dashboard's states")?;
        *last = Some((Instant::now(), value.clone()));
        Ok(value)
    }

    /// `POST /api/ha/call?domain=light&service=turn_on&entity_id=light.kitchen&brightness_pct=40`.
    pub fn call(&self, query: &str) -> Result<()> {
        let param = |key: &str| query.split('&').find_map(|p| p.strip_prefix(key)?.strip_prefix('=')).map(decode);
        let (domain, service) = (param("domain").unwrap_or_default(), param("service").unwrap_or_default());
        // Several, comma separated, in one call: a whole apartment's lights switch together.
        let entities: Vec<String> =
            param("entity_id").context("`entity_id` is missing")?.split(',').map(str::to_owned).collect();
        let allowed = ALLOWED.iter().any(|(d, services)| *d == domain && services.contains(&service.as_str()));
        if !allowed {
            bail!("{domain}.{service} is not allowed from the page");
        }
        for entity in &entities {
            if entity.split('.').next() != Some(domain.as_str()) {
                bail!("{entity} is not a {domain}");
            }
            if domain == "switch" && self.network_switch(entity)? {
                bail!("the network's switches are read-only here, except the guest Wi-Fi");
            }
        }
        let mut data = json!({ "entity_id": entities });
        for (key, kind) in PARAMETERS {
            let Some(raw) = param(key) else { continue };
            data[*key] = match kind {
                Kind::Int => raw.parse::<i64>().map(Value::from).context("not a whole number")?,
                Kind::Float => raw.parse::<f64>().map(Value::from).context("not a number")?,
                Kind::Bool => Value::from(raw == "true" || raw == "1"),
                Kind::Text => Value::from(raw),
                Kind::Rgb => {
                    let rgb: Vec<u8> = raw.split(',').map(|c| c.trim().parse()).collect::<Result<_, _>>()?;
                    anyhow::ensure!(rgb.len() == 3, "rgb_color is three numbers");
                    Value::from(rgb)
                }
            };
        }
        self.ha()?.service(&domain, &service, data)?;
        // The next poll shows the change rather than the state from before it.
        *self.last.lock().unwrap_or_else(|e| e.into_inner()) = None;
        tracing::info!(domain, service, ?entities, "home assistant call from the web page");
        Ok(())
    }

    /// Network gear's switch other than a guest network's.
    fn network_switch(&self, entity: &str) -> Result<bool> {
        let states = self.states()?;
        let found = states.as_array().into_iter().flatten().find(|s| s["id"] == entity);
        Ok(found.is_some_and(|s| {
            let name = s["name"].as_str().unwrap_or_default().to_lowercase();
            s["network"] == true && !["guest", "gjest"].iter().any(|g| name.contains(g))
        }))
    }

    /// `/api/ha/history?entity=...`: the last 24 hours of a sensor, as `[[seconds, value], ...]`.
    pub fn history(&self, query: &str) -> Result<Value> {
        let entity = query.split('&').find_map(|p| p.strip_prefix("entity=")).map(decode).context("`entity`")?;
        anyhow::ensure!(entity.starts_with("sensor."), "history is for sensors");
        let since = jiff::Timestamp::now() - jiff::SignedDuration::from_hours(24);
        let path = format!("/api/history/period/{since}?filter_entity_id={entity}&minimal_response&no_attributes");
        let value = self.ha()?.get(&path)?;
        let points: Vec<(i64, f64)> = value
            .get(0)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|p| {
                let at: jiff::Timestamp = p["last_changed"].as_str()?.parse().ok()?;
                Some((at.as_second(), p["state"].as_str()?.parse().ok()?))
            })
            .collect();
        Ok(downsample(&points, TREND_POINTS).into_iter().map(|(t, v)| json!([t, v])).collect())
    }

    /// `/api/ha/forecast?entity=weather.home&type=daily`.
    pub fn forecast(&self, query: &str) -> Result<Value> {
        let param = |key: &str| query.split('&').find_map(|p| p.strip_prefix(key)?.strip_prefix('=')).map(decode);
        let entity = param("entity").context("`entity`")?;
        anyhow::ensure!(entity.starts_with("weather."), "a weather entity");
        let kind = param("type").filter(|t| t == "hourly").unwrap_or_else(|| "daily".into());
        let mut response =
            self.ha()?.service_response("weather", "get_forecasts", json!({ "entity_id": entity, "type": kind }))?;
        Ok(response[entity.as_str()]["forecast"].take())
    }

    /// `/api/ha/image?path=/api/media_player_proxy/...`: album art and camera stills, which the
    /// browser cannot fetch from Home Assistant itself.
    pub fn image(&self, query: &str) -> Result<Response> {
        let path = query.split('&').find_map(|p| p.strip_prefix("path=")).map(decode).context("`path`")?;
        let proxied = ["/api/media_player_proxy/", "/api/camera_proxy/"].iter().any(|p| path.starts_with(p));
        anyhow::ensure!(proxied && !path.contains(".."), "only Home Assistant's image proxies");
        let (kind, bytes) = self.ha()?.get_bytes(&path)?;
        let kind = if kind.contains("png") { "image/png" } else { "image/jpeg" };
        Ok(("200 OK", kind, bytes))
    }
}

fn decode(value: &str) -> String {
    crate::web::decode(value)
}

/// At most `n` points, each the mean of its slice of the series.
fn downsample(points: &[(i64, f64)], n: usize) -> Vec<(i64, f64)> {
    if points.len() <= n {
        return points.to_vec();
    }
    points
        .chunks(points.len().div_ceil(n))
        .map(|chunk| {
            let mean = chunk.iter().map(|(_, v)| v).sum::<f64>() / chunk.len() as f64;
            (chunk[chunk.len() / 2].0, (mean * 10.0).round() / 10.0)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_what_the_page_may_not_do() {
        let control = Control::new(None);
        let refused = |q: &str| control.call(q).unwrap_err().to_string();
        assert!(refused("domain=button&service=press&entity_id=button.router_restart").contains("not allowed"));
        assert!(refused("domain=alarm_control_panel&service=alarm_disarm&entity_id=alarm_control_panel.home")
            .contains("not allowed"));
        assert!(refused("domain=update&service=install&entity_id=update.core").contains("not allowed"));
        assert!(refused("domain=light&service=turn_on&entity_id=switch.kitchen").contains("not a light"));
        assert!(refused("domain=light&service=turn_on").contains("entity_id"));
        assert!(refused("domain=light&service=turn_on&entity_id=light.a,switch.b").contains("not a light"));
    }

    #[test]
    fn trends_are_cut_down_to_their_means() {
        let points: Vec<(i64, f64)> = (0..100).map(|i| (i, i as f64)).collect();
        let small = downsample(&points, 10);
        assert_eq!(small.len(), 10);
        assert_eq!(small[0], (5, 4.5));
        assert_eq!(downsample(&points[..5], 10).len(), 5);
    }

    #[test]
    fn images_only_through_the_proxies() {
        let control = Control::new(Some(("http://127.0.0.1:9".into(), "t".into())));
        assert!(control.image("path=%2Fapi%2Fstates").is_err());
        assert!(control.image("path=%2Fapi%2Fcamera_proxy%2F..%2Fstates").is_err());
    }
}
