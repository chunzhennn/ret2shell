use axum::{
  Router,
  extract::{Path, Query, Request, State, WebSocketUpgrade},
  middleware::{Next, from_fn},
  response::IntoResponse,
  routing::get,
};
use r2s_cluster::{CHALLENGE_NS, Cluster};
use serde::Deserialize;
use tracing::debug;

use crate::traits::ResponseError;

pub fn router() -> Router<Cluster> {
  Router::new()
    .route(
      "/{token}",
      get(link_challenge_env).options(ping_challenge_env),
    )
    .route_layer(from_fn(disable_wsrx))
}

// Retain the implementation, but block every WSRX entry point before upgrade
// or Kubernetes access. Hiding the frontend alone would leave a gateway bypass.
async fn disable_wsrx(_request: Request, _next: Next) -> ResponseError {
  ResponseError::Forbidden("WSRX is disabled".into())
}

#[derive(Deserialize)]
struct LinkQuery {
  port: u16,
}

async fn link_challenge_env(
  State(cluster): State<Cluster>, Path(token): Path<String>,
  Query(LinkQuery { port }): Query<LinkQuery>, ws: WebSocketUpgrade,
) -> Result<impl IntoResponse, ResponseError> {
  Ok(ws.on_upgrade(move |socket| async move {
    let result = cluster
      .at(CHALLENGE_NS)
      .wsrx_link(&token, port, socket)
      .await;
    if let Err(e) = result {
      debug!(error = ?e, "failed to link challenge env");
    }
  }))
}

async fn ping_challenge_env() -> impl IntoResponse {
  "pong"
}

#[cfg(test)]
mod tests {
  use axum::{body::Body, http::StatusCode};
  use tower::ServiceExt;

  use super::*;

  #[tokio::test]
  async fn wsrx_upgrade_and_probe_are_blocked_without_cluster_access() {
    let app = router().with_state(Cluster::new(None, &Default::default()));
    for method in ["GET", "OPTIONS"] {
      let request = Request::builder()
        .method(method)
        .uri("/existing-traffic-token?port=1337")
        .header("connection", "upgrade")
        .header("upgrade", "websocket")
        .header("sec-websocket-version", "13")
        .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
        .body(Body::empty())
        .unwrap();
      let response = app.clone().oneshot(request).await.unwrap();
      assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }
  }
}
