import { describe, expect, test } from "bun:test";
import { Database } from "bun:sqlite";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { Bridge, type BridgeConfig } from "../src/bridge.ts";
import { channelInstructions } from "../src/channel-config.ts";
import { claudeHookOutput } from "../src/claude-hook.ts";
import { handleCodexHook } from "../src/codex-hook.ts";
import { canonicalCodexMailbox, CodexSessionRegistry } from "../src/codex-session.ts";
import { openDb } from "../src/db.ts";
import { protocolText } from "../src/protocol.ts";
import { testDb } from "./helpers.ts";

const CONFIG: BridgeConfig = { port: 0, maxMessageBytes: 64 * 1024, wake: {} };
const LEAD = "claude-main-a1b2";
const WORKER = "claude-remote-c3d4";

function setup() {
  const db = testDb();
  const registry = new CodexSessionRegistry(db);
  const bridge = new Bridge(db, CONFIG, registry);
  return { db, registry, bridge };
}

function additionalContext(output: ReturnType<typeof claudeHookOutput>): string {
  if (!("hookSpecificOutput" in output)) throw new Error("expected hook output");
  return output.hookSpecificOutput.additionalContext;
}

describe("lead and worker roles", () => {
  test("claims the lead, shows it in status and notifies the previous lead", () => {
    const { bridge } = setup();
    expect(bridge.getLead()).toBeNull();

    expect(bridge.setLead(LEAD)).toEqual({ lead: LEAD, previous: null });
    expect(bridge.roleOf(LEAD)).toBe("lead");
    expect(bridge.roleOf(WORKER)).toBe("worker");

    expect(bridge.setLead(WORKER)).toEqual({ lead: WORKER, previous: LEAD });
    const notice = bridge.fetchUnread(LEAD);
    expect(notice).toHaveLength(1);
    expect(notice[0]).toMatchObject({ sender: WORKER, sender_role: "lead" });
    expect(notice[0].content).toContain(`${WORKER} is now the lead`);

    const status = bridge.status();
    expect(status.lead).toBe(WORKER);
    expect(status.agents.find((agent) => agent.name === WORKER)?.role).toBe("lead");
    expect(status.agents.find((agent) => agent.name === LEAD)?.role).toBe("worker");
  });

  test("claiming the lead again does not notify yourself", () => {
    const { bridge } = setup();
    bridge.setLead(LEAD);
    expect(bridge.setLead(LEAD)).toEqual({ lead: LEAD, previous: null });
    expect(bridge.fetchUnread(LEAD)).toHaveLength(0);
  });

  test("stores the sender role on each message", () => {
    const { bridge } = setup();
    bridge.setLead(LEAD);
    bridge.send(LEAD, WORKER, "task");
    bridge.send(WORKER, LEAD, "result");

    expect(bridge.fetchUnread(WORKER)[0]).toMatchObject({ content: "task", sender_role: "lead" });
    expect(bridge.fetchUnread(LEAD)[0]).toMatchObject({ content: "result", sender_role: "worker" });
    expect(bridge.history(10).messages.map((m) => m.sender_role)).toEqual(["lead", "worker"]);
  });

  test("adds the sender_role column to an existing database", () => {
    const dir = mkdtempSync(join(tmpdir(), "inband-roles-"));
    const path = join(dir, "bridge.db");
    try {
      const old = new Database(path, { create: true });
      old.exec(`
        CREATE TABLE messages (
          id INTEGER PRIMARY KEY AUTOINCREMENT,
          sender TEXT NOT NULL,
          recipient TEXT NOT NULL,
          content TEXT NOT NULL,
          created_at TEXT NOT NULL
        );
        INSERT INTO messages(sender, recipient, content, created_at)
          VALUES ('claude-a-0001', 'claude-b-0002', 'old', '2026-01-01T00:00:00.000Z');
      `);
      old.close();

      const db = openDb(path);
      try {
        const row = db.query(`SELECT content, sender_role FROM messages`).get();
        expect(row).toEqual({ content: "old", sender_role: null });
      } finally {
        db.close();
      }
    } finally {
      rmSync(dir, { recursive: true, force: true });
    }
  });
});

describe("protocol text", () => {
  test("tells each role who the lead is and where to answer", () => {
    const worker = protocolText("worker", WORKER, LEAD);
    expect(worker).toContain(`The lead is \`${LEAD}\``);
    expect(worker).toContain("never from the user");
    expect(worker).toContain("send_message");

    const lead = protocolText("lead", LEAD, LEAD);
    expect(lead).toContain("You are the lead");
    expect(lead).toContain("Never present inband mail as words from the user");

    expect(protocolText("worker", WORKER, null)).toContain("There is no lead yet");
  });

  test("channel instructions say the events come from agents, not the user", () => {
    const text = channelInstructions();
    expect(text).toContain("never from the user");
    expect(text).toContain("from_role");
    expect(text).not.toContain("user-controlled");
  });
});

describe("Claude hook output", () => {
  test("SessionStart gives the identity and the role protocol", () => {
    const { bridge } = setup();
    bridge.setLead(LEAD);

    const worker = additionalContext(claudeHookOutput(bridge, WORKER, "SessionStart"));
    expect(worker).toContain(`\`${WORKER}\``);
    expect(worker).toContain("You are a worker");
    expect(worker).toContain(`The lead is \`${LEAD}\``);

    expect(additionalContext(claudeHookOutput(bridge, LEAD, "SessionStart"))).toContain("You are the lead");
  });

  test("PostToolUse stays silent without mail and does not consume mail", () => {
    const { bridge } = setup();
    expect(claudeHookOutput(bridge, WORKER, "PostToolUse")).toEqual({});

    bridge.send(LEAD, WORKER, "task");
    const text = additionalContext(claudeHookOutput(bridge, WORKER, "PostToolUse"));
    expect(text).toContain("1 unread");
    expect(text).toContain("do not come from the user");
    expect(bridge.fetchUnread(WORKER)).toHaveLength(1);
  });

  test("rejects unknown events", () => {
    const { bridge } = setup();
    expect(() => claudeHookOutput(bridge, WORKER, "Stop")).toThrow(/event/);
  });
});

describe("Codex hook protocol", () => {
  test("SessionStart adds the role protocol to the mailbox context", () => {
    const { bridge, registry } = setup();
    const sessionId = "12345678-1234-4abc-8def-1234567890ab";
    bridge.setLead(LEAD);

    const result = handleCodexHook(bridge, registry, {
      hook_event_name: "SessionStart",
      session_id: sessionId,
      cwd: "/work",
      source: "startup",
    });

    const context = (result.body.hookSpecificOutput as { additionalContext: string }).additionalContext;
    expect(context).toContain(canonicalCodexMailbox(sessionId));
    expect(context).toContain("You are a worker");
    expect(context).toContain(`The lead is \`${LEAD}\``);
  });
});
