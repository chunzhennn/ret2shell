//! Optional Traefik route reconciliation. Only explicitly labelled challenge
//! Services are managed; no listener, certificate, CRD, or workload is
//! installed.
use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::Service;
use kube::{
  Api, ResourceExt,
  api::{ApiResource, DynamicObject, GroupVersionKind, ListParams, Patch, PatchParams, PostParams},
};
use r2s_config::cluster::TlsGatewayConfig;
use serde_json::{Value, json};
use tracing::warn;

use crate::{CHALLENGE_NS, Cluster, ClusterError, traffic::MappedPort};

pub const GATEWAY_LABEL: &str = "ret.sh.cn/gateway";
pub const CONFIG_ANNOTATION: &str = "ret.sh.cn/tls-gateway";
pub const STATUS_ANNOTATION: &str = "ret.sh.cn/gateway-status";
const CONTROLLER_LABEL: &str = "ret.sh.cn/gateway-controller";
const CONTROLLER: &str = "ret2shell-traefik";

pub fn is_gateway_service(service: &Service) -> bool {
  service
    .labels()
    .get(GATEWAY_LABEL)
    .is_some_and(|v| v == "tls")
}

/// Preserve the case-sensitive traffic token while using a single DNS label.
pub fn route_id(traffic: &str) -> Result<String, ClusterError> {
  if traffic.is_empty() || traffic.len() > 26 || !traffic.bytes().all(|b| b.is_ascii_alphanumeric())
  {
    return Err(ClusterError::GatewayConfig(
      "invalid traffic token for gateway hostname".into(),
    ));
  }
  Ok(traffic.bytes().map(|b| format!("{b:02x}")).collect())
}

pub fn service_config(service: &Service) -> Result<TlsGatewayConfig, ClusterError> {
  let raw = service
    .annotations()
    .get(CONFIG_ANNOTATION)
    .ok_or_else(|| ClusterError::MissingField(CONFIG_ANNOTATION.into()))?;
  let config: TlsGatewayConfig = serde_json::from_str(raw)?;
  config.validate().map_err(ClusterError::GatewayConfig)?;
  Ok(config)
}

pub fn mapped_ports(service: &Service) -> Result<Vec<MappedPort>, ClusterError> {
  let config = service_config(service)?;
  let traffic = service
    .labels()
    .get("ret.sh.cn/traffic")
    .ok_or_else(|| ClusterError::MissingField("traffic".into()))?;
  let id = route_id(traffic)?;
  let ports = service
    .spec
    .as_ref()
    .and_then(|s| s.ports.as_ref())
    .ok_or_else(|| ClusterError::MissingField("service ports".into()))?;
  ports
    .iter()
    .map(|port| {
      if port.protocol.as_deref().unwrap_or("TCP") != "TCP" || !(1..=65535).contains(&port.port) {
        return Err(ClusterError::GatewayConfig(
          "TLS gateway requires valid TCP ports".into(),
        ));
      }
      let host = format!("{id}-p{}.{}", port.port, config.domain);
      if host.len() > 253 {
        return Err(ClusterError::GatewayConfig(
          "gateway hostname is too long".into(),
        ));
      }
      Ok(MappedPort {
        name: port.name.clone().unwrap_or_else(|| "default".into()),
        address: format!("{host}:{}", config.port),
        scheme: Some(
          if port.app_protocol.as_deref() == Some("ret.sh.cn/traffic-http") {
            "https"
          } else {
            "tls"
          }
          .into(),
        ),
        server_name: Some(host),
      })
    })
    .collect()
}

/// One resource per Service; updating its routes also removes obsolete ports.
pub fn desired_route(service: &Service) -> Result<DynamicObject, ClusterError> {
  let config = service_config(service)?;
  let uid = service
    .uid()
    .ok_or_else(|| ClusterError::MissingField("service uid".into()))?;
  let name = service.name_any();
  let namespace = service
    .namespace()
    .ok_or_else(|| ClusterError::MissingField("service namespace".into()))?;
  let ports = service
    .spec
    .as_ref()
    .and_then(|s| s.ports.as_ref())
    .ok_or_else(|| ClusterError::MissingField("service ports".into()))?;
  let endpoints = mapped_ports(service)?;
  if endpoints.is_empty() {
    return Err(ClusterError::GatewayConfig(
      "gateway service has no ports".into(),
    ));
  }
  let routes: Vec<Value> = endpoints
    .iter()
    .zip(ports)
    .map(|(endpoint, port)| {
      json!({
        "match": format!("HostSNI(`{}`)", endpoint.server_name.as_deref().unwrap_or_default()),
        "services": [{"name": name, "port": port.port, "nativeLB": true, "tls": false}]
      })
    })
    .collect();
  let mut spec = json!({
    "entryPoints": [config.entry_point],
    "routes": routes,
    "tls": {
      "secretName": config.certificate_secret,
      "passthrough": false,
      "options": {"name": config.tls_option}
    }
  });
  if let Some(class) = config.ingress_class {
    spec["ingressClassName"] = json!(class);
  }
  Ok(serde_json::from_value(json!({
    "apiVersion": "traefik.io/v1alpha1",
    "kind": "IngressRouteTCP",
    "metadata": {
      "name": format!("r2s-gw-{uid}"),
      "namespace": namespace,
      "labels": {CONTROLLER_LABEL: CONTROLLER},
      "ownerReferences": [{"apiVersion": "v1", "kind": "Service", "name": name, "uid": uid}]
    },
    "spec": spec
  }))?)
}

fn route_needs_update(
  current: &DynamicObject, desired: &DynamicObject,
) -> Result<bool, ClusterError> {
  if current.labels().get(CONTROLLER_LABEL).map(String::as_str) != Some(CONTROLLER)
    || current.metadata.owner_references != desired.metadata.owner_references
  {
    return Err(ClusterError::GatewayConfig(
      "refusing to overwrite a route owned by another controller or Service".into(),
    ));
  }
  Ok(current.data.get("spec") != desired.data.get("spec"))
}

impl Cluster {
  /// Periodic full reconciliation handles missed events and process restarts.
  /// Requests are sequential and unchanged resources are never rewritten.
  pub async fn reconcile_tls_gateway(&self) -> Result<(), ClusterError> {
    let client = self.client.clone().ok_or(ClusterError::ClusterDisabled)?;
    let services: Api<Service> = Api::namespaced(client.clone(), CHALLENGE_NS);
    let mut managed = services
      .list(&ListParams::default().labels(&format!("{GATEWAY_LABEL}=tls")))
      .await?;
    managed.items.retain(is_gateway_service);
    if managed.items.is_empty() {
      return Ok(());
    }
    let resource = ApiResource::from_gvk(&GroupVersionKind::gvk(
      "traefik.io",
      "v1alpha1",
      "IngressRouteTCP",
    ));
    let routes: Api<DynamicObject> = Api::namespaced_with(client, CHALLENGE_NS, &resource);
    // Include unmanaged resources to detect name collisions before applying.
    let current = match routes.list(&ListParams::default()).await {
      Ok(list) => list
        .items
        .into_iter()
        .map(|r| (r.name_any(), r))
        .collect::<BTreeMap<_, _>>(),
      Err(err) => {
        for service in &managed.items {
          set_status(&services, service, "error").await?;
        }
        return Err(err.into());
      }
    };
    for service in &managed.items {
      let result = async {
        let mut desired = desired_route(service)?;
        let name = desired.name_any();
        if let Some(existing) = current.get(&name) {
          if route_needs_update(existing, &desired)? {
            // A concurrent ownership change must fail with a conflict.
            desired.metadata.resource_version = existing.resource_version();
            routes
              .patch(
                &name,
                &PatchParams::apply(CONTROLLER),
                &Patch::Apply(&desired),
              )
              .await?;
          }
        } else {
          // POST prevents a race from overwriting a newly-created foreign route.
          routes
            .create(
              &PostParams {
                field_manager: Some(CONTROLLER.into()),
                ..Default::default()
              },
              &desired,
            )
            .await?;
        }
        Ok::<_, ClusterError>(())
      }
      .await;
      let status = match result {
        Ok(()) => "configured",
        Err(error) => {
          warn!(service = %service.name_any(), ?error, "failed to reconcile TLS gateway route");
          "error"
        }
      };
      if let Err(error) = set_status(&services, service, status).await {
        warn!(service = %service.name_any(), ?error, "failed to update gateway status");
      }
    }
    Ok(())
  }
}

async fn set_status(
  api: &Api<Service>, service: &Service, status: &str,
) -> Result<(), ClusterError> {
  if service
    .annotations()
    .get(STATUS_ANNOTATION)
    .map(String::as_str)
    == Some(status)
  {
    return Ok(());
  }
  api
    .patch(
      &service.name_any(),
      &PatchParams::default(),
      &Patch::Merge(json!({
        "metadata": {
          "resourceVersion": service.resource_version(),
          "annotations": {STATUS_ANNOTATION: status}
        }
      })),
    )
    .await?;
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;

  fn service() -> Service {
    serde_json::from_value(json!({
      "metadata": {"name": "test", "namespace": CHALLENGE_NS, "uid": "test-uid",
        "labels": {"ret.sh.cn/traffic": "aB12", GATEWAY_LABEL: "tls"},
        "annotations": {CONFIG_ANNOTATION: serde_json::to_string(&json!({
          "domain": "chal.example.com", "port": 443, "entry_point": "websecure",
          "certificate_secret": "wildcard", "tls_option": "challenge-tcp"
        })).unwrap()}},
      "spec": {"type": "ClusterIP", "ports": [
        {"name": "web", "port": 8080, "protocol": "TCP", "appProtocol": "ret.sh.cn/traffic-http"},
        {"name": "pwn", "port": 9999, "protocol": "TCP", "appProtocol": "ret.sh.cn/traffic-raw"}
      ]}
    }))
    .unwrap()
  }

  #[test]
  fn hosts_preserve_case_and_fit_a_wildcard() {
    assert_ne!(route_id("aB").unwrap(), route_id("ab").unwrap());
    let ports = mapped_ports(&service()).unwrap();
    assert_eq!(ports[0].address, "61423132-p8080.chal.example.com:443");
    assert_eq!(ports[0].scheme.as_deref(), Some("https"));
    assert_eq!(ports[1].scheme.as_deref(), Some("tls"));
    assert!(route_id("bad/token").is_err());
  }

  #[test]
  fn route_terminates_tls_and_uses_service_ports_with_owner_uid() {
    let route = desired_route(&service()).unwrap();
    assert_eq!(route.data["spec"]["tls"]["passthrough"], false);
    assert_eq!(route.data["spec"]["routes"][1]["services"][0]["port"], 9999);
    assert_eq!(route.data["spec"]["routes"][1]["services"][0]["tls"], false);
    assert_eq!(
      route.metadata.owner_references.as_ref().unwrap()[0].uid,
      "test-uid"
    );
    assert!(!route_needs_update(&route, &route).unwrap());
    let mut changed = service();
    changed.spec.as_mut().unwrap().ports.as_mut().unwrap().pop();
    assert!(route_needs_update(&route, &desired_route(&changed).unwrap()).unwrap());
    let mut foreign = route.clone();
    foreign.metadata.labels = None;
    assert!(route_needs_update(&foreign, &route).is_err());
  }

  #[test]
  fn non_tcp_is_rejected() {
    let mut s = service();
    s.spec.as_mut().unwrap().ports.as_mut().unwrap()[0].protocol = Some("UDP".into());
    assert!(desired_route(&s).is_err());
  }

  #[derive(Default)]
  struct MockApi {
    services: Vec<Service>,
    routes: Vec<DynamicObject>,
    writes: Vec<(String, Value)>,
  }

  async fn mock_cluster(
    services: Vec<Service>, routes: Vec<DynamicObject>,
  ) -> (Cluster, std::sync::Arc<tokio::sync::Mutex<MockApi>>) {
    let state = std::sync::Arc::new(tokio::sync::Mutex::new(MockApi {
      services,
      routes,
      ..Default::default()
    }));
    let handler_state = state.clone();
    let app = axum::Router::new().fallback(move |req: axum::extract::Request| {
      let state = handler_state.clone();
      async move {
        let path = req.uri().path().to_owned();
        let method = req.method().clone();
        let body = axum::body::to_bytes(req.into_body(), 1024 * 1024).await.unwrap();
        let mut state = state.lock().await;
        let response = if method == axum::http::Method::GET && path.ends_with("/services") {
          json!({"apiVersion":"v1", "kind":"ServiceList", "metadata":{}, "items":state.services})
        } else if method == axum::http::Method::GET && path.ends_with("/ingressroutetcps") {
          json!({"apiVersion":"traefik.io/v1alpha1", "kind":"IngressRouteTCPList", "metadata":{}, "items":state.routes})
        } else if method == axum::http::Method::PATCH || method == axum::http::Method::POST {
          let value: Value = serde_json::from_slice(&body).unwrap();
          state.writes.push((path.clone(), value.clone()));
          if path.contains("/ingressroutetcps") {
            state.routes = vec![serde_json::from_value(value.clone()).unwrap()];
            value
          } else {
            let status = value["metadata"]["annotations"][STATUS_ANNOTATION].as_str().unwrap();
            state.services[0].metadata.annotations.as_mut().unwrap().insert(STATUS_ANNOTATION.into(), status.into());
            serde_json::to_value(&state.services[0]).unwrap()
          }
        } else {
          panic!("unexpected Kubernetes request: {method} {path}");
        };
        axum::Json(response)
      }
    });
    let client = kube::Client::new(app, CHALLENGE_NS);
    (Cluster::new(Some(client), &Default::default()), state)
  }

  #[tokio::test]
  async fn reconciliation_is_idempotent_and_does_not_rewrite_service_spec() {
    let (cluster, state) = mock_cluster(vec![service()], vec![]).await;
    cluster.reconcile_tls_gateway().await.unwrap();
    {
      let state = state.lock().await;
      assert_eq!(state.writes.len(), 2);
      assert!(state.writes[0].0.contains("/ingressroutetcps"));
      assert!(state.writes[1].1.get("spec").is_none());
      assert_eq!(
        state.writes[1].1["metadata"]["annotations"][STATUS_ANNOTATION],
        "configured"
      );
    }
    cluster.reconcile_tls_gateway().await.unwrap();
    assert_eq!(state.lock().await.writes.len(), 2);
  }

  #[tokio::test]
  async fn foreign_routes_and_unlabelled_services_are_not_modified() {
    let mut foreign = desired_route(&service()).unwrap();
    foreign.metadata.labels = None;
    let (cluster, state) = mock_cluster(vec![service()], vec![foreign]).await;
    cluster.reconcile_tls_gateway().await.unwrap();
    let writes = &state.lock().await.writes;
    assert_eq!(writes.len(), 1);
    assert!(writes[0].0.contains("/services/"));
    assert_eq!(
      writes[0].1["metadata"]["annotations"][STATUS_ANNOTATION],
      "error"
    );

    let mut legacy = service();
    legacy
      .metadata
      .labels
      .as_mut()
      .unwrap()
      .remove(GATEWAY_LABEL);
    legacy.spec.as_mut().unwrap().type_ = Some("NodePort".into());
    let (cluster, state) = mock_cluster(vec![legacy], vec![]).await;
    cluster.reconcile_tls_gateway().await.unwrap();
    assert!(state.lock().await.writes.is_empty());
  }
}
