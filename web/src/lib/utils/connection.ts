import type { MappedPort } from "@models/instance";

/** External transport comes from the endpoint, not the container protocol. */
export function connectionUrl(endpoint: MappedPort, fallbackScheme = "tcp"): URL | null {
  try {
    const url = new URL(
      endpoint.address.includes("://") ? endpoint.address : `${endpoint.scheme || fallbackScheme}://${endpoint.address}`
    );
    if (!["http:", "https:", "tcp:", "tls:", "udp:", "stcp:"].includes(url.protocol)) return null;
    if (url.username || url.password || !url.hostname) return null;
    return url;
  } catch {
    return null;
  }
}

function shellQuote(value: string) {
  return `'${value.replaceAll("'", "'\\''")}'`;
}

export function tlsCommands(endpoint: MappedPort): { pwntools: string; openssl: string } | null {
  const url = connectionUrl(endpoint);
  if (url?.protocol !== "tls:") return null;
  const host = url.hostname.replace(/^\[|\]$/g, "");
  const port = Number(url.port || 443);
  const serverName = endpoint.server_name || host;
  const destination = `${url.hostname}:${port}`;
  return {
    pwntools: `remote(${JSON.stringify(host)}, ${port}, ssl=True, sni=${JSON.stringify(serverName)})`,
    openssl: `openssl s_client -connect ${shellQuote(destination)} -servername ${shellQuote(serverName)} -verify_hostname ${shellQuote(serverName)} -verify_return_error -quiet`,
  };
}
