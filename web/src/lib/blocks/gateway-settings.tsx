import { useUpdateExposureMutation } from "@api/cluster";
import type { ClusterConfig, ExposureMode } from "@models/config";
import { t } from "@storage/theme";
import Button from "@widgets/button";
import Input from "@widgets/input";
import Select from "@widgets/select";
import { createEffect, createSignal, createUniqueId, Show } from "solid-js";

export default function GatewaySettings(props: { config?: ClusterConfig; onSaved: () => void }) {
  const id = createUniqueId();
  const [mode, setMode] = createSignal<ExposureMode>("auto");
  const [domain, setDomain] = createSignal("");
  const [port, setPort] = createSignal(443);
  const [entryPoint, setEntryPoint] = createSignal("websecure");
  const [secret, setSecret] = createSignal("ret2shell-challenge-tls");
  const [tlsOption, setTlsOption] = createSignal("ret2shell-challenge-tcp");
  const [ingressClass, setIngressClass] = createSignal("");
  const [directAddress, setDirectAddress] = createSignal("");
  createEffect(() => {
    setMode(props.config?.exposure_mode || "auto");
    const gateway = props.config?.tls_gateway;
    setDomain(gateway?.domain || "");
    setPort(gateway?.port || 443);
    setEntryPoint(gateway?.entry_point || "websecure");
    setSecret(gateway?.certificate_secret || "ret2shell-challenge-tls");
    setTlsOption(gateway?.tls_option || "ret2shell-challenge-tcp");
    setIngressClass(gateway?.ingress_class || "");
    setDirectAddress(props.config?.direct_access?.address || "");
  });
  const update = useUpdateExposureMutation({ onSuccess: props.onSaved });
  return (
    <section class="space-y-3 py-3">
      <h2 class="font-bold">{t("traffic.gateway.title")}</h2>
      <p class="text-sm opacity-70">{t("traffic.gateway.help")}</p>
      <Select
        size="sm"
        value={[mode()]}
        items={[
          { value: "auto", label: t("traffic.gateway.auto") },
          { value: "node_port", label: "NodePort" },
          { value: "cluster_ip", label: "ClusterIP" },
          { value: "tls_gateway", label: "TLS / SNI" },
        ]}
        onValueChange={(e) => setMode((e.value[0] as ExposureMode) || "auto")}
      />
      <Show when={mode() === "tls_gateway"}>
        <div class="grid grid-cols-1 md:grid-cols-2 gap-3">
          <label for={`${id}-domain`}>
            {t("traffic.gateway.domain")}
            <Input
              id={`${id}-domain`}
              value={domain()}
              placeholder="chal.example.com"
              onInput={(e) => setDomain(e.target.value)}
            />
          </label>
          <label for={`${id}-port`}>
            {t("traffic.gateway.port")}
            <Input
              id={`${id}-port`}
              type="number"
              min="1"
              max="65535"
              value={port()}
              onInput={(e) => setPort(Number(e.target.value))}
            />
          </label>
          <label for={`${id}-entryPoint`}>
            {t("traffic.gateway.entryPoint")}
            <Input id={`${id}-entryPoint`} value={entryPoint()} onInput={(e) => setEntryPoint(e.target.value)} />
          </label>
          <label for={`${id}-secret`}>
            {t("traffic.gateway.secret")}
            <Input id={`${id}-secret`} value={secret()} onInput={(e) => setSecret(e.target.value)} />
          </label>
          <label for={`${id}-tlsOption`}>
            {t("traffic.gateway.tlsOption")}
            <Input id={`${id}-tlsOption`} value={tlsOption()} onInput={(e) => setTlsOption(e.target.value)} />
          </label>
          <label for={`${id}-ingressClass`}>
            {t("traffic.gateway.ingressClass")}
            <Input id={`${id}-ingressClass`} value={ingressClass()} onInput={(e) => setIngressClass(e.target.value)} />
          </label>
          <label for={`${id}-directAddress`}>
            {t("traffic.gateway.directAddress")}
            <Input
              id={`${id}-directAddress`}
              value={directAddress()}
              placeholder="{node}.nodes.example.com"
              onInput={(e) => setDirectAddress(e.target.value)}
            />
          </label>
        </div>
        <p class="text-sm opacity-70">{t("traffic.gateway.directAddressHelp")}</p>
      </Show>
      <Button
        size="sm"
        level="primary"
        loading={update.isPending}
        onClick={() =>
          update.mutate({
            exposure_mode: mode(),
            tls_gateway:
              mode() === "tls_gateway"
                ? {
                    domain: domain().trim(),
                    port: port(),
                    entry_point: entryPoint().trim(),
                    certificate_secret: secret().trim(),
                    tls_option: tlsOption().trim(),
                    ingress_class: ingressClass().trim() || null,
                  }
                : null,
            direct_access:
              mode() === "tls_gateway" && directAddress().trim() ? { address: directAddress().trim() } : null,
          })
        }
      >
        {t("general.actions.save.title")}
      </Button>
    </section>
  );
}
