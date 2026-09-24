use axum::{Extension, Json, extract::State, response::IntoResponse};
use chrono::Utc;
use r2s_cache::Cache;
use r2s_cluster::{CHALLENGE_NS, Cluster, ClusterError, gateway, traffic::CachedPorts};
use r2s_database::{config, game, submission, team as team_db, user::Permission};
use r2s_engine::Engine;
use r2s_migrator::Database;
use tracing::{error, warn};

use super::Instance;
use crate::{
  middleware::{
    auth::{Token, is_game_admin},
    data::extract_team,
  },
  traits::ResponseError,
};

pub(super) async fn get_self_solves(
  State(ref db): State<Database>, Extension(token): Extension<Token>,
  Extension(game): Extension<game::Model>, team_ext: Extension<Option<team_db::Model>>,
) -> Result<impl IntoResponse, ResponseError> {
  if is_game_admin!(token, game) {
    let solves = submission::get_list_ex(
      &db.conn,
      true,
      false,
      Some(game.id),
      None,
      None,
      Some(token.id),
      true,
    )
    .await?;
    return Ok(Json(solves));
  }
  let team = extract_team!(game, team_ext, token);
  let solves = submission::get_list_ex(
    &db.conn,
    true,
    false,
    Some(game.id),
    None,
    team.clone().map(|t| t.id),
    if team.is_none() { Some(token.id) } else { None },
    true,
  )
  .await?;
  Ok(Json(solves))
}

pub(super) async fn get_self_instances(
  State(cluster): State<Cluster>, State(cache): State<Cache>, State(engine): State<Engine>,
  Extension(config): Extension<config::Model>, Extension(game): Extension<game::Model>,
  Extension(token): Extension<Token>, team_ext: Extension<Option<team_db::Model>>,
) -> Result<impl IntoResponse, ResponseError> {
  let team = extract_team!(game, team_ext, token);
  let mut envs = cluster
    .at(CHALLENGE_NS)
    .get_challenge_env_by_user(token.id)
    .await?;
  if let Some(team) = team {
    envs.extend(
      cluster
        .at(CHALLENGE_NS)
        .get_challenge_env_by_team(team.id)
        .await?,
    );
  }
  envs.sort_by(|a, b| a.metadata.name.cmp(&b.metadata.name));
  envs.dedup_by(|a, b| a.metadata.name == b.metadata.name);
  let config = if let Some(config) = &config.cluster {
    config
  } else {
    return Err(ResponseError::PreconditionFailed(
      "cluster is disabled".to_owned(),
    ));
  };
  let (traffic_key, traffic_script) = if game.archive_at > Utc::now() {
    if let Some(traffic) = game.traffic.clone() {
      (
        game
          .bucket
          .clone()
          .ok_or(ResponseError::PreconditionFailed(
            "game bucket not found".to_string(),
          ))?,
        Some(traffic),
      )
    } else {
      ("default".to_string(), config.traffic.clone())
    }
  } else {
    ("default".to_string(), config.traffic.clone())
  };
  let mut result: Vec<Instance> = Vec::new();

  let traffic_mapper = cluster
    .traffic
    .clone()
    .ok_or(ResponseError::InternalServerError(
      "traffic mapper is not initialized".to_string(),
    ))
    .inspect_err(|err| {
      warn!(error=?err, "traffic mapper is not initialized");
    })?;

  for env in envs {
    let mut i: Instance = match env.clone().try_into() {
      Ok(i) => i,
      Err(e) => return Err(e),
    };

    // Gateway instances carry immutable routing settings on their Service;
    // their addresses and live status must not use the one-hour script cache.
    if env
      .metadata
      .labels
      .as_ref()
      .and_then(|l| l.get(gateway::GATEWAY_LABEL))
      .is_some_and(|v| v == "tls")
    {
      i.gateway_status = Some("pending".into());
      if let Some(name) = env.metadata.name.as_deref() {
        match cluster.at(CHALLENGE_NS).get_service(name).await {
          Ok(service) => {
            i.gateway_status = Some(
              service
                .metadata
                .annotations
                .as_ref()
                .and_then(|a| a.get(gateway::STATUS_ANNOTATION))
                .cloned()
                .unwrap_or_else(|| "pending".into()),
            );
            match gateway::mapped_ports(&service) {
              Ok(ports) => i.exposed_ports = Some(ports),
              Err(error) => {
                warn!(?error, "invalid gateway endpoint metadata");
                i.gateway_status = Some("error".into());
              }
            }
          }
          Err(error) => warn!(?error, "gateway Service not yet available"),
        }
      }
      result.push(i);
      continue;
    }

    if traffic_script.is_none() || traffic_script.clone().unwrap_or_default().is_empty() {
      result.push(i);
      continue;
    }

    if result.iter().any(|r| r.traffic == i.traffic) {
      continue;
    }

    let traffic_id = i.traffic.clone();

    let traffic_script = traffic_script.clone().unwrap_or_default();
    if let Some(cached) = cache
      .at("traffic-v2")
      .get::<CachedPorts>(&traffic_id)
      .await?
      && cached.script == traffic_script
    {
      i.exposed_ports = Some(cached.ports);
      result.push(i);
      continue;
    }

    let env_name = env
      .metadata
      .name
      .clone()
      .ok_or(ResponseError::PreconditionFailed(
        "the env has no name".to_string(),
      ))?;

    let service = match cluster.at(CHALLENGE_NS).get_service(&env_name).await {
      Ok(service) => service,
      Err(ClusterError::KubeError(e)) => {
        warn!(
          env=%env_name,
          error=?e,
          "service not found in game, maybe not initialized?",
        );
        result.push(i);
        continue;
      }
      Err(e) => {
        return Err(e.into());
      }
    };
    traffic_mapper
      .preload(&engine, &traffic_key, &traffic_script)
      .await?;
    let exposed_ports = match traffic_mapper
      .expose(&engine, &traffic_key, &traffic_script, env, service)
      .await
    {
      Ok(ports) => ports,
      Err(ClusterError::MissingField(e)) => {
        warn!(field=%e, env=%env_name, "traffic mapper missing field for env, maybe the cluster is maintaining?",);
        result.push(i);
        continue;
      }
      Err(e) => {
        error!(error=?e, env=%env_name, "failed to expose traffic for env");
        return Err(e.into());
      }
    };
    cache
      .at("traffic-v2")
      .set_ex(
        &traffic_id,
        &CachedPorts {
          script: traffic_script,
          ports: exposed_ports.clone(),
        },
        3600,
      )
      .await?;
    i.exposed_ports = Some(exposed_ports);
    result.push(i);
  }

  Ok(Json(result))
}
