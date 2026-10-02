import { describe, expect, test } from "bun:test";
import {
  assertAgentAuthorized,
  authenticateAuthorizationHeader,
  authenticateSignedRequest,
  buildAuthRuntime,
  directoryAgentPatterns,
  signInbandRequest,
  visibleAgentPatterns,
} from "../src/auth.ts";

const CLAUDE_TOKEN = "c".repeat(64);
const CODEX_TOKEN = "d".repeat(64);
const ADMIN_TOKEN = "a".repeat(64);

function runtime() {
  return buildAuthRuntime({
    required: true,
    clients: {
      claude: { token: CLAUDE_TOKEN, agents: ["claude-*"] },
      codex: { token: CODEX_TOKEN, agents: ["codex-*"] },
      admin: { token: ADMIN_TOKEN, agents: ["*"], admin: true },
    },
  });
}

describe("inband auth", () => {
  test("requires a known bearer token and binds it to agent-name scopes", () => {
    const auth = runtime();
    const claude = authenticateAuthorizationHeader(auth, `Bearer ${CLAUDE_TOKEN}`);

    expect(claude.clientId).toBe("claude");
    expect(() => assertAgentAuthorized(claude, "claude-api-a1b2", "from")).not.toThrow();
    expect(() => assertAgentAuthorized(claude, "codex-019f6767-789c-73b2-bc5c-ac8575f29efd", "from")).toThrow(
      /not authorized/i,
    );
    expect(() => authenticateAuthorizationHeader(auth, undefined)).toThrow(/missing authorization/i);
    expect(() => authenticateAuthorizationHeader(auth, "Bearer nope")).toThrow(/invalid bearer/i);
  });

  test("lets admin tokens operate on every mailbox", () => {
    const admin = authenticateAuthorizationHeader(runtime(), `Bearer ${ADMIN_TOKEN}`);

    expect(admin.extra.admin).toBe(true);
    expect(() => assertAgentAuthorized(admin, "claude-api-a1b2", "for")).not.toThrow();
    expect(() => assertAgentAuthorized(admin, "codex-019f6767-789c-73b2-bc5c-ac8575f29efd", "for")).not.toThrow();
  });

  test("verifies signed requests with freshness and nonce replay protection", () => {
    const auth = runtime();
    const request = {
      method: "POST",
      url: "http://127.0.0.1:7447/presence",
      body: { agent: "claude-api-a1b2", online: true },
    };
    const headers = signInbandRequest({
      clientId: "claude",
      token: CLAUDE_TOKEN,
      ...request,
      nowMs: Date.parse("2026-09-05T12:00:00.000Z"),
      nonce: "nonce-1",
    });

    const first = authenticateSignedRequest(auth, { ...request, headers }, Date.parse("2026-09-05T12:00:30.000Z"));
    expect(first.clientId).toBe("claude");
    expect(() =>
      authenticateSignedRequest(auth, { ...request, headers }, Date.parse("2026-09-05T12:00:31.000Z")),
    ).toThrow(/replay/i);
  });

  test("accepts signed requests from hooks that predate the rename", () => {
    const request = {
      method: "GET",
      url: "http://127.0.0.1:7447/claude/hook?agent=claude-api-a1b2&event=SessionStart",
    };
    const signed = signInbandRequest({ clientId: "claude", token: CLAUDE_TOKEN, ...request, nonce: "legacy-1" });
    const legacy = Object.fromEntries(
      Object.entries(signed).map(([name, value]) => [name.replace("x-inband-", "x-agent-bridge-"), value]),
    );

    expect(authenticateSignedRequest(runtime(), { ...request, headers: legacy }).clientId).toBe("claude");
  });

  test("rejects weak token config and invalid agent patterns", () => {
    expect(() =>
      buildAuthRuntime({
        required: true,
        clients: {
          bad: { token: "change-me-secret-change-me-secret", agents: ["bad-*"] },
        },
      }),
    ).toThrow(/placeholder/i);
    expect(() =>
      buildAuthRuntime({
        required: true,
        clients: {
          bad: { token: "b".repeat(64), agents: ["../bad-*"] },
        },
      }),
    ).toThrow(/agent pattern/i);
  });

  test("lists directory agents without granting mailbox or history access", () => {
    const auth = buildAuthRuntime({
      required: true,
      clients: {
        claude: { token: CLAUDE_TOKEN, agents: ["claude-*"] },
        codex: { token: CODEX_TOKEN, agents: ["codex-*"], directory: ["*"] },
      },
    });
    const claude = authenticateAuthorizationHeader(auth, `Bearer ${CLAUDE_TOKEN}`);
    const codex = authenticateAuthorizationHeader(auth, `Bearer ${CODEX_TOKEN}`);

    expect(directoryAgentPatterns(claude)).toEqual(["claude-*"]);
    expect(directoryAgentPatterns(codex)).toBeUndefined();
    expect(visibleAgentPatterns(codex)).toEqual(["codex-*"]);
    expect(() => assertAgentAuthorized(codex, "claude-api-a1b2", "from")).toThrow(/not authorized/i);
    expect(() =>
      buildAuthRuntime({
        required: true,
        clients: { codex: { token: CODEX_TOKEN, agents: ["codex-*"], directory: [] } },
      }),
    ).toThrow(/directory pattern/i);
  });
});
