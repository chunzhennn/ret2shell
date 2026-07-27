//! This module loads the configuration from the file and provided them to other
//! modules.
//!
//! # Configuration file
//!
//! We use `toml` crate to parse configuration file.
//!
//! The configuration file will be looked up in the following order:
//!
//! 1. `./config/config.toml`: the current working directory
//! 2. `~/.config/ret2shell/config.toml`: the user's XDG config directory
//! 3. `/etc/ret2shell/config.toml`: the system config directory
//!
//! `ret2shell` server will try them in order and use the first one it found.
//!
//! # Environment overrides
//!
//! After loading the configuration file, any field can be overridden with an
//! environment variable named `R2S_CONFIG__<SECTION>__<FIELD>`. Nested fields
//! use additional `__` separators, for example
//! `R2S_CONFIG__CLUSTER__REGISTRY__PASSWORD`.
//!
//! Values are interpreted as TOML whenever the targeted field is not a string,
//! so booleans and numbers must be spelled the way TOML spells them: `true`,
//! not `1` or `TRUE`. An inline table replaces a whole table at once, as in
//! `R2S_CONFIG__SERVER__RATE_LIMIT='{ burst_limit = 64 }'` — the table is
//! replaced, not merged into. An empty variable is an ordinary value rather
//! than an absent one, so `R2S_CONFIG__AUTH__SIGNING_KEY=` sets an empty
//! signing key instead of keeping the one from the file; beware of templates
//! that render unset values as empty strings.
//!
//! Variables that do not name an existing setting are rejected, so a typo
//! stops the server instead of silently leaving the file value in place.
//!
//! Environment variables take precedence over the configuration file, but not
//! over the configuration stored in the database: settings that are editable
//! from the admin panel are still resolved in favour of the database. The
//! configuration file is still required and must contain a valid base
//! configuration.
//!
//! # Management
//!
//! In previous Cyber Terminal implementations, the config file could be
//! modified on-the-fly and the server will reload the configuration
//! automatically. This affects the ability to implement cluster deployment and
//! load balancing on the server, so we removed this feature on `ret2shell`. The
//! configuration file will be readonly after the server started.
//!
//! If you want to change the configuration, you should manually edit it through
//! DevOps tools then restart the server.
//!
//! For convenience, we move some configurations into the database, so that you
//! can still change them through the web interface.
use std::{ffi::OsString, path::Path};

use serde::{Deserialize, Serialize};
use thiserror::Error;
pub mod auditor;
pub mod auth;
pub mod bucket;
pub mod cache;
pub mod captcha;
pub mod cluster;
pub mod database;
pub mod email;
pub mod logging;
pub mod media;
pub mod queue;
pub mod server;
pub mod traits;

#[derive(Error, Debug)]
pub enum ConfigError {
  #[error("configuration file not found")]
  NotFound,
  #[error("configuration file is invalid")]
  Invalid,
  #[error("deserialize failed: {0}")]
  DeserializeFailed(#[from] toml::de::Error),
  #[error("serialize failed: {0}")]
  SerializeFailed(#[from] toml::ser::Error),
  #[error("configuration environment variable `{0}` is invalid")]
  InvalidEnvironmentVariable(String),
}

/// Represents the configuration for the whole application.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GlobalConfig {
  pub auditor: Option<auditor::Config>,
  pub auth: Option<auth::Config>,
  pub bucket: Option<bucket::Config>,
  pub cache: Option<cache::Config>,
  pub captcha: Option<captcha::Config>,
  pub cluster: Option<cluster::Config>,
  pub database: Option<database::Config>,
  pub email: Option<email::Config>,
  pub logging: Option<logging::Config>,
  pub media: Option<media::Config>,
  pub queue: Option<queue::Config>,
  pub server: Option<server::Config>,
}

// Predefined paths for the configuration file.
const CONFIG_PREDEFINED_PATH: [&str; 3] = ["/etc/ret2shell/", "~/.config/ret2shell/", "./config/"];

// Predefined file name for the configuration file.
const CONFIG_PREDEFINED_FILE_NAME: &str = "config.toml";

// Prefix and nesting separator for configuration environment variables.
const CONFIG_ENV_PREFIX: &str = "R2S_CONFIG__";
const CONFIG_ENV_SEPARATOR: &str = "__";

// Upper bound on the interpretations tried when overrides target fields that
// are absent from the configuration file and therefore give no type hint.
const CONFIG_ENV_MAX_ATTEMPTS: usize = 512;

impl GlobalConfig {
  /// Load the GlobalConfig from a configuration file.
  /// It searches for the configuration file in predefined paths and returns
  /// the loaded configuration.
  pub fn load() -> Result<Self, ConfigError> {
    // load config str from predefined paths
    let mut config_str = String::new();
    let mut ok = false;
    for path in CONFIG_PREDEFINED_PATH.iter() {
      let path = match Path::new(path).canonicalize() {
        Ok(p) => p,
        Err(_) => {
          // println!("[stage 1] config path error: {err:?}, original path: {path}");
          continue;
        }
      };
      // println!("config file path is: {path:?}");
      let file_path = path.join(CONFIG_PREDEFINED_FILE_NAME);
      match std::fs::read_to_string(&file_path) {
        Ok(s) => {
          config_str = s;
          ok = true;
          break;
        }
        Err(_) => {
          // println!("[stage 2] config path error: {err:?}, original path: {path:?}");
          continue;
        }
      }
    }
    if !ok || config_str.is_empty() {
      return Err(ConfigError::NotFound);
    }
    Self::from_toml_with_environment(&config_str, std::env::vars_os())
  }

  fn from_toml_with_environment<I>(config_str: &str, environment: I) -> Result<Self, ConfigError>
  where
    I: IntoIterator<Item = (OsString, OsString)>, {
    let config: GlobalConfig = toml::from_str(config_str)?;
    let mut overrides = Vec::new();

    for (name, value) in environment {
      let Ok(name) = name.into_string() else {
        continue;
      };
      if !name.starts_with(CONFIG_ENV_PREFIX) {
        continue;
      }
      let value = value
        .into_string()
        .map_err(|_| ConfigError::InvalidEnvironmentVariable(name.clone()))?;
      overrides.push((name, value));
    }
    if overrides.is_empty() {
      return Ok(config);
    }
    overrides.sort_unstable_by(|a, b| a.0.cmp(&b.0));

    config.with_environment_overrides(&overrides)
  }

  /// Applies every override to a single document and deserializes it once, so
  /// that variables describing the same section are validated together instead
  /// of requiring each intermediate state to be valid on its own.
  fn with_environment_overrides(self, overrides: &[(String, String)]) -> Result<Self, ConfigError> {
    let base = toml::Value::try_from(&self)?;
    let plan = EnvironmentOverrides::resolve(&base, overrides)?;
    let ambiguous = plan.ambiguous();
    let mut positions = Vec::new();
    let mut fallback = Vec::new();
    let mut attempts = 0;

    'search: for size in 0..=ambiguous.len() {
      positions.clear();
      positions.extend(0..size);
      loop {
        fallback.clear();
        fallback.extend(positions.iter().map(|position| ambiguous[*position]));
        if let Some(config) = plan
          .document(&base, &fallback)
          .and_then(|document| document.try_into::<GlobalConfig>().ok())
        {
          // A path that does not survive the round trip is not part of the
          // configuration schema, which means the variable name is misspelled.
          let serialized = toml::Value::try_from(&config)?;
          return match plan.unknown(&serialized) {
            Some(index) => Err(ConfigError::InvalidEnvironmentVariable(
              overrides[index].0.clone(),
            )),
            None => Ok(config),
          };
        }
        attempts += 1;
        if attempts >= CONFIG_ENV_MAX_ATTEMPTS {
          break 'search;
        }
        if !advance_combination(&mut positions, ambiguous.len()) {
          break;
        }
      }
    }

    Err(ConfigError::InvalidEnvironmentVariable(
      overrides[plan.culprit(&base)].0.clone(),
    ))
  }
}

/// Interpretation of each environment override, kept apart from the document so
/// that ambiguous values can be reinterpreted without parsing them again.
struct EnvironmentOverrides {
  paths: Vec<Vec<String>>,
  values: Vec<toml::Value>,
  /// Second interpretation for values whose field is absent from the
  /// configuration file and therefore carries no type information.
  fallbacks: Vec<Option<toml::Value>>,
}

impl EnvironmentOverrides {
  fn resolve(base: &toml::Value, overrides: &[(String, String)]) -> Result<Self, ConfigError> {
    let mut plan = Self {
      paths: Vec::with_capacity(overrides.len()),
      values: Vec::with_capacity(overrides.len()),
      fallbacks: Vec::with_capacity(overrides.len()),
    };
    let mut document = base.clone();

    for (name, value) in overrides {
      let path = environment_path(name)
        .ok_or_else(|| ConfigError::InvalidEnvironmentVariable(name.clone()))?;
      let text = toml::Value::String(value.clone());
      let (resolved, fallback) = match value_at_path(&document, &path) {
        Some(toml::Value::String(_)) => (text, None),
        Some(_) => (
          parse_toml_value(value)
            .ok_or_else(|| ConfigError::InvalidEnvironmentVariable(name.clone()))?,
          None,
        ),
        None => match parse_toml_value(value) {
          Some(parsed) if parsed != text => (parsed, Some(text)),
          _ => (text, None),
        },
      };
      if !insert_value_at_path(&mut document, &path, resolved.clone()) {
        return Err(ConfigError::InvalidEnvironmentVariable(name.clone()));
      }
      plan.paths.push(path);
      plan.values.push(resolved);
      plan.fallbacks.push(fallback);
    }

    Ok(plan)
  }

  fn ambiguous(&self) -> Vec<usize> {
    self
      .fallbacks
      .iter()
      .enumerate()
      .filter_map(|(index, fallback)| fallback.is_some().then_some(index))
      .collect()
  }

  /// Rebuilds the overridden document, reinterpreting the overrides listed in
  /// `fallback` as plain strings.
  fn document(&self, base: &toml::Value, fallback: &[usize]) -> Option<toml::Value> {
    let mut document = base.clone();

    for (index, path) in self.paths.iter().enumerate() {
      let value = if fallback.contains(&index) {
        self.fallbacks[index].clone()?
      } else {
        self.values[index].clone()
      };
      if !insert_value_at_path(&mut document, path, value) {
        return None;
      }
    }

    Some(document)
  }

  fn unknown(&self, document: &toml::Value) -> Option<usize> {
    self
      .paths
      .iter()
      .position(|path| value_at_path(document, path).is_none())
  }

  /// Points at the override that cannot be applied on its own, which is the
  /// most likely reason for a batch that never deserializes. Falls back to the
  /// first override when each one is valid alone but the batch is not.
  fn culprit(&self, base: &toml::Value) -> usize {
    self
      .paths
      .iter()
      .enumerate()
      .find(|(index, path)| {
        ![Some(&self.values[*index]), self.fallbacks[*index].as_ref()]
          .into_iter()
          .flatten()
          .any(|value| {
            let mut document = base.clone();
            insert_value_at_path(&mut document, path, value.clone())
              && document.try_into::<GlobalConfig>().is_ok()
          })
      })
      .map_or(0, |(index, _)| index)
  }
}

/// Advances `positions` to the next combination of the same size taken from
/// `total` items, in lexicographic order. Returns `false` once the last
/// combination has been visited.
fn advance_combination(positions: &mut [usize], total: usize) -> bool {
  let size = positions.len();
  if size == 0 || size > total {
    return false;
  }
  let mut index = size;

  while index > 0 {
    index -= 1;
    if positions[index] < total - size + index {
      positions[index] += 1;
      for next in index + 1..size {
        positions[next] = positions[next - 1] + 1;
      }
      return true;
    }
  }

  false
}

fn environment_path(name: &str) -> Option<Vec<String>> {
  let path = name
    .strip_prefix(CONFIG_ENV_PREFIX)?
    .split(CONFIG_ENV_SEPARATOR)
    .map(|segment| {
      if segment.is_empty()
        || !segment
          .chars()
          .all(|character| character.is_ascii_alphanumeric() || character == '_')
      {
        return None;
      }
      Some(segment.to_ascii_lowercase())
    })
    .collect::<Option<Vec<_>>>()?;

  (path.len() >= 2).then_some(path)
}

fn parse_toml_value(value: &str) -> Option<toml::Value> {
  let mut table = toml::from_str::<toml::Table>(&format!("value = {value}")).ok()?;
  // Reject values that smuggle extra entries past the wrapper, such as
  // `8080\nport = 1`, instead of silently keeping only the first one.
  if table.len() != 1 {
    return None;
  }

  table.remove("value")
}

fn value_at_path<'a>(value: &'a toml::Value, path: &[String]) -> Option<&'a toml::Value> {
  path
    .iter()
    .try_fold(value, |value, segment| value.as_table()?.get(segment))
}

fn insert_value_at_path(value: &mut toml::Value, path: &[String], new_value: toml::Value) -> bool {
  let Some((field, parents)) = path.split_last() else {
    return false;
  };
  let mut current = value;

  for segment in parents {
    let Some(table) = current.as_table_mut() else {
      return false;
    };
    current = table
      .entry(segment)
      .or_insert_with(|| toml::Value::Table(toml::Table::new()));
  }

  let Some(table) = current.as_table_mut() else {
    return false;
  };
  table.insert(field.clone(), new_value);
  true
}

#[cfg(test)]
mod tests {
  use std::ffi::OsString;

  use super::{ConfigError, GlobalConfig};

  const CONFIG: &str = r#"
[auth]
buffer_time = 21600
expires_time = 86400
signing_key = "file-secret"

[queue]
host = "127.0.0.1"

[server]
api_base_path = "/api"
cors_origins = "*"
external_domain = "ret2shell.example"
external_https = true
host = "0.0.0.0"
port = 8080

[server.rate_limit]
burst_limit = 32
burst_restore_rate = 500
"#;

  fn environment(variables: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
    variables
      .iter()
      .map(|(name, value)| (OsString::from(name), OsString::from(value)))
      .collect()
  }

  #[test]
  fn environment_overrides_file_values_and_parses_field_types() {
    let config = GlobalConfig::from_toml_with_environment(
      CONFIG,
      environment(&[
        ("R2S_CONFIG__AUTH__SIGNING_KEY", "true"),
        ("R2S_CONFIG__AUTH__EXPIRES_TIME", "60"),
        ("R2S_CONFIG__QUEUE__TLS", "true"),
        ("R2S_CONFIG__SERVER__RATE_LIMIT__BURST_LIMIT", "64"),
      ]),
    )
    .unwrap();

    let auth = config.auth.unwrap();
    assert_eq!(auth.signing_key, "true");
    assert_eq!(auth.expires_time, 60);
    assert_eq!(config.queue.unwrap().tls, Some(true));
    assert_eq!(
      config.server.unwrap().rate_limit.unwrap().burst_limit,
      Some(64)
    );
  }

  #[test]
  fn environment_builds_a_section_that_is_absent_from_the_file() {
    let config = GlobalConfig::from_toml_with_environment(
      CONFIG,
      environment(&[
        ("R2S_CONFIG__CLUSTER__ENABLED", "true"),
        ("R2S_CONFIG__CLUSTER__REGISTRY__EXTERNAL", "reg.example"),
        ("R2S_CONFIG__CLUSTER__REGISTRY__INSECURE", "false"),
        ("R2S_CONFIG__CLUSTER__REGISTRY__PASSWORD", "hunter2"),
        ("R2S_CONFIG__CLUSTER__REGISTRY__SERVER", "reg.example"),
      ]),
    )
    .unwrap();

    let cluster = config.cluster.unwrap();
    assert!(cluster.enabled);
    let registry = cluster.registry.unwrap();
    assert_eq!(registry.server, "reg.example");
    assert_eq!(registry.external, "reg.example");
    assert!(!registry.insecure);
    assert_eq!(registry.password.as_deref(), Some("hunter2"));
  }

  #[test]
  fn absent_string_field_keeps_a_value_that_looks_like_a_number() {
    let config = GlobalConfig::from_toml_with_environment(
      CONFIG,
      environment(&[
        ("R2S_CONFIG__QUEUE__PASSWORD", "12345678"),
        ("R2S_CONFIG__QUEUE__PING_INTERVAL", "5"),
        ("R2S_CONFIG__QUEUE__TLS", "true"),
        ("R2S_CONFIG__QUEUE__TOKEN", "87654321"),
      ]),
    )
    .unwrap();

    let queue = config.queue.unwrap();
    assert_eq!(queue.password.as_deref(), Some("12345678"));
    assert_eq!(queue.token.as_deref(), Some("87654321"));
    assert_eq!(queue.ping_interval, Some(5));
    assert_eq!(queue.tls, Some(true));
  }

  #[test]
  fn rejected_batch_names_the_offending_variable() {
    let error = GlobalConfig::from_toml_with_environment(
      CONFIG,
      environment(&[
        ("R2S_CONFIG__SERVER__EXTERNAL_DOMAIN", "ctf.example"),
        ("R2S_CONFIG__SERVER__PORT", "70000"),
      ]),
    )
    .unwrap_err();

    assert!(matches!(
      error,
      ConfigError::InvalidEnvironmentVariable(name)
        if name == "R2S_CONFIG__SERVER__PORT"
    ));
  }

  #[test]
  fn value_carrying_extra_entries_is_rejected() {
    let error = GlobalConfig::from_toml_with_environment(
      CONFIG,
      environment(&[("R2S_CONFIG__SERVER__PORT", "8081\nhost = 'evil'")]),
    )
    .unwrap_err();

    assert!(matches!(
      error,
      ConfigError::InvalidEnvironmentVariable(name)
        if name == "R2S_CONFIG__SERVER__PORT"
    ));
  }

  #[test]
  fn empty_value_overrides_with_an_empty_value() {
    let config = GlobalConfig::from_toml_with_environment(
      CONFIG,
      environment(&[("R2S_CONFIG__AUTH__SIGNING_KEY", "")]),
    )
    .unwrap();

    assert_eq!(config.auth.unwrap().signing_key, "");
  }

  #[test]
  fn inline_table_replaces_a_whole_table() {
    let config = GlobalConfig::from_toml_with_environment(
      CONFIG,
      environment(&[(
        "R2S_CONFIG__SERVER__RATE_LIMIT",
        "{ burst_limit = 64, burst_restore_rate = 250 }",
      )]),
    )
    .unwrap();

    let rate_limit = config.server.unwrap().rate_limit.unwrap();
    assert_eq!(rate_limit.burst_limit, Some(64));
    assert_eq!(rate_limit.burst_restore_rate, Some(250));
  }

  #[test]
  fn unrelated_environment_variables_are_ignored() {
    let config =
      GlobalConfig::from_toml_with_environment(CONFIG, environment(&[("R2S_TOKEN", "ignored")]))
        .unwrap();

    assert_eq!(config.auth.unwrap().signing_key, "file-secret");
  }

  #[test]
  fn unknown_environment_path_is_rejected_without_exposing_value() {
    let error = GlobalConfig::from_toml_with_environment(
      CONFIG,
      environment(&[("R2S_CONFIG__AUTH__SIGNING_KEY_TYPO", "do-not-log-this")]),
    )
    .unwrap_err();

    assert!(matches!(
      error,
      ConfigError::InvalidEnvironmentVariable(ref name)
        if name == "R2S_CONFIG__AUTH__SIGNING_KEY_TYPO"
    ));
    assert!(!error.to_string().contains("do-not-log-this"));
  }

  #[test]
  fn invalid_environment_value_type_is_rejected() {
    let error = GlobalConfig::from_toml_with_environment(
      CONFIG,
      environment(&[("R2S_CONFIG__AUTH__EXPIRES_TIME", "not-a-number")]),
    )
    .unwrap_err();

    assert!(matches!(
      error,
      ConfigError::InvalidEnvironmentVariable(name)
        if name == "R2S_CONFIG__AUTH__EXPIRES_TIME"
    ));
  }
}
