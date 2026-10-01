import { describe, expect, test } from "bun:test";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  buildSubscribeUrl,
  channelInstructions,
  createMailboxResolver,
  readChannelConfig,
} from "../src/channel-config.ts";

describe("channel config", () => {
  test("refuses to subscribe without an exact session identity", () => {
    expect(() => readChannelConfig({})).toThrow(/identity/i);
  });

  test("reads bridge URL and mailbox from env", () => {
    const config = readChannelConfig({
      AGENT_BRIDGE_URL: "http://127.0.0.1:8744/",
      AGENT_BRIDGE_MAILBOX: "claude-desktop-a1b2",
    });

    expect(config).toEqual({
      bridgeUrl: "http://127.0.0.1:8744",
      mailbox: "claude-desktop-a1b2",
    });
  });

  test("refuses non-loopback bridge URLs", () => {
    expect(() =>
      readChannelConfig({
        AGENT_BRIDGE_URL: "http://bridge.example.test:7447/",
      }),
    ).toThrow(/non-loopback/i);
  });

  test("strips trailing slashes from the bridge URL", () => {
    expect(readChannelConfig({ AGENT_BRIDGE_URL: "http://127.0.0.1:7447///", AGENT_BRIDGE_MAILBOX: "claude-test" }).bridgeUrl).toBe(
      "http://127.0.0.1:7447",
    );
  });

  test("builds an exact mailbox subscribe URL when a mailbox is configured", () => {
    const url = buildSubscribeUrl({
      bridgeUrl: "http://127.0.0.1:8744/base/",
      mailbox: "claude-desktop-a1b2",
    }, 290);

    expect(url).toBe("http://127.0.0.1:8744/base/subscribe?mailbox=claude-desktop-a1b2&timeout=290");
  });

  test("normalizes explicit identities and rejects generic or invalid mailboxes", () => {
    expect(readChannelConfig({ AGENT_BRIDGE_MAILBOX: " CLAUDE-Desktop-A1B2 " }).mailbox).toBe("claude-desktop-a1b2");
    for (const mailbox of ["claude", "all", "claude/test", "x".repeat(65)]) {
      expect(() => readChannelConfig({ AGENT_BRIDGE_MAILBOX: mailbox })).toThrow(/mailbox/i);
    }
  });

  test("includes the last notification ID when reconnecting", () => {
    const url = new URL(buildSubscribeUrl({ bridgeUrl: "http://127.0.0.1:7447", mailbox: "claude-test" }, 290, 42));
    expect(url.searchParams.get("after_id")).toBe("42");
  });

  test("instructions name the owned mailbox and get_messages target", () => {
    const instructions = channelInstructions("claude-desktop-a1b2");

    expect(instructions).toContain("YOUR agent-bridge mailbox is claude-desktop-a1b2");
    expect(instructions).toContain('get_messages tool with for="claude-desktop-a1b2"');
  });

  test("native subscriptions use the current hook identity after a session change", () => {
    const instructions = channelInstructions();

    expect(instructions).not.toContain("YOUR agent-bridge mailbox is claude.");
    expect(instructions).not.toContain('for="claude"');
    expect(instructions).toContain("SessionStart");
  });
});

test("native identity waits for its owner and pauses on an invalid or reused process record", () => {
  const dir = mkdtempSync(join(tmpdir(), "bridge-channel-owner-"));
  const registryFile = join(dir, "session.json");
  const sessionId = "a1b20000-0000-4000-8000-000000000001";
  const resolve = createMailboxResolver({ bridgeUrl: "http://127.0.0.1:7447", nativeSession: { registryFile, sessionId, pid: 123 } });
  const row = { pid: 123, sessionId, cwd: "/projects/shared", startedAt: 1234, procStart: "1234" };
  try {
    expect(resolve()).toBeUndefined();
    writeFileSync(registryFile, JSON.stringify({ ...row, sessionId: "b2c30000-0000-4000-8000-000000000002" }));
    expect(resolve()).toBeUndefined();
    writeFileSync(registryFile, JSON.stringify(row));
    expect(resolve()).toBe("claude-shared-a1b2");
    writeFileSync(registryFile, "{");
    expect(resolve()).toBeUndefined();
    writeFileSync(registryFile, JSON.stringify({ ...row, pid: 124 }));
    expect(resolve()).toBeUndefined();
    writeFileSync(registryFile, JSON.stringify({ ...row, procStart: "5678" }));
    expect(resolve()).toBeUndefined();
    writeFileSync(registryFile, JSON.stringify({ ...row, startedAt: 5678 }));
    expect(resolve()).toBeUndefined();
  } finally { rmSync(dir, { recursive: true, force: true }); }
});

test("native identity matches hook slugging for Unicode project names", () => {
  const dir = mkdtempSync(join(tmpdir(), "bridge-channel-unicode-"));
  const registryFile = join(dir, "session.json");
  const sessionId = "a1b20000-0000-4000-8000-000000000001";
  const resolve = createMailboxResolver({ bridgeUrl: "http://127.0.0.1:7447", nativeSession: { registryFile, sessionId, pid: 123 } });
  try {
    writeFileSync(registryFile, JSON.stringify({ pid: 123, sessionId, cwd: "/projects/foo😀bar", startedAt: 1234, procStart: "1234" }));
    expect(resolve()).toBe("claude-foo-bar-a1b2");
  } finally { rmSync(dir, { recursive: true, force: true }); }
});
