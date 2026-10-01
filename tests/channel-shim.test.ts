import { expect, test } from "bun:test";
import { mkdtempSync, mkdirSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { Bridge } from "../src/bridge.ts";
import { testDb } from "./helpers.ts";

function startShim(env: Record<string, string>) {
  const child = Bun.spawn([process.execPath, new URL("../src/channel-shim.ts", import.meta.url).pathname], {
    stdin: "pipe", stdout: "pipe", stderr: "pipe",
    env: { ...process.env, AGENT_BRIDGE_MAILBOX: "", AGENT_BRIDGE_TOKENS_FILE: "/nonexistent/channel-test-tokens", ...env },
  });
  const reader = child.stdout.getReader();
  let pending = "";
  async function readMessage() {
    while (!pending.includes("\n")) {
      let timer: ReturnType<typeof setTimeout>;
      const chunk = await Promise.race([
        reader.read(),
        new Promise<never>((_, reject) => { timer = setTimeout(() => reject(new Error("no channel response")), 2000); }),
      ]).finally(() => clearTimeout(timer));
      if (chunk.done) throw new Error("shim closed stdout");
      pending += new TextDecoder().decode(chunk.value);
    }
    const end = pending.indexOf("\n");
    const message = JSON.parse(pending.slice(0, end));
    pending = pending.slice(end + 1);
    return message;
  }
  return {
    readMessage,
    async initialize() {
      child.stdin.write(JSON.stringify({ jsonrpc: "2.0", id: 1, method: "initialize", params: {
        protocolVersion: "2024-11-05", capabilities: {}, clientInfo: { name: "test", version: "1" },
      } }) + "\n");
      expect((await readMessage()).id).toBe(1);
      child.stdin.write(JSON.stringify({ jsonrpc: "2.0", method: "notifications/initialized" }) + "\n");
    },
    async close() { child.kill(); await child.exited; reader.releaseLock(); },
  };
}

test("native channel isolates startup mail and follows a retained CLI across clear", async () => {
  const dir = mkdtempSync(join(tmpdir(), "bridge-native-channel-"));
  mkdirSync(join(dir, "sessions"));
  const sessionFile = join(dir, "sessions", `${process.pid}.json`);
  const sessionA = "a1b20000-0000-4000-8000-000000000001";
  const sessionB = "b2c30000-0000-4000-8000-000000000002";
  const saveSession = (sessionId: string) => writeFileSync(sessionFile, JSON.stringify({
    pid: process.pid, sessionId, cwd: "/projects/shared", startedAt: 1234, procStart: "1234",
  }));
  saveSession(sessionA);
  const db = testDb();
  const bridge = new Bridge(db, { port: 0, maxMessageBytes: 65536, wake: {} });
  bridge.send("opencode", "claude-shared-dead", "old unrelated session");
  bridge.send("opencode", "claude-shared-a1b2", "queued for A");
  const urls: URL[] = [];
  const server = Bun.serve({ hostname: "127.0.0.1", port: 0, async fetch(request) {
    const url = new URL(request.url);
    urls.push(url);
    const after = Number(url.searchParams.get("after_id"));
    const onClose = (cleanup: () => void) => request.signal.addEventListener("abort", cleanup, { once: true });
    const messages = url.searchParams.has("mailbox")
      ? await bridge.subscribeMailbox(url.searchParams.get("mailbox")!, 1, onClose, after)
      : await bridge.subscribeFamily(url.searchParams.get("prefix")!, 1, onClose, after);
    return Response.json({ messages });
  } });
  const env = { CLAUDE_CODE_SESSION_ID: sessionA, CLAUDE_PROJECT_DIR: "/different-project-root", CLAUDE_CONFIG_DIR: dir,
    AGENT_BRIDGE_URL: `http://127.0.0.1:${server.port}` };
  let shim = startShim(env);
  try {
    await shim.initialize();
    expect((await shim.readMessage()).params).toMatchObject({ content: "queued for A", meta: { to: "claude-shared-a1b2" } });
    expect(urls[0].searchParams.get("mailbox")).toBe("claude-shared-a1b2");
    expect(urls[0].searchParams.has("prefix")).toBe(false);

    saveSession(sessionB);
    bridge.send("opencode", "claude-shared-a1b2", "A after clear must not leak");
    bridge.send("opencode", "claude-shared-b2c3", "queued for B during clear");
    expect((await shim.readMessage()).params).toMatchObject({ content: "queued for B during clear", meta: { to: "claude-shared-b2c3" } });

    await shim.close();
    shim = startShim({ ...env, CLAUDE_CODE_SESSION_ID: sessionB });
    await shim.initialize();
    expect((await shim.readMessage()).params.content).toBe("queued for B during clear");
    expect(bridge.fetchUnread("claude-shared-dead")).toHaveLength(1);
    expect(bridge.fetchUnread("claude-shared-a1b2")).toHaveLength(2);
  } finally {
    await shim.close();
    server.stop(true);
    db.close();
    rmSync(dir, { recursive: true, force: true });
  }
});

test("channel drops rows for a different mailbox even if the server returns them", async () => {
  const server = Bun.serve({ hostname: "127.0.0.1", port: 0, fetch(request) {
    if (new URL(request.url).searchParams.get("after_id") !== "0") return new Promise<Response>(() => {});
    return Response.json({ messages: [
      { id: 1, sender: "opencode", recipient: "claude-other", content: "not yours", created_at: "now" },
      { id: 2, sender: "opencode", recipient: "all", content: "missing concrete recipient", created_at: "now" },
      { id: 3, sender: "opencode", recipient: "claude-owned", content: "yours", created_at: "now" },
    ] });
  } });
  const shim = startShim({ AGENT_BRIDGE_MAILBOX: "claude-owned", AGENT_BRIDGE_URL: `http://127.0.0.1:${server.port}` });
  try {
    await shim.initialize();
    expect((await shim.readMessage()).params.content).toBe("yours");
  } finally { await shim.close(); server.stop(true); }
});

test("a temporarily unavailable session record does not skip unread notifications", async () => {
  const dir = mkdtempSync(join(tmpdir(), "bridge-channel-recovery-"));
  mkdirSync(join(dir, "sessions"));
  const file = join(dir, "sessions", `${process.pid}.json`);
  const sessionId = "c3d40000-0000-4000-8000-000000000003";
  const record = JSON.stringify({ pid: process.pid, sessionId, cwd: "/projects/shared", startedAt: 1234, procStart: "1234" });
  writeFileSync(file, record);
  let first = true;
  let restore: ReturnType<typeof setTimeout> | undefined;
  const server = Bun.serve({ hostname: "127.0.0.1", port: 0, fetch(request) {
    if (new URL(request.url).searchParams.get("after_id") !== "0") return new Promise<Response>(() => {});
    if (first) {
      first = false;
      writeFileSync(file, "");
      restore = setTimeout(() => writeFileSync(file, record), 50);
    }
    return Response.json({ messages: [{ id: 1, sender: "opencode", recipient: "claude-shared-c3d4", content: "still unread", created_at: "now" }] });
  } });
  const shim = startShim({ CLAUDE_CODE_SESSION_ID: sessionId, CLAUDE_CONFIG_DIR: dir, AGENT_BRIDGE_URL: `http://127.0.0.1:${server.port}` });
  try {
    await shim.initialize();
    expect((await shim.readMessage()).params.content).toBe("still unread");
  } finally {
    await shim.close(); clearTimeout(restore); server.stop(true); rmSync(dir, { recursive: true, force: true });
  }
});

test("queued channel notifications wait for MCP initialization", async () => {
  let polls = 0;
  const server = Bun.serve({
    hostname: "127.0.0.1",
    port: 0,
    fetch(request) {
      polls++;
      const cursor = new URL(request.url).searchParams.get("after_id");
      if (cursor !== "0") return new Promise<Response>(() => {});
      return Response.json({ messages: [{
        id: 1,
        sender: "opencode",
        recipient: "claude-test-a1b2",
        content: "queued mail",
        created_at: "2026-09-10T00:00:00.000Z",
      }] });
    },
  });
  const child = Bun.spawn([process.execPath, new URL("../src/channel-shim.ts", import.meta.url).pathname], {
    stdin: "pipe",
    stdout: "pipe",
    stderr: "pipe",
    env: {
      ...process.env,
      AGENT_BRIDGE_URL: `http://127.0.0.1:${server.port}`,
      AGENT_BRIDGE_MAILBOX: "claude-test-a1b2",
      AGENT_BRIDGE_TOKENS_FILE: "/nonexistent/channel-test-tokens",
    },
  });
  const reader = child.stdout.getReader();
  let pending = "";
  async function readMessage() {
    while (!pending.includes("\n")) {
      const chunk = await reader.read();
      if (chunk.done) throw new Error("shim closed stdout");
      pending += new TextDecoder().decode(chunk.value);
    }
    const end = pending.indexOf("\n");
    const message = JSON.parse(pending.slice(0, end));
    pending = pending.slice(end + 1);
    return message;
  }
  try {
    child.stdin.write(JSON.stringify({ jsonrpc: "2.0", id: 1, method: "initialize", params: {
      protocolVersion: "2024-11-05", capabilities: {}, clientInfo: { name: "test", version: "1" },
    } }) + "\n");
    const initialized = await readMessage();
    expect(initialized.id).toBe(1);
    await Bun.sleep(40);
    expect(polls).toBe(0);
    child.stdin.write(JSON.stringify({ jsonrpc: "2.0", method: "notifications/initialized" }) + "\n");
    const notification = await readMessage();
    expect(notification.method).toBe("notifications/claude/channel");
    expect(notification.params.meta.to).toBe("claude-test-a1b2");
    expect(notification.params.content).toBe("queued mail");
  } finally {
    child.kill();
    await child.exited;
    reader.releaseLock();
    server.stop(true);
  }
});
