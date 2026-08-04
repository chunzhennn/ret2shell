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
//! # Environment variables
//!
//! A configuration value can reference an environment variable with an exact
//! `${ENVIRONMENT_VARIABLE}` placeholder. Quote the placeholder when the
//! configuration field is a string, or leave it bare when the variable
//! contains another TOML value such as an integer or boolean:
//!
//! ```toml
//! signing_key = '${AUTH_SIGNING_KEY}'
//! port = ${SERVER_PORT}
//! external_https = ${EXTERNAL_HTTPS}
//! ```
//!
//! Referenced variables are required. Bare values must be valid TOML and all
//! resolved values must match the type of their configuration field.
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
mod env;
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
  #[error("environment variable `{0}` referenced by the configuration is not set")]
  EnvironmentNotFound(String),
  #[error("environment variable `{0}` is not valid UTF-8")]
  EnvironmentNotUnicode(String),
  #[error("environment variable `{0}` must contain a valid TOML value")]
  EnvironmentValueInvalid(String),
  #[error("environment value(s) for `{0}` do not match the configuration field types")]
  EnvironmentTypeMismatch(String),
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
    Self::from_document(&config_str, std::env::vars_os())
  }

  fn from_document<I>(document: &str, variables: I) -> Result<Self, ConfigError>
  where
    I: IntoIterator<Item = (OsString, OsString)>, {
    let (resolved, referenced_variables) =
      env::resolve(document, variables).map_err(|error| match error {
        env::Error::Document(error) => ConfigError::DeserializeFailed(error),
        env::Error::NotFound(name) => ConfigError::EnvironmentNotFound(name),
        env::Error::NotUnicode(name) => ConfigError::EnvironmentNotUnicode(name),
        env::Error::InvalidValue(name) => ConfigError::EnvironmentValueInvalid(name),
      })?;
    resolved.try_into().map_err(|error| {
      if referenced_variables.is_empty() {
        ConfigError::DeserializeFailed(error)
      } else {
        ConfigError::EnvironmentTypeMismatch(referenced_variables.join("`, `"))
      }
    })
  }
}

#[cfg(test)]
mod tests {
  use std::ffi::OsString;

  use super::{ConfigError, GlobalConfig};
  use crate::captcha::ValidatorType;

  const DOCUMENT: &str = r#"
[auth]
signing_key = '${AUTH_SIGNING_KEY}'
buffer_time = ${AUTH_BUFFER_TIME}
expires_time = 86400

[captcha]
enabled = ${CAPTCHA_ENABLED}
difficulty = ${CAPTCHA_DIFFICULTY}
validator = '${CAPTCHA_VALIDATOR}'

[server]
api_base_path = '/api'
cors_origins = '*'
external_domain = '${EXTERNAL_DOMAIN}'
external_https = ${EXTERNAL_HTTPS}
host = '127.0.0.1'
port = ${SERVER_PORT}
"#;

  fn variables(entries: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
    entries
      .iter()
      .map(|(name, value)| (OsString::from(name), OsString::from(value)))
      .collect()
  }

  fn valid_variables() -> Vec<(OsString, OsString)> {
    variables(&[
      ("AUTH_SIGNING_KEY", "8080's secret\nsecond line"),
      ("AUTH_BUFFER_TIME", "60"),
      ("CAPTCHA_ENABLED", "true"),
      ("CAPTCHA_DIFFICULTY", "4"),
      ("CAPTCHA_VALIDATOR", "h_captcha"),
      ("EXTERNAL_DOMAIN", "ctf.example.com"),
      ("EXTERNAL_HTTPS", "false"),
      ("SERVER_PORT", "9090"),
    ])
  }

  #[test]
  fn resolves_quoted_strings_and_typed_bare_values() {
    let config = GlobalConfig::from_document(DOCUMENT, valid_variables()).unwrap();

    let auth = config.auth.unwrap();
    assert_eq!(auth.signing_key, "8080's secret\nsecond line");
    assert_eq!(auth.buffer_time, 60);

    let captcha = config.captcha.unwrap();
    assert!(captcha.enabled);
    assert_eq!(captcha.difficulty, Some(4));
    assert_eq!(captcha.validator, ValidatorType::HCaptcha);

    let server = config.server.unwrap();
    assert_eq!(server.external_domain, "ctf.example.com");
    assert!(!server.external_https);
    assert_eq!(server.port, 9090);
  }

  #[test]
  fn ignores_placeholders_in_comments_and_larger_strings() {
    let document = r#"
# ignored = ${COMMENT_ONLY}
[auth]
signing_key = 'prefix-${NOT_A_PLACEHOLDER}'
buffer_time = 60
expires_time = 120
"#;
    let config = GlobalConfig::from_document(document, variables(&[])).unwrap();

    assert_eq!(
      config.auth.unwrap().signing_key,
      "prefix-${NOT_A_PLACEHOLDER}"
    );
  }

  #[test]
  fn requires_referenced_environment_variables() {
    let document = r#"
[auth]
signing_key = '${AUTH_SIGNING_KEY}'
buffer_time = 60
expires_time = 120
"#;
    let error = GlobalConfig::from_document(document, variables(&[])).unwrap_err();

    assert!(matches!(error, ConfigError::EnvironmentNotFound(_)));
    assert!(error.to_string().contains("AUTH_SIGNING_KEY"));
  }

  #[test]
  fn rejects_bare_values_that_are_not_toml() {
    for invalid in ["not-a-number", "9090\nextra = true"] {
      let mut variables = valid_variables();
      variables.retain(|(name, _)| name != "SERVER_PORT");
      variables.push(("SERVER_PORT".into(), invalid.into()));

      let error = GlobalConfig::from_document(DOCUMENT, variables).unwrap_err();

      assert!(matches!(error, ConfigError::EnvironmentValueInvalid(_)));
      assert!(error.to_string().contains("SERVER_PORT"));
      assert!(!error.to_string().contains(invalid));
    }
  }

  #[test]
  fn rejects_values_that_do_not_match_the_config_field_type() {
    let mut variables = valid_variables();
    variables.retain(|(name, _)| name != "SERVER_PORT");
    variables.push(("SERVER_PORT".into(), "true".into()));

    let error = GlobalConfig::from_document(DOCUMENT, variables).unwrap_err();

    assert!(matches!(error, ConfigError::EnvironmentTypeMismatch(_)));
    assert!(error.to_string().contains("SERVER_PORT"));
    assert!(!error.to_string().contains("true"));
  }

  #[test]
  fn preserves_deserialize_errors_without_environment_placeholders() {
    let document = r#"
[auth]
signing_key = 'secret'
buffer_time = 'not-a-number'
expires_time = 120
"#;
    let error = GlobalConfig::from_document(document, variables(&[])).unwrap_err();

    assert!(matches!(error, ConfigError::DeserializeFailed(_)));
  }
}
