import { connectionUrl, tlsCommands } from "@lib/utils/connection";
import type { MappedPort } from "@models/instance";
import { t } from "@storage/theme";
import ClipboardBtn from "@widgets/clipboard-btn";
import { Show } from "solid-js";

export default function Connection(props: { endpoint: MappedPort; fallbackScheme: string }) {
  const url = () => connectionUrl(props.endpoint, props.fallbackScheme);
  const web = () => url()?.protocol === "https:" || url()?.protocol === "http:";
  const commands = () => tlsCommands(props.endpoint);
  const address = () => (web() ? url()!.href : props.endpoint.address);
  return (
    <div class="flex flex-wrap items-center gap-2">
      <ClipboardBtn size="sm" value={address()} label={address()} />
      <Show when={web()}>
        <a class="btn btn-sm" href={url()!.href} target="_blank" rel="noopener noreferrer">
          {t("traffic.gateway.open")}
        </a>
      </Show>
      <Show when={commands()}>
        {(value) => (
          <>
            <ClipboardBtn size="sm" value={value().pwntools} label="pwntools (TLS)" />
            <ClipboardBtn size="sm" value={value().openssl} label="OpenSSL" />
          </>
        )}
      </Show>
    </div>
  );
}
