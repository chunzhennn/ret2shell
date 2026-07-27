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
//! Environment variables take precedence over the configuration file. The
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
    overrides.sort_unstable_by(|a, b| a.0.cmp(&b.0));

    overrides
      .into_iter()
      .try_fold(config, |config, (name, value)| {
        config.with_environment_override(&name, &value)
      })
  }

  fn with_environment_override(self, name: &str, value: &str) -> Result<Self, ConfigError> {
    let path = environment_path(name)
      .ok_or_else(|| ConfigError::InvalidEnvironmentVariable(name.to_owned()))?;
    let serialized = toml::Value::try_from(&self)?;
    let current_value = value_at_path(&serialized, &path);
    let mut candidates = Vec::new();

    match current_value {
      Some(toml::Value::String(_)) => {
        candidates.push(toml::Value::String(value.to_owned()));
      }
      Some(_) => {
        if let Some(value) = parse_toml_value(value) {
          candidates.push(value);
        }
      }
      None => {
        candidates.push(toml::Value::String(value.to_owned()));
        if let Some(value) = parse_toml_value(value)
          && candidates.first() != Some(&value)
        {
          candidates.push(value);
        }
      }
    }

    for value in candidates {
      let mut candidate = serialized.clone();
      if !insert_value_at_path(&mut candidate, &path, value) {
        continue;
      }
      let Ok(config) = candidate.try_into::<GlobalConfig>() else {
        continue;
      };
      let serialized = toml::Value::try_from(&config)?;
      if value_at_path(&serialized, &path).is_some() {
        return Ok(config);
      }
    }

    Err(ConfigError::InvalidEnvironmentVariable(name.to_owned()))
  }
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
  toml::from_str::<toml::Table>(&format!("value = {value}"))
    .ok()?
    .remove("value")
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
