# TLS gateway integration

Ret2Shell supports standard HTTPS and raw TCP-over-TLS challenge access through
Traefik. Players use a browser, pwntools or OpenSSL; no Ret2Shell-specific client
is required. Challenge payloads go directly through Traefik to the challenge
Service, without traversing the platform API server or Kubernetes port-forward.
Protocols that cannot be demultiplexed on the shared TLS port (SSH, UDP) can opt
out per service and use a direct node port instead; see
[Direct exposed ports](#direct-exposed-ports).

## Platform configuration

The setting under **Administration → Traffic → Challenge access** (DevOps
permission) is independent of the platform website's own Service type.
`PATCH /api/cluster/exposure` accepts:

```json
{
  "exposure_mode": "tls_gateway",
  "tls_gateway": {
    "domain": "chal.example.com",
    "port": 443,
    "entry_point": "websecure",
    "certificate_secret": "ret2shell-challenge-tls",
    "tls_option": "ret2shell-challenge-tcp",
    "ingress_class": null
  },
  "direct_access": {
    "address": "{node}.nodes.example.com"
  }
}
```

The equivalent TOML fields are `cluster.exposure_mode`, `cluster.tls_gateway`
and `cluster.direct_access`. Helm exposes them as
`platform.config.cluster.exposureMode`, `tlsGateway` and `directAccess`; see
[the overlay](../deploy/helm/ret2shell/examples/values-tls-gateway.yaml).
The database's optional runtime mode overrides the file setting. An explicit
`auto` in the administration page also overrides a Helm `tls_gateway` setting.

| Mode | New challenge Service |
| --- | --- |
| `auto` (default) | Original behavior: NodePort when a traffic script exists, otherwise ClusterIP |
| `node_port` | NodePort; configure a traffic script to display direct addresses |
| `cluster_ip` | ClusterIP; suitable for a separately managed ingress/traffic script |
| `tls_gateway` | ClusterIP, labelled `ret.sh.cn/gateway=tls`; built-in SNI routing. NodePort instead when any service opts for direct exposure |

Gateway mode rejects UDP/STCP ports that would go through the gateway before
creating a Pod; direct-exposed ports may use any protocol. The container
remains configured with its actual plaintext protocol (`tcp` with `http` or
`raw`). Each gateway Service stores its configuration snapshot in
`ret.sh.cn/tls-gateway`. Changes apply only to new instances, so a mode/domain
change does not silently migrate active challenges. No existing Service is
automatically converted from NodePort to ClusterIP.

## Direct exposed ports

The shared gateway port demultiplexes instances by TLS SNI, so protocols
without a TLS ClientHello — SSH is the common case — cannot ride the gateway.
A challenge service can set its exposure to `direct` to opt out: its port gets
no `IngressRouteTCP` route and is addressed through a raw node port, exactly
like the legacy NodePort behavior.

Direct exposure only takes effect in `tls_gateway` mode; in other modes the
field is inert and the platform-wide behavior applies. A Service with any
direct port is created as NodePort (gateway-labelled ports on the same Service
keep their Traefik routes — `nativeLB` reaches pod endpoints regardless of the
Service type — but they also get node ports allocated). Services store the
address template snapshot in `ret.sh.cn/direct-access` and are labelled
`ret.sh.cn/direct=true`; endpoints render as `<rendered address>:<node port>`
with the container protocol as scheme (`tcp`, `udp`, or `http`).

The address template comes from `cluster.direct_access`:

- `{node}.nodes.example.com` renders the instance's node name into the host,
  for clusters where each node has its own reachable name;
- a plain host such as `ssh.example.com` addresses every node behind one load
  balancer or port-forwarding layer.

Prerequisites and caveats:

- Players must be able to reach the challenge nodes on the node port range
  (30000-32767 by default), the same requirement as legacy NodePort mode. In
  the Traefik-gateway-only deployment described above, cluster nodes are often
  not player-reachable and must be exposed first.
- Direct access is only as isolated as the allocated node port; there is no
  TLS termination or per-instance hostname. Challenges should carry their own
  authentication (an SSH server with credentials, for example).
- UDP/STCP service ports are allowed on direct-exposed services, which also
  makes UDP challenges usable in gateway mode.

## Route lifecycle

The platform runs an independent, sequential reconciliation task every ten
seconds using its existing **challenge cluster** kubeconfig. It lists only
gateway-labelled Services in `ret2shell-challenge`. No gateway-labelled Services
means no Traefik API calls or gateway writes. This task does not install CRDs,
Traefik, certificates, listeners, or NetworkPolicies.

For every Service it creates one `IngressRouteTCP`, named `r2s-gw-<Service UID>`,
containing one route per exposed TCP port. Backends reference the Service and
its actual port, with `nativeLB: true` and backend `tls: false`. Frontend TLS
terminates in Traefik (`passthrough: false`).

Hostnames are `<hex(original traffic token)>-p<service port>.<domain>`. Encoding
the original bytes preserves case-sensitive tokens while producing a lowercase
single DNS label covered by `*.<domain>`. Routes have a Service owner reference
and are garbage-collected when the Service is deleted. An extension of the
instance lifetime does not change routing. Service UIDs also prevent a new
instance reusing the name of an old instance's route.

The controller reads current routes before writing, skips unchanged resources,
uses create-on-absence and optimistic concurrency on updates, and refuses to
overwrite resources with foreign ownership. Kubernetes errors are logged and
retried on the next pass; the instance cleanup worker runs separately.
`pending`, `configured`, and `error` in the Service's
`ret.sh.cn/gateway-status` annotation are exposed as `Instance.gateway_status`.
**Configured means the desired route was written, not that a certificate,
Traefik configuration, DNS, or the application passed an end-to-end probe.**

Gateway instances use built-in endpoint generation, taking precedence over
global/game traffic scripts. Their addresses and status do not use the legacy
one-hour cache. Script-based instances keep working: the cache also compares
script content, and compiled scripts use content-dependent keys, so a script
edit does not keep returning an old cached address or old compiled program.

## Challenge cluster prerequisites

1. Install a pinned Traefik v3 release and its `traefik.io` CRDs in the
   **challenge cluster**, with a TCP entry point matching `entry_point`.
   [Traefik values](../deploy/tls-gateway/traefik-values.yaml) are an optional
   standalone deployment example, not part of the platform chart. The example
   has been render-validated with chart 41.6.0 (Traefik v3.7.13). It
   binds host port 443: check both host listeners and existing Kubernetes
   hostPort/Service allocations before applying it. For an initial isolated
   trial, change hostPort and the platform gateway port to 8443.
2. Point `*.chal.example.com` to the gateway's reachable address. Place a trusted
   wildcard certificate and private key in the `ret2shell-challenge-tls` TLS
   Secret in namespace `ret2shell-challenge`; manage renewal independently.
   Do not commit the private key. The platform never reads the Secret contents.
3. Apply the [TLSOption](../deploy/tls-gateway/tls-option.yaml) in the challenge
   namespace. It restricts ALPN to HTTP/1.1 because raw TCP routing does not
   translate HTTP/2. Pwn clients without ALPN are accepted. Do not enable HTTP/3
   for this TCP-only gateway.
4. Give the challenge kubeconfig identity the permissions in
   [controller-rbac.yaml](../deploy/tls-gateway/controller-rbac.yaml), adjusting
   the subject first. The platform Helm chart's extra Role covers only the
   same-cluster deployment; installing it in the platform cluster cannot grant
   access in a separate challenge cluster. Traefik needs its own provider RBAC.
5. Ensure Traefik can reach the challenge ClusterIPs over the CNI network. A
   gateway on another physical machine in the LAN does not automatically have
   a route to ClusterIPs. Do not enable the gateway on the platform cluster's
   ingress and assume it can reach the remote challenge network.
6. Build and deploy the platform image from this branch, then enable gateway
   mode. Neither Helm nor the configuration UI automatically installs the
   challenge-side prerequisites.

For custom admission policies, keep `ingress_class` consistent with the
Traefik provider configuration. The supplied standalone example leaves it null
and disables the conventional Ingress provider and default IngressClass.

## Existing traffic script API

Script inputs additionally expose `service.route_id` and `port.port` (the actual
Service port). `port.node_port` is retained for compatibility. Existing
`#{ service_name: "host:port" }` results are unchanged. Values may now also be:

```rune
#{
    pwn: #{
        address: "pwn.example.com:443",
        scheme: "tls",
        server_name: "pwn.example.com",
    },
    web: #{ address: "web.example.com:443", scheme: "https" },
}
```

Supported schemes are `http`, `https`, `tcp`, `tls`, and `udp`. The frontend
uses the external scheme instead of inferring it from the container protocol.
HTTPS endpoints have an open button; raw TLS endpoints provide pwntools and
OpenSSL commands. WSRX is disabled globally in this branch: its HTTP endpoints
return 403 before WebSocket upgrade or Kubernetes port-forward, and the frontend
does not connect to the local daemon or show WSRX controls. The handlers, client
implementation and dependencies remain in place. Direct HTTP/NodePort access
continues working for legacy instances; WSRX-only access is no longer available.

## Verification and rollback

Use a disposable Web and Pwn challenge, not an existing participant instance:

- Confirm both Services are ClusterIP and have no `nodePort` allocation.
- Connect to both names on the same gateway port; verify routing isolation.
- Check HTTPS with certificate verification and Pwn with TLS/SNI, including
  an idle interactive connection and binary payloads.
- Stop one instance; verify its Service and route disappear and the old name
  cannot reach a newly created instance.
- Restart the platform or delete a generated route; verify reconciliation
  restores the desired routes without rewriting unrelated resources.
- Check that existing NodePort instances continue working throughout.

To stop creating gateway instances, change the runtime mode to `auto` or
`node_port` and restore the desired direct-address traffic script. Existing
gateway instances retain their configuration and routes until stopped. Drain
them before removing Traefik or its CRDs. Reverting only the Helm value does not
override a mode saved in the administration page.

## Local deployment assessment (2026-09-24)

Read-only checks of `merak@10.92.35.10` found a single Ready MicroK8s node running
Kubernetes v1.35.6, three existing NodePort challenge Services, and no Traefik
CRDs or IngressClass. Host TCP listeners did not include 443 at the time of the
check; this alone does not rule out Kubernetes hostPort/NAT reservations.

The platform deployment under `~/workspace/merak-manifest/services/ret2shell`
uses a separate challenge kubeconfig and has `platform.rbac.create: false`.
Its `platform.exposure.type: clusterIP` controls only the platform website.
It also vendors the Helm chart, so release the new platform image and update
that vendored chart before using the new Helm values. The image currently
pinned there predates this feature. These live manifests were not changed.

Reference: [Traefik IngressRouteTCP](https://doc.traefik.io/traefik/reference/routing-configuration/kubernetes/crd/tcp/ingressroutetcp/).
