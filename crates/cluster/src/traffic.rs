use k8s_openapi::api::core::v1::{Pod, Service};
use kube::ResourceExt;
use r2s_engine::{DiagnosticMarker, Engine, EngineError};
use rune::{Any, ContextError, Module, Value, alloc::clone::TryClone, runtime::Object};
use serde::{Deserialize, Serialize};

use crate::ClusterError;

#[derive(Clone, Debug, Default)]
pub struct TrafficMapper;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MappedPort {
  pub name: String,
  pub address: String,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub scheme: Option<String>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub server_name: Option<String>,
}

/// A cache entry records its script so updates take effect immediately,
/// including across platform replicas. Keep the traffic ID key for lifecycle
/// invalidation.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CachedPorts {
  pub script: String,
  pub ports: Vec<MappedPort>,
}

#[derive(TryClone, Debug, Any)]
#[rune(item = ::ret2shell::cluster)]
pub struct RunePortInfo {
  #[rune(get)]
  pub name: String,
  #[rune(get)]
  pub node_port: u16,
  #[rune(get)]
  pub port: u16,
  #[rune(get)]
  pub protocol: String,
  #[rune(get)]
  pub app_protocol: String,
}

#[derive(Debug, Any)]
#[rune(item = ::ret2shell::cluster)]
pub struct RuneServiceInfo {
  #[rune(get)]
  pub traffic: String,
  #[rune(get)]
  pub route_id: String,
  #[rune(get)]
  pub created_at: i64,
  #[rune(get)]
  pub lifetime: u64,
  #[rune(get)]
  pub ports: rune::alloc::Vec<RunePortInfo>,
}

impl RuneServiceInfo {
  pub fn try_from_service(service: &Service, pod: &Pod) -> Result<Self, ClusterError> {
    let renew = pod
      .metadata
      .annotations
      .clone()
      .unwrap_or_default()
      .get("ret.sh.cn/renew")
      .map(|v| v.parse::<i32>().unwrap_or(0))
      .unwrap_or(0);
    let lifetime: u64 = ((renew + 1) * 3600) as u64;
    let created_at = pod
      .metadata
      .creation_timestamp
      .clone()
      .unwrap()
      .0
      .as_second();
    let mut ports_info = Vec::new();
    for port in service.spec.as_ref().unwrap().ports.as_ref().unwrap() {
      let port_info = RunePortInfo {
        name: port.name.clone().unwrap_or("default".to_owned()),
        node_port: port.node_port.unwrap_or(0) as u16,
        port: port.port as u16,
        protocol: port.protocol.clone().unwrap_or("TCP".to_owned()),
        app_protocol: port
          .app_protocol
          .clone()
          .unwrap_or("tcp".to_owned())
          .replace("ret.sh.cn/traffic-", ""),
      };
      ports_info.push(port_info);
    }

    let traffic = service
      .labels()
      .get("ret.sh.cn/traffic")
      .ok_or(ClusterError::MissingField("traffic".to_string()))?
      .to_owned();
    Ok(Self {
      route_id: crate::gateway::route_id(&traffic)?,
      traffic,
      created_at,
      lifetime,
      ports: ports_info.try_into().map_err(EngineError::from)?,
    })
  }
}

#[rune::module(::ret2shell::cluster)]
fn module(_stdio: bool) -> Result<Module, ContextError> {
  let mut module = Module::from_meta(self::module_meta)?;
  module.ty::<RunePortInfo>()?;
  module.ty::<RuneServiceInfo>()?;
  Ok(module)
}

impl TrafficMapper {
  fn default_modules() -> Vec<fn(bool) -> Result<rune::Module, rune::ContextError>> {
    vec![
      rune_modules::http::module,
      rune_modules::json::module,
      rune_modules::toml::module,
      rune_modules::rand::module,
      rune_modules::process::module,
      module,
    ]
  }

  pub async fn expire(&self, engine: &Engine, key: impl AsRef<str>) {
    engine
      .expire_prefix(format!("traffic-{}-", key.as_ref()))
      .await
  }

  /// linter for rune scripts
  /// Originally from https://github.com/ElaBosak233/cdsctf/blob/main/crates/checker/src/traits.rs
  pub async fn lint(&self, script: impl AsRef<str>) -> Result<Vec<DiagnosticMarker>, EngineError> {
    Engine::lint(Self::default_modules(), script, &["expose"]).await
  }

  pub async fn preload(
    &self, engine: &Engine, key: impl AsRef<str>, script: impl AsRef<str>,
  ) -> Result<(), EngineError> {
    let key = self.script_key(key.as_ref(), script.as_ref());
    engine
      .preload(Self::default_modules(), key, script, None)
      .await
  }

  fn script_key(&self, key: &str, script: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    script.hash(&mut hasher);
    format!("traffic-{key}-{:016x}", hasher.finish())
  }

  pub async fn expose(
    &self, engine: &Engine, key: impl AsRef<str>, script: &str, pod: Pod, service: Service,
  ) -> Result<Vec<MappedPort>, ClusterError> {
    let key = self.script_key(key.as_ref(), script);

    let service_info = RuneServiceInfo::try_from_service(&service, &pod)?;
    let node_name = pod
      .spec
      .ok_or(ClusterError::MissingField("pod::spec".to_owned()))?
      .node_name
      .ok_or(ClusterError::MissingField("pod::node_name".to_owned()))?;

    let output = engine
      .execute(key, "expose", (node_name, service_info))
      .await?;

    let output: Result<Object, Value> = rune::from_value(output).map_err(EngineError::from)?;
    let mut result = Vec::new();
    if let Ok(object) = output {
      for (key, value) in object.iter() {
        result.push(parse_endpoint(key, value.clone())?);
      }
      Ok(result)
    } else {
      Err(EngineError::ScriptError("early returns from script".to_owned()).into())
    }
  }
}

fn parse_endpoint(name: &str, value: Value) -> Result<MappedPort, ClusterError> {
  if let Ok(address) = rune::from_value::<String>(value.clone()) {
    return Ok(MappedPort {
      name: name.into(),
      address,
      scheme: None,
      server_name: None,
    });
  }
  let object: Object = rune::from_value(value).map_err(EngineError::from)?;
  let get = |key: &str| -> Result<Option<String>, ClusterError> {
    object
      .get(key)
      .map(|v| {
        rune::from_value::<String>(v.clone())
          .map_err(EngineError::from)
          .map_err(Into::into)
      })
      .transpose()
  };
  let address =
    get("address")?.ok_or_else(|| ClusterError::MissingField("endpoint address".into()))?;
  let scheme = get("scheme")?;
  if scheme
    .as_deref()
    .is_some_and(|s| !matches!(s, "http" | "https" | "tcp" | "tls" | "udp"))
  {
    return Err(EngineError::ScriptError("unsupported endpoint scheme".into()).into());
  }
  Ok(MappedPort {
    name: name.into(),
    address,
    scheme,
    server_name: get("server_name")?,
  })
}

#[cfg(test)]
mod tests {
  use serde_json::json;

  use super::*;

  fn fixtures() -> (Pod, Service) {
    let pod = serde_json::from_value(json!({
      "metadata": {"creationTimestamp": "2026-09-24T00:00:00Z"},
      "spec": {"containers": [], "nodeName": "test-node"}
    }))
    .unwrap();
    let service = serde_json::from_value(json!({
      "metadata": {"labels": {"ret.sh.cn/traffic": "aB12"}},
      "spec": {"ports": [{"name": "pwn", "port": 9999}]}
    }))
    .unwrap();
    (pod, service)
  }

  #[tokio::test]
  async fn legacy_and_structured_scripts_work_after_an_update() {
    let mapper = TrafficMapper;
    let engine = Engine::default();
    let legacy = r#"pub async fn expose(_node, service) { Ok(#{pwn: `old.example.com:${service.ports[0].port}`}) }"#;
    mapper.preload(&engine, "same-key", legacy).await.unwrap();
    let (pod, service) = fixtures();
    let result = mapper
      .expose(&engine, "same-key", legacy, pod, service)
      .await
      .unwrap();
    assert_eq!(result[0].address, "old.example.com:9999");
    assert!(result[0].scheme.is_none());
    let updated = r#"pub async fn expose(_node, service) {
      Ok(#{pwn: #{address: `${service.route_id}.example.com:443`, scheme: "tls", server_name: "pwn.example.com"}})
    }"#;
    mapper.preload(&engine, "same-key", updated).await.unwrap();
    let (pod, service) = fixtures();
    let result = mapper
      .expose(&engine, "same-key", updated, pod, service)
      .await
      .unwrap();
    assert_eq!(result[0].address, "61423132.example.com:443");
    assert_eq!(result[0].scheme.as_deref(), Some("tls"));
    assert_eq!(result[0].server_name.as_deref(), Some("pwn.example.com"));
    mapper.expire(&engine, "same-key").await;
    let (pod, service) = fixtures();
    assert!(
      mapper
        .expose(&engine, "same-key", updated, pod, service)
        .await
        .is_err()
    );
  }

  #[test]
  fn old_cached_endpoint_remains_deserializable() {
    let port: MappedPort = serde_json::from_str(r#"{"name":"web","address":"host:80"}"#).unwrap();
    assert!(port.scheme.is_none());
  }
}
