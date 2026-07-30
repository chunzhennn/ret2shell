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
//! Every field of the configuration file can be overridden by an environment
//! variable named `R2S_CONFIG__<SECTION>__<FIELD>`, nested fields use
//! additional `__` separators, e.g. `R2S_CONFIG__CLUSTER__REGISTRY__PASSWORD`.
//!
//! A non-empty configuration file is still required. String overrides are used
//! verbatim, non-string scalar values are parsed as their target field type,
//! and composite values use TOML syntax, e.g. `['one', 'two']` for an array. An
//! empty string is a value; it does not unset an optional field. Environment
//! variables only replace values from the file, so fields managed in the
//! database retain their existing merge priority and take precedence over
//! environment overrides.
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
  #[error("environment override failed: {0}")]
  OverrideFailed(String),
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
    // load config from config str, overridden by the environment
    Self::from_document(&config_str, std::env::vars_os())
  }

  /// Load the GlobalConfig from a configuration document, applying the
  /// overrides found in the given environment variables.
  fn from_document<I>(document: &str, variables: I) -> Result<Self, ConfigError>
  where
    I: IntoIterator<Item = (OsString, OsString)>, {
    let overrides = env::Overrides::collect(variables).map_err(Self::override_error)?;
    if overrides.is_empty() {
      return Ok(toml::from_str(document)?);
    }
    overrides
      .apply(toml::from_str::<toml::Table>(document)?)
      .map_err(Self::override_error)
  }

  fn override_error(error: env::EnvError) -> ConfigError {
    match error {
      env::EnvError::File(error) => ConfigError::DeserializeFailed(error),
      error => ConfigError::OverrideFailed(error.to_string()),
    }
  }
}

#[cfg(test)]
mod tests {
  use std::ffi::OsString;

  use super::{ConfigError, GlobalConfig};
  use crate::captcha::ValidatorType;

  const DOCUMENT: &str = r#"
[auth]
buffer_time = 21600
expires_time = 86400
signing_key = 'file-secret'

[captcha]
difficulty = 4
enabled = true
validator = 'pow'

[cluster]
enabled = true

[cluster.registry]
enabled = true
external = 'localhost:5000'
insecure = false
server = 'localhost:5000'

[server]
api_base_path = '/api'
cors_origins = '*'
external_domain = 'dev.ret.sh.cn'
external_https = true
host = '127.0.0.1'
port = 8080

[server.rate_limit]
burst_limit = 32
burst_restore_rate = 500
"#;

  fn variables(entries: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
    entries
      .iter()
      .map(|(name, value)| (OsString::from(name), OsString::from(value)))
      .collect()
  }

  #[test]
  fn file_values_are_kept_when_the_environment_is_empty() {
    let config =
      GlobalConfig::from_document(DOCUMENT, variables(&[("R2S_TOKEN", "ignored")])).unwrap();

    assert_eq!(config.auth.unwrap().signing_key, "file-secret");
    assert_eq!(config.server.unwrap().port, 8080);
  }

  #[test]
  fn overrides_are_parsed_as_the_type_of_the_field() {
    let config = GlobalConfig::from_document(
      DOCUMENT,
      variables(&[
        ("R2S_CONFIG__AUTH__SIGNING_KEY", "8080"),
        ("R2S_CONFIG__AUTH__EXPIRES_TIME", "60"),
        ("R2S_CONFIG__SERVER__PORT", "9090"),
        ("R2S_CONFIG__SERVER__EXTERNAL_HTTPS", "false"),
        ("R2S_CONFIG__CAPTCHA__VALIDATOR", "h_captcha"),
      ]),
    )
    .unwrap();

    let auth = config.auth.unwrap();
    assert_eq!(auth.signing_key, "8080");
    assert_eq!(auth.expires_time, 60);
    assert_eq!(auth.buffer_time, 21600);
    let server = config.server.unwrap();
    assert_eq!(server.port, 9090);
    assert!(!server.external_https);
    assert_eq!(config.captcha.unwrap().validator, ValidatorType::HCaptcha);
  }

  #[test]
  fn overrides_reach_nested_and_absent_fields() {
    let config = GlobalConfig::from_document(
      DOCUMENT,
      variables(&[
        ("R2S_CONFIG__CLUSTER__REGISTRY__PASSWORD", "registry-secret"),
        ("R2S_CONFIG__SERVER__RATE_LIMIT__BURST_LIMIT", "64"),
        ("R2S_CONFIG__QUEUE__HOST", "queue.internal"),
        ("R2S_CONFIG__QUEUE__TLS", "true"),
      ]),
    )
    .unwrap();

    let registry = config.cluster.unwrap().registry.unwrap();
    assert_eq!(registry.password.as_deref(), Some("registry-secret"));
    assert_eq!(registry.server, "localhost:5000");
    let rate_limit = config.server.unwrap().rate_limit.unwrap();
    assert_eq!(rate_limit.burst_limit, Some(64));
    assert_eq!(rate_limit.burst_restore_rate, Some(500));
    let queue = config.queue.unwrap();
    assert_eq!(queue.host, "queue.internal");
    assert_eq!(queue.tls, Some(true));
    assert_eq!(queue.port, None);
  }

  #[test]
  fn overrides_that_do_not_match_the_field_type_are_rejected() {
    let error = GlobalConfig::from_document(
      DOCUMENT,
      variables(&[("R2S_CONFIG__AUTH__EXPIRES_TIME", "not-a-number")]),
    )
    .unwrap_err();

    assert!(matches!(error, ConfigError::OverrideFailed(_)));
    assert!(
      error
        .to_string()
        .contains("`R2S_CONFIG__AUTH__EXPIRES_TIME`")
    );
    assert!(!error.to_string().contains("not-a-number"));
  }

  #[test]
  fn file_type_errors_remain_deserialize_failures_with_overrides() {
    let document = DOCUMENT.replace("port = 8080", "port = 'not-a-number'");
    let error = GlobalConfig::from_document(
      &document,
      variables(&[("R2S_CONFIG__AUTH__SIGNING_KEY", "environment-secret")]),
    )
    .unwrap_err();

    assert!(
      matches!(error, ConfigError::DeserializeFailed(_)),
      "{error}"
    );
  }

  #[test]
  fn overrides_that_do_not_match_any_field_are_rejected() {
    for name in [
      "R2S_CONFIG__AUTH__SIGNING_KEY_TYPO",
      "R2S_CONFIG__UNKNOWN__FIELD",
      "R2S_CONFIG__QUEUE__HOTS",
      "R2S_CONFIG__AUTH__",
    ] {
      let error = GlobalConfig::from_document(DOCUMENT, variables(&[(name, "do-not-log-this")]))
        .unwrap_err()
        .to_string();

      assert!(error.contains(name), "{error}");
      assert!(!error.contains("do-not-log-this"), "{error}");
    }
  }
}
