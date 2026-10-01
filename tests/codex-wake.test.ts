import { describe, expect, test } from "bun:test";
import {
  Bridge,
  type BridgeConfig,
  type BridgeRuntime,
  type WakeDispatchInput,
} from "../src/bridge.ts";
import { CodexSessionRegistry } from "../src/codex-session.ts";
import type { CodexWakeResult } from "../src/wake.ts";
import { wakeOpencode, type WakeTarget } from "../src/wake.ts";
import { testDb } from "./helpers.ts";

const A = "019f6767-789c-73b2-bc5c-ac8575f29efd";
const B = "019f6767-789c-73b2-bc5c-ac8575f29efe";
const MAILBOX_A = `codex-${A}`;
const MAILBOX_B = `codex-${B}`;

const CONFIG: BridgeConfig = {
  port: 7447,
  maxMessageBytes: 65536,
  wake: {
    opencode: {
      type: "opencode",
      baseUrl: "http://127.0.0.1:14096",
      prompt: "opencode prompt",
      debounceSeconds: 30,
      maxWakesPerHour: 20,
    },
    codex: {
      type: "codex",
      command: "/usr/local/bin/codex",
      prompt: "mail for {mailbox}",
      debounceSeconds: 30,
      maxWakesPerHour: 20,
      retryDelaysSeconds: [5, 15, 30, 60],
    },
  },
};

class FakeScheduler {
  nowMs = Date.parse("2026-07-16T10:00:00.000Z");
  scheduled: number[] = [];
  private timers: Array<{ id: number; delay: number; callback: () => void; cancelled: boolean }> = [];
  private nextId = 1;

  setTimeout = (callback: () => void, delay: number) => {
    const timer = { id: this.nextId++, delay, callback, cancelled: false };
    this.timers.push(timer);
    this.scheduled.push(delay);
    return timer as unknown as ReturnType<typeof setTimeout>;
  };

  clearTimeout = (handle: ReturnType<typeof setTimeout>) => {
    const id = (handle as unknown as { id: number }).id;
    const timer = this.timers.find((candidate) => candidate.id === id);
    if (timer) timer.cancelled = true;
  };

  async runNext(): Promise<void> {
    const timer = this.timers.shift();
    if (!timer) throw new Error("no timer scheduled");
    this.nowMs += timer.delay;
    if (!timer.cancelled) timer.callback();
    await flush();
  }

  pending(): number {
    return this.timers.filter((timer) => !timer.cancelled).length;
  }
}

async function flush(): Promise<void> {
  await Bun.sleep(0);
  for (let index = 0; index < 5; index++) await Promise.resolve();
}

function setup(results: CodexWakeResult[] = [{ disposition: "started", detail: "ok" }]) {
  const db = testDb();
  const registry = new CodexSessionRegistry(db);
  const scheduler = new FakeScheduler();
  const calls: Array<{ target: WakeTarget; input: WakeDispatchInput }> = [];
  const queue = [...results];
  const runtime: BridgeRuntime = {
    now: () => new Date(scheduler.nowMs),
    setTimeout: scheduler.setTimeout,
    clearTimeout: scheduler.clearTimeout,
    dispatchWake: async (target, input) => {
      calls.push({ target, input });
      return queue.shift() ?? results.at(-1)!;
    },
  };
  const bridge = new Bridge(db, structuredClone(CONFIG), registry, runtime);
  return { db, registry, scheduler, calls, bridge };
}

describe("Codex wake orchestration", () => {
  test("refuses non-loopback OpenCode wake URLs", async () => {
    await expect(
      wakeOpencode({
        type: "opencode",
        baseUrl: "http://example.test:14096",
        prompt: "wake",
        debounceSeconds: 30,
        maxWakesPerHour: 20,
      }),
    ).resolves.toEqual({
      disposition: "failed",
      detail: expect.stringMatching(/non-loopback/i),
    });
  });

  test("uses family Codex config with full identity while preserving OpenCode target", async () => {
    const { db, registry, bridge, calls } = setup([
      { disposition: "started", detail: "codex" },
      { disposition: "started", detail: "opencode" },
    ]);
    registry.register({ sessionId: A, cwd: "/tmp/a", lifecycle: "idle" });

    bridge.send("claude", MAILBOX_A, "wake A");
    bridge.send("claude", "opencode", "wake opencode");
    await flush();

    expect(calls[0]).toMatchObject({
      target: { type: "codex", command: "/usr/local/bin/codex" },
      input: { recipient: MAILBOX_A, mailbox: MAILBOX_A, sessionId: A },
    });
    expect(calls[1]).toMatchObject({ target: { type: "opencode" }, input: { recipient: "opencode" } });
    expect(
      db.query(`SELECT ok, detail FROM wakes WHERE recipient = ?1`).get(MAILBOX_A),
    ).toEqual({ ok: 1, detail: "started: codex" });
  });

  test("deduplicates active retries and uses 5, 15, 30, 60 second delays", async () => {
    const active = { disposition: "deferred-active-turn", detail: "busy" } as const;
    const { registry, bridge, calls, scheduler } = setup([active, active, active, active, active]);
    registry.register({ sessionId: A, cwd: "/tmp/a", lifecycle: "active" });

    bridge.send("claude", MAILBOX_A, "one");
    bridge.send("opencode", MAILBOX_A, "two");
    await flush();
    expect(calls).toHaveLength(1);
    for (let index = 0; index < 4; index++) await scheduler.runNext();

    expect(calls).toHaveLength(5);
    expect(scheduler.scheduled).toEqual([5000, 15000, 30000, 60000]);
    expect(scheduler.pending()).toBe(0);
  });

  test("a queued wake counts as success and stops retries", async () => {
    const { db, registry, bridge, scheduler, calls } = setup([{ disposition: "queued", detail: "accepted" }]);
    registry.register({ sessionId: A, cwd: "/tmp/a", lifecycle: "idle" });
    bridge.send("claude", MAILBOX_A, "queued work");
    await flush();
    expect(calls).toHaveLength(1);
    expect(scheduler.pending()).toBe(0);
    expect(db.query("SELECT ok, detail FROM wakes ORDER BY id DESC LIMIT 1").get()).toEqual({
      ok: 1, detail: "queued: accepted",
    });
    expect(bridge.peekUnread(MAILBOX_A)).toHaveLength(1);
  });

  test("successful retry stops remaining attempts", async () => {
    const active = { disposition: "deferred-active-turn", detail: "busy" } as const;
    const started = { disposition: "started", detail: "started" } as const;
    const state = setup([active, started]);
    state.registry.register({ sessionId: A, cwd: "/tmp/a", lifecycle: "active" });
    state.bridge.send("claude", MAILBOX_A, "one");
    await flush();
    await state.scheduler.runNext();
    expect(state.calls).toHaveLength(2);
    expect(state.scheduler.pending()).toBe(0);
  });

  test("consuming the last unread delivery cancels a pending retry", async () => {
    const active = { disposition: "deferred-active-turn", detail: "busy" } as const;
    const state = setup([active]);
    state.registry.register({ sessionId: A, cwd: "/tmp/a", lifecycle: "active" });
    state.bridge.send("claude", MAILBOX_A, "one");
    await flush();
    expect(state.scheduler.pending()).toBe(1);
    expect(state.bridge.fetchUnread(MAILBOX_A)).toHaveLength(1);
    expect(state.scheduler.pending()).toBe(0);
  });

  test("failures leave deliveries unread and only successful starts debounce", async () => {
    const failed = { disposition: "failed", detail: "proxy failed" } as const;
    const { db, registry, bridge, calls } = setup([failed]);
    registry.register({ sessionId: A, cwd: "/tmp/a", lifecycle: "idle" });
    db.query(`INSERT INTO wakes(recipient, created_at, ok, detail) VALUES (?1, ?2, 0, 'old fail')`)
      .run(MAILBOX_A, "2026-07-16T09:59:59.000Z");
    bridge.send("claude", MAILBOX_A, "work");
    await flush();
    expect(calls).toHaveLength(1);
    expect(bridge.peekUnread(MAILBOX_A)).toHaveLength(1);

    bridge.fetchUnread(MAILBOX_A);
    db.query(`INSERT INTO wakes(recipient, created_at, ok, detail) VALUES (?1, ?2, 1, 'started')`)
      .run(MAILBOX_A, "2026-07-16T09:59:59.500Z");
    const sent = bridge.send("claude", MAILBOX_A, "debounced");
    expect(sent.notify[MAILBOX_A]).toContain("wake-debounced");
  });

  test("hourly cap is per canonical mailbox", async () => {
    const { db, registry, bridge, calls } = setup();
    registry.register({ sessionId: A, cwd: "/tmp/a", lifecycle: "idle" });
    registry.register({ sessionId: B, cwd: "/tmp/b", lifecycle: "idle" });
    for (let index = 0; index < 20; index++) {
      db.query(`INSERT INTO wakes(recipient, created_at, ok, detail) VALUES (?1, ?2, 0, 'x')`)
        .run(MAILBOX_A, `2026-07-16T09:59:${String(index).padStart(2, "0")}.000Z`);
    }
    const capped = bridge.send("claude", MAILBOX_A, "capped");
    bridge.send("claude", MAILBOX_B, "allowed");
    await flush();
    expect(capped.notify[MAILBOX_A]).toContain("wake-suppressed");
    expect(calls).toHaveLength(1);
    expect(calls[0].input.recipient).toBe(MAILBOX_B);
  });

  test("startup reconciliation schedules only registered mailboxes with unread rows", async () => {
    const { db, registry, bridge, calls } = setup([
      { disposition: "started", detail: "a" },
      { disposition: "started", detail: "b" },
    ]);
    registry.register({ sessionId: A, cwd: "/tmp/a", lifecycle: "idle" });
    registry.register({ sessionId: B, cwd: "/tmp/b", lifecycle: "idle" });
    const now = "2026-07-16T09:59:00.000Z";
    const inserted = db
      .query(`INSERT INTO messages(sender, recipient, content, created_at) VALUES ('claude', ?1, 'queued', ?2)`)
      .run(MAILBOX_A, now);
    db.query(`INSERT INTO deliveries(message_id, recipient, read_at) VALUES (?1, ?2, NULL)`)
      .run(inserted.lastInsertRowid, MAILBOX_A);
    bridge.reconcileCodexWakes();
    await flush();
    expect(calls.map((call) => call.input.recipient)).toEqual([MAILBOX_A]);
  });
});
