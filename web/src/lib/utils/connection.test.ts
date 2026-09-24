import { describe, expect, it } from "vitest";
import { connectionUrl, tlsCommands } from "./connection";

describe("challenge connection information", () => {
  it("uses the gateway scheme instead of the plaintext backend protocol", () => {
    expect(connectionUrl({ name: "web", address: "web.example.com:443", scheme: "https" }, "http")?.href).toBe(
      "https://web.example.com/"
    );
    expect(connectionUrl({ name: "web", address: "legacy.example.com:8080" }, "http")?.href).toBe(
      "http://legacy.example.com:8080/"
    );
  });
  it("does not prepend a second scheme or link executable URLs", () => {
    expect(connectionUrl({ name: "web", address: "https://web.example.com/" })?.href).toBe("https://web.example.com/");
    expect(connectionUrl({ name: "bad", address: "javascript://example.com" })).toBeNull();
  });
  it("includes SNI and preserves a nonstandard TLS port", () => {
    const commands = tlsCommands({
      name: "pwn",
      address: "10.0.0.1:8443",
      scheme: "tls",
      server_name: "pwn.example.com",
    });
    expect(commands?.pwntools).toBe('remote("10.0.0.1", 8443, ssl=True, sni="pwn.example.com")');
    expect(commands?.openssl).toContain("-connect '10.0.0.1:8443' -servername 'pwn.example.com'");
    expect(commands?.openssl).toContain("-verify_return_error");
  });
  it("handles IPv6 without putting brackets in the pwntools host", () => {
    const commands = tlsCommands({ name: "pwn", address: "[::1]:443", scheme: "tls", server_name: "pwn.example.com" });
    expect(commands?.pwntools).toContain('remote("::1", 443,');
    expect(commands?.openssl).toContain("'[::1]:443'");
  });
});
