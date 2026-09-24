//! Settings from a file and from secret files, layered under the command line and environment.
//!
//! Every option has one name, used three ways: `--wake-name` on the command line, `VOICE_WAKE_NAME`
//! in the environment, `wake-name:` (or `wake_name:`) in the YAML settings file. Precedence: command
//! line, then environment, then the file, then the built-in default. The file suits a Kubernetes
//! ConfigMap; Home Assistant add-on options (`/data/options.json`) are JSON, which is YAML too.
//!
//! Secrets: for an option read from `X`, a variable `X_FILE` names a file holding the value (a
//! mounted Kubernetes Secret). Inside a Home Assistant add-on, the Supervisor's token and proxy are
//! used for Home Assistant when nothing else is configured.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::Command;

/// The option that names the settings file, and its variable.
pub const CONFIG_ARG: &str = "config";
pub const CONFIG_ENV: &str = "VOICE_CONFIG";
/// Home Assistant add-ons get a token for the Supervisor's proxy to the Home Assistant API.
const SUPERVISOR_URL: &str = "http://supervisor/core";

/// Values by long option name, from every source below the command line and environment.
pub fn layered(command: &Command, args: &[OsString], env: &dyn Fn(&str) -> Option<String>) -> Result<Values> {
    let mut values = Values::default();
    if let Some(path) = config_path(args, env) {
        values.file(command, &path)?;
    }
    for arg in command.get_arguments() {
        let (Some(long), Some(var)) = (arg.get_long(), arg.get_env()) else {
            continue;
        };
        let var = var.to_string_lossy();
        if env(&var).is_some() {
            continue;
        }
        if let Some(file) = env(&format!("{var}_FILE")) {
            let text = std::fs::read_to_string(&file).with_context(|| format!("reading {var}_FILE ({file})"))?;
            values.set(long, text.trim().to_owned(), format!("{var}_FILE"));
        }
    }
    // Values the environment or command line override would never be used; drop them so the log
    // shows only what takes effect.
    for arg in command.get_arguments() {
        let Some(long) = arg.get_long() else { continue };
        let from_env = arg.get_env().is_some_and(|var| env(&var.to_string_lossy()).is_some());
        let flag = format!("--{long}");
        let from_args = args.iter().skip(1).any(|a| {
            let a = a.to_string_lossy();
            a == flag || a.starts_with(&format!("{flag}="))
        });
        if from_env || from_args {
            values.map.remove(long);
        }
    }
    if let Some(token) = env("SUPERVISOR_TOKEN") {
        let configured = |name: &str, var: &str| values.map.contains_key(name) || env(var).is_some();
        if !configured("ha-url", "HA_URL") && !configured("ha-token", "HA_TOKEN") {
            values.set("ha-url", SUPERVISOR_URL.into(), "Home Assistant add-on".into());
            values.set("ha-token", token, "Home Assistant add-on".into());
        }
    }
    Ok(values)
}

/// Settings below the command line: each value and where it came from.
#[derive(Debug, Default)]
pub struct Values {
    map: BTreeMap<String, (String, String)>,
}

impl Values {
    fn set(&mut self, long: &str, value: String, source: String) {
        self.map.insert(long.to_owned(), (value, source));
    }

    fn file(&mut self, command: &Command, path: &Path) -> Result<()> {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading settings {}", path.display()))?;
        let doc: serde_yaml_ng::Value =
            serde_yaml_ng::from_str(&text).with_context(|| format!("parsing settings {}", path.display()))?;
        let map = match doc {
            serde_yaml_ng::Value::Mapping(map) => map,
            serde_yaml_ng::Value::Null => return Ok(()),
            _ => bail!("{}: expected `name: value` lines", path.display()),
        };
        let known: Vec<&str> = command.get_arguments().filter_map(|a| a.get_long()).collect();
        for (key, value) in map {
            let Some(key) = key.as_str() else { bail!("{}: setting names must be text", path.display()) };
            let long = key.replace('_', "-");
            if !known.contains(&long.as_str()) || long == CONFIG_ARG {
                bail!("{}: unknown setting `{key}` (see `voice-engine --help`)", path.display());
            }
            let text = match value {
                // An empty value in the file means "use the default".
                serde_yaml_ng::Value::Null => continue,
                serde_yaml_ng::Value::Bool(b) => b.to_string(),
                serde_yaml_ng::Value::Number(n) => n.to_string(),
                serde_yaml_ng::Value::String(s) => s,
                serde_yaml_ng::Value::Sequence(items) => items
                    .iter()
                    .map(|item| match item {
                        serde_yaml_ng::Value::String(s) => Ok(s.clone()),
                        serde_yaml_ng::Value::Number(n) => Ok(n.to_string()),
                        _ => bail!("{}: `{key}` lists only text", path.display()),
                    })
                    .collect::<Result<Vec<_>>>()?
                    .join(","),
                _ => bail!("{}: `{key}` must be a single value or a list", path.display()),
            };
            self.set(&long, text, path.display().to_string());
        }
        Ok(())
    }

    /// Installs the values as the options' defaults, so the command line and environment still win.
    pub fn apply(&self, mut command: Command) -> Command {
        for (long, (value, _)) in &self.map {
            // Parsed once at startup; clap keeps defaults as `'static`.
            let value: &'static str = Box::leak(value.clone().into_boxed_str());
            let id = command.get_arguments().find(|a| a.get_long() == Some(long)).map(|a| a.get_id().clone());
            if let Some(id) = id {
                command = command.mut_arg(id, |arg| {
                    let secret = arg.is_hide_env_values_set();
                    arg.default_value(value).hide_default_value(secret)
                });
            }
        }
        command
    }

    /// Option name and source of every layered value, for the startup log; no values, which may be secret.
    pub fn sources(&self) -> Vec<(String, String)> {
        self.map.iter().map(|(long, (_, source))| (long.clone(), source.clone())).collect()
    }
}

/// `--config PATH`, `--config=PATH`, or `VOICE_CONFIG`.
fn config_path(args: &[OsString], env: &dyn Fn(&str) -> Option<String>) -> Option<PathBuf> {
    let flag = format!("--{CONFIG_ARG}");
    let mut iter = args.iter().skip(1);
    while let Some(arg) = iter.next() {
        let arg = arg.to_string_lossy();
        if arg == flag {
            return iter.next().map(PathBuf::from);
        }
        if let Some(path) = arg.strip_prefix(&format!("{flag}=")) {
            return Some(PathBuf::from(path));
        }
    }
    env(CONFIG_ENV).map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use clap::{Arg, ArgAction};

    use super::*;

    fn command() -> Command {
        Command::new("t")
            .arg(Arg::new("config").long("config").env(CONFIG_ENV))
            .arg(Arg::new("wake_name").long("wake-name").env("VOICE_WAKE_NAME").default_value("Homie"))
            .arg(Arg::new("home").long("home").env("VOICE_HOME"))
            .arg(Arg::new("norwegian").long("norwegian").env("VOICE_NORWEGIAN").action(ArgAction::Set))
            .arg(Arg::new("spellings").long("wake-spellings").env("VOICE_WAKE_SPELLINGS").value_delimiter(','))
            .arg(Arg::new("ha_url").long("ha-url").env("HA_URL"))
            .arg(Arg::new("ha_token").long("ha-token").env("HA_TOKEN").hide_env_values(true))
    }

    fn parse(args: &[&str], env: &[(&str, &str)], file: Option<&str>) -> Result<clap::ArgMatches> {
        let dir = std::env::temp_dir().join(format!("voice-settings-{}-{}", std::process::id(), rand_suffix()));
        std::fs::create_dir_all(&dir)?;
        let mut env: HashMap<String, String> = env.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        if let Some(text) = file {
            let path = dir.join("settings.yaml");
            std::fs::write(&path, text)?;
            env.insert(CONFIG_ENV.into(), path.display().to_string());
        }
        for (k, v) in env.clone() {
            if let Some(content) = v.strip_prefix("file:") {
                let path = dir.join(&k);
                std::fs::write(&path, content)?;
                env.insert(k, path.display().to_string());
            }
        }
        let args: Vec<OsString> = std::iter::once("t").chain(args.iter().copied()).map(OsString::from).collect();
        let lookup = |k: &str| env.get(k).cloned();
        let values = layered(&command(), &args, &lookup)?;
        Ok(values.apply(command()).try_get_matches_from(args)?)
    }

    fn rand_suffix() -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        N.fetch_add(1, Ordering::Relaxed)
    }

    fn get(m: &clap::ArgMatches, id: &str) -> Option<String> {
        m.get_one::<String>(id).cloned()
    }

    #[test]
    fn file_values_become_defaults_and_flags_win() {
        let file = "wake_name: Jarvis\nhome: Bergen\nnorwegian: true\nwake-spellings: [jarvis, jarvès]\nha-url:\n";
        let m = parse(&[], &[], Some(file)).unwrap();
        assert_eq!(get(&m, "wake_name").as_deref(), Some("Jarvis"));
        assert_eq!(get(&m, "home").as_deref(), Some("Bergen"));
        assert_eq!(get(&m, "norwegian").as_deref(), Some("true"));
        let spellings: Vec<&String> = m.get_many::<String>("spellings").unwrap().collect();
        assert_eq!(spellings, ["jarvis", "jarvès"]);
        assert_eq!(get(&m, "ha_url"), None);
        let m = parse(&["--home", "Tromsø"], &[], Some(file)).unwrap();
        assert_eq!(get(&m, "home").as_deref(), Some("Tromsø"));
        let args = [OsString::from("t"), OsString::from("--home=Tromsø")];
        let env = |k: &str| (k == "VOICE_WAKE_NAME").then(|| "Env".to_owned());
        let dir = std::env::temp_dir().join(format!("voice-settings-src-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("s.yaml"), file).unwrap();
        let args = [args[0].clone(), args[1].clone(), OsString::from("--config"), dir.join("s.yaml").into()];
        let names: Vec<String> =
            layered(&command(), &args, &env).unwrap().sources().into_iter().map(|(n, _)| n).collect();
        assert_eq!(names, ["norwegian", "wake-spellings"]);
    }

    #[test]
    fn unknown_settings_are_refused() {
        let error = parse(&[], &[], Some("wake_nmae: Jarvis\n")).unwrap_err();
        assert!(error.to_string().contains("unknown setting `wake_nmae`"), "{error}");
    }

    #[test]
    fn secrets_come_from_files() {
        let m = parse(&[], &[("HA_TOKEN_FILE", "file:s3cret\n")], None).unwrap();
        assert_eq!(get(&m, "ha_token").as_deref(), Some("s3cret"));
    }

    #[test]
    fn home_assistant_add_on_uses_the_supervisor() {
        let m = parse(&[], &[("SUPERVISOR_TOKEN", "sup")], None).unwrap();
        assert_eq!(get(&m, "ha_url").as_deref(), Some(SUPERVISOR_URL));
        assert_eq!(get(&m, "ha_token").as_deref(), Some("sup"));
        // An explicit Home Assistant wins over the add-on defaults.
        let m = parse(&[], &[("SUPERVISOR_TOKEN", "sup")], Some("ha-url: http://ha:8123\n")).unwrap();
        assert_eq!(get(&m, "ha_url").as_deref(), Some("http://ha:8123"));
        assert_eq!(get(&m, "ha_token"), None);
    }
}
