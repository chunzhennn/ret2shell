# Ret2Shell Helm Chart

This chart installs Ret2Shell into the fixed namespace `ret2shell-platform` and uses the fixed challenge namespace `ret2shell-challenge`.

Important constraints:

- Install with `-n ret2shell-platform --create-namespace`
- The release namespace must be `ret2shell-platform`
- The challenge namespace is always `ret2shell-challenge`
- The platform is deployed as a singleton workload and must not be scaled above one replica

RBAC defaults:

- The chart creates a platform `ServiceAccount`
- The chart creates a `ClusterRoleBinding`
- By default that binding targets the built-in `cluster-admin` role because Ret2Shell currently needs broad cluster control for challenge orchestration
- You can disable chart-managed RBAC and reuse a pre-created service account with:

```bash
--set platform.serviceAccount.create=false \
--set platform.serviceAccount.name=<existing-sa> \
--set platform.rbac.create=false
```

Quick start:

```bash
helm install ret2shell ./deploy/helm/ret2shell -n ret2shell-platform --create-namespace
```

## Existing credential Secrets

Keep real credentials out of Helm values by creating Secrets before installing
the chart and referencing their names and non-sensitive data-key mappings:

```yaml
platform:
  config:
    existingEnvSecret: ret2shell-platform-env
postgresql:
  auth:
    existingSecret: ret2shell-postgresql-credentials
    secretKeys:
      username: database-user
      password: database-password
      database: database-name
valkey:
  auth:
    existingSecret: ret2shell-valkey-credentials
    secretKeys:
      password: valkey-password
nats:
  auth:
    existingSecret: ret2shell-nats-credentials
    secretKeys:
      token: nats-token
registry:
  mode: external
  external:
    existingSecret: ret2shell-registry-credentials
    secretKeys:
      username: registry-user
      password: registry-password
```

External services can use component-specific Secrets while keeping
non-sensitive connection settings in values:

```yaml
postgresql:
  mode: external
  external:
    host: postgresql.example.com
    port: 5432
    sslMode: require
    existingSecret: ret2shell-external-postgresql
    secretKeys:
      username: database-user
      password: database-password
      database: database-name
valkey:
  mode: external
  external:
    existingSecret: ret2shell-external-valkey
    secretKeys:
      url: valkey-url
nats:
  mode: external
  external:
    host: nats.example.com
    port: 4222
    tls: true
    existingSecret: ret2shell-external-nats
    secretKeys:
      token: nats-token
      user: ""
      password: ""
victoriaLogs:
  mode: external
  external:
    existingSecret: ret2shell-external-victoria-logs
    secretKeys:
      url: victoria-logs-url
```

The platform environment Secret uses configuration environment variable names
as keys. Environment values override the generated `config.toml`. Common
credential keys include:

- `R2S_CONFIG__AUTH__SIGNING_KEY`
- `R2S_CONFIG__DATABASE__DB`, `R2S_CONFIG__DATABASE__USER`, and
  `R2S_CONFIG__DATABASE__PASSWORD`
- `R2S_CONFIG__CACHE__URL`
- `R2S_CONFIG__EMAIL__PASSWORD`
- `R2S_CONFIG__QUEUE__TOKEN`, `R2S_CONFIG__QUEUE__USER`, and
  `R2S_CONFIG__QUEUE__PASSWORD`
- `R2S_CONFIG__CLUSTER__REGISTRY__USERNAME` and
  `R2S_CONFIG__CLUSTER__REGISTRY__PASSWORD`

Any other `config.toml` field can use the same
`R2S_CONFIG__<SECTION>__<FIELD>` convention. Credential Secret data keys are
configured by each component's `secretKeys` map. For bundled services, their
defaults remain `username`, `password`, and `database` for PostgreSQL,
`password` for Valkey, and `token` for NATS.

For external modes, component-specific Secrets are mapped to explicit
configuration environment variables. They take precedence over both inline
values and the same variables from `platform.config.existingEnvSecret`; the
corresponding inline credentials or URLs are not rendered into `config.toml`.

- PostgreSQL expects the selected `username`, `password`, and `database` keys.
- Valkey expects one key containing the complete connection URL.
- NATS supports either a `token` key or both `user` and `password` keys. Set an
  unused key mapping to `""`; if both authentication methods are configured,
  the token takes precedence.
- Registry expects the selected `username` and `password` keys.
- VictoriaLogs expects one key containing its complete URL.

The external registry credentials configure the Ret2Shell registry client;
they are not Kubernetes image pull credentials. `global.imagePullSecrets`
applies to chart workloads, while private challenge images still need a
Docker registry Secret in the challenge namespace referenced by the
challenge's `pull_secret`.

For bundled services, the database password, cache URL, and queue token in the
platform environment Secret must match the credentials in the service Secrets.
Passwords embedded in Valkey connection URLs must be URL-encoded.

`platform.config.existingSecret` remains available when you want to manage the
entire `config.toml` as a Secret instead.
`platform.config.secretKeys.config` selects its data key and defaults to
`config.toml`. It can be combined with `existingEnvSecret`, with environment
values taking precedence. Updating an existing Secret does not restart pods
automatically; trigger a workload rollout after rotating credentials.

Useful switches:

- `platform.exposure.type=ingress|nodePort`
- `postgresql.mode=internal|external`
- `valkey.mode=internal|external`
- `valkey.architecture=standalone|replication`
- `nats.mode=internal|external`
- `nats.replicaCount=<n>` enables bundled NATS clustering when `n > 1`
- `registry.mode=disabled|internal|external`
- `registry.replicaCount=<n>` scales the bundled registry when shared storage is available
- `victoriaLogs.mode=disabled|internal|external`
- `platform.rbac.useClusterAdmin=true|false`

Operational knobs now available on the bundled dependencies include:

- `*.podAnnotations`, `*.podLabels`, `*.priorityClassName`, `*.topologySpreadConstraints`
- `*.podDisruptionBudget.*`
- `postgresql.metrics.*`
- `valkey.metrics.*`
- `valkey.replica.replicaCount`
- `nats.metrics.*`
- `registry.metrics.*`
- `victoriaLogs.serviceMonitor.*`

Notes:

- `postgresql` stays single-instance in this v1 chart, but now exposes richer pod and metrics configuration.
- `valkey.architecture=replication` runs a single StatefulSet with pod `0` as the primary and the remaining pods as replicas.
- `valkey.persistence.existingClaim` must stay empty when `valkey.architecture=replication`.
- `nats.persistence.existingClaim` must stay empty when `nats.replicaCount > 1`.
- `registry.replicaCount > 1` requires RWX/shared storage or an equivalent shared backend claim.

Example renders:

```bash
helm template ret2shell ./deploy/helm/ret2shell -n ret2shell-platform -f ./deploy/helm/ret2shell/examples/values-ingress-internal.yaml
helm template ret2shell ./deploy/helm/ret2shell -n ret2shell-platform -f ./deploy/helm/ret2shell/examples/values-nodeport-external.yaml
```

The chart defaults are templateable for validation, but you should replace at least these values before production use:

- `platform.image.*`
- `platform.config.auth.signingKey`
- `platform.config.server.externalDomain`
- all default passwords and tokens
- `registry.externalAccess.host` when internal registry is enabled

Current v1 registry behavior:

- Internal registry support is modeled as an anonymous registry plus a node-reachable external address
- If you need custom registry auth behavior, prefer switching `registry.mode=external` and supplying a preconfigured external registry
