import { afterEach, describe, expect, test } from "bun:test";
import type { Server } from "bun";
import { Bridge, type BridgeConfig } from "../src/bridge.ts";
import { handleCodexHook } from "../src/codex-hook.ts";
import {
  canonicalCodexMailbox,
  CodexSessionRegistry,
} from "../src/codex-session.ts";
import { testDb } from "./helpers.ts";

const SESSION_ID = "12345678-1234-4abc-8def-1234567890ab";
const MAILBOX = canonicalCodexMailbox(SESSION_ID);
const CONFIG: BridgeConfig = { port: 7447, maxMessageBytes: 64 * 1024, wake: {} };
const SCRIPT = new URL("../scripts/codex-hook.ts", import.meta.url).pathname;
const ROOT = new URL("..", import.meta.url).pathname;
let nextPort = 19_800;

function setup() {
  const db = testDb();
  const registry = new CodexSessionRegistry(db);
  const bridge = new Bridge(db, CONFIG, registry);
  return { db, registry, bridge };
}

describe("handleCodexHook", () => {
  for (const source of ["startup", "resume", "clear", "compact"] as const) {
    test(`registers ${source} SessionStart idempotently with canonical MCP context`, () => {
      const { db, registry, bridge } = setup();
      const payload = {
        hook_event_name: "SessionStart",
        session_id: SESSION_ID,
        cwd: "/home/dev/project",
        source,
        mailbox: "caller-controlled-identity",
      };

      const first = handleCodexHook(bridge, registry, payload);
      const second = handleCodexHook(bridge, registry, payload);

      expect(first.status).toBe(200);
      expect(first.body).toEqual(second.body);
      expect(first.body).toMatchObject({
        hookSpecificOutput: { hookEventName: "SessionStart" },
      });
      const context = (first.body as {
        hookSpecificOutput: { additionalContext: string };
      }).hookSpecificOutput.additionalContext;
      expect(context).toContain(MAILBOX);
      expect(context).toContain(`MCP \`from\` field: \`${MAILBOX}\``);
      expect(context).toContain(`MCP \`for\` field: \`${MAILBOX}\``);
      expect(
        db.query(`SELECT COUNT(*) AS count FROM codex_sessions WHERE session_id = ?1`)
          .get(SESSION_ID) as { count: number },
      ).toEqual({ count: 1 });
      expect(registry.getBySessionId(SESSION_ID)?.mailbox).toBe(MAILBOX);
    });
  }

  test("allows Stop with no unread messages and marks the session idle", () => {
    const { registry, bridge } = setup();
    registry.register({ sessionId: SESSION_ID, cwd: "/tmp/project", lifecycle: "active" });

    const result = handleCodexHook(bridge, registry, {
      hook_event_name: "Stop",
      session_id: SESSION_ID,
      cwd: "/tmp/project",
      stop_hook_active: false,
    });

    expect(result).toEqual({ status: 200, body: { continue: true } });
    expect(registry.getBySessionId(SESSION_ID)?.lifecycle).toBe("idle");
  });

  test("blocks Stop with unread messages and names the exact canonical mailbox", () => {
    const { registry, bridge } = setup();
    registry.register({ sessionId: SESSION_ID, cwd: "/tmp/project", lifecycle: "active" });
    bridge.send("claude", MAILBOX, "queued work");

    const result = handleCodexHook(bridge, registry, {
      hook_event_name: "Stop",
      session_id: SESSION_ID,
      cwd: "/tmp/project",
      stop_hook_active: false,
    });

    expect(result.status).toBe(200);
    expect(result.body).toMatchObject({ decision: "block" });
    expect((result.body as { reason: string }).reason).toContain(MAILBOX);
    expect(bridge.peekUnread(MAILBOX)).toHaveLength(1);
  });

  test("allows an active Stop hook to prevent a loop and preserves unread mail", () => {
    const { registry, bridge } = setup();
    registry.register({ sessionId: SESSION_ID, cwd: "/tmp/project", lifecycle: "active" });
    bridge.send("claude", MAILBOX, "still queued");

    const result = handleCodexHook(bridge, registry, {
      hook_event_name: "Stop",
      session_id: SESSION_ID,
      cwd: "/tmp/project",
      stop_hook_active: true,
    });

    expect(result).toEqual({ status: 200, body: { continue: true } });
    expect(bridge.peekUnread(MAILBOX)).toHaveLength(1);
    expect(registry.getBySessionId(SESSION_ID)?.lifecycle).toBe("idle");
  });

  test("returns 400 for an unknown event", () => {
    const { registry, bridge } = setup();
    const result = handleCodexHook(bridge, registry, {
      hook_event_name: "Notification",
      session_id: SESSION_ID,
    });

    expect(result.status).toBe(400);
  });

  test("returns 400 for an invalid session UUID", () => {
    const { registry, bridge } = setup();
    const result = handleCodexHook(bridge, registry, {
      hook_event_name: "SessionStart",
      session_id: "not-a-uuid",
      cwd: "/tmp/project",
      source: "startup",
    });

    expect(result.status).toBe(400);
  });
});

async function runScript(payload: unknown, url: string, timeoutMs = 2000) {
  const proc = Bun.spawn([process.execPath, "run", SCRIPT], {
    cwd: ROOT,
    env: {
      ...process.env,
      INBAND_CODEX_HOOK_URL: url,
      INBAND_CODEX_HOOK_TIMEOUT_MS: String(timeoutMs),
    },
    stdin: "pipe",
    stdout: "pipe",
    stderr: "pipe",
  });
  proc.stdin.write(JSON.stringify(payload));
  proc.stdin.end();
  const [exitCode, stdout, stderr] = await Promise.all([
    proc.exited,
    new Response(proc.stdout).text(),
    new Response(proc.stderr).text(),
  ]);
  return { exitCode, stdout: stdout.trim(), stderr };
}

function startServer(fetch: (request: Request) => Response | Promise<Response>): Server<unknown> {
  for (let attempt = 0; attempt < 100; attempt++) {
    try {
      return Bun.serve({
        hostname: "127.0.0.1",
        port: nextPort++,
        fetch,
      });
    } catch {
      nextPort++;
    }
  }
  throw new Error("could not allocate a local test port");
}

describe("codex hook command", () => {
  let server: Server<unknown> | undefined;

  afterEach(() => {
    server?.stop(true);
    server = undefined;
  });

  test("prints the bridge JSON verbatim on success", async () => {
    const expected = {
      hookSpecificOutput: {
        hookEventName: "SessionStart",
        additionalContext: "canonical context",
      },
    };
    server = startServer(async (request) => {
        expect(request.method).toBe("POST");
        expect(new URL(request.url).pathname).toBe("/codex/hook");
        expect(await request.json()).toMatchObject({ session_id: SESSION_ID });
        return Response.json(expected);
      });

    const result = await runScript(
      { hook_event_name: "SessionStart", session_id: SESSION_ID },
      `http://127.0.0.1:${server.port}/codex/hook`,
    );

    expect(result).toEqual({ exitCode: 0, stdout: JSON.stringify(expected), stderr: "" });
  });

  test("fails open on timeout", async () => {
    server = startServer(() => new Promise<Response>(() => {}));

    const result = await runScript(
      { hook_event_name: "Stop", session_id: SESSION_ID },
      `http://127.0.0.1:${server.port}/codex/hook`,
      30,
    );

    expect(result.exitCode).toBe(0);
    expect(result.stdout).toBe("{}");
    expect(result.stderr).not.toBe("");
  });

  test("fails open on connection refusal", async () => {
    server = startServer(() => new Response("unused"));
    const refusedUrl = `http://127.0.0.1:${server.port}/codex/hook`;
    server.stop(true);
    server = undefined;

    const result = await runScript(
      { hook_event_name: "Stop", session_id: SESSION_ID },
      refusedUrl,
      100,
    );

    expect(result.exitCode).toBe(0);
    expect(result.stdout).toBe("{}");
    expect(result.stderr).not.toBe("");
  });

  test("fails open without posting hook payloads to non-loopback URLs", async () => {
    const result = await runScript(
      { hook_event_name: "SessionStart", session_id: SESSION_ID },
      "http://bridge.example.test/codex/hook",
    );

    expect(result.exitCode).toBe(0);
    expect(result.stdout).toBe("{}");
    expect(result.stderr).toMatch(/non-loopback/i);
  });

  test("fails open on a bridge 5xx or invalid JSON response", async () => {
    let invalidJson = false;
    server = startServer(() =>
        invalidJson
          ? new Response("not-json", { status: 200 })
          : Response.json({ error: "local failure" }, { status: 500 }),
    );
    const url = `http://127.0.0.1:${server.port}/codex/hook`;

    const failed = await runScript({}, url);
    invalidJson = true;
    const malformed = await runScript({}, url);

    expect(failed.exitCode).toBe(0);
    expect(failed.stdout).toBe("{}");
    expect(failed.stderr).not.toBe("");
    expect(malformed.exitCode).toBe(0);
    expect(malformed.stdout).toBe("{}");
    expect(malformed.stderr).not.toBe("");
  });
});
