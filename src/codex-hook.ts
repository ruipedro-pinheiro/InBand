import type { Bridge } from "./bridge.ts";
import { protocolText } from "./protocol.ts";
import {
  canonicalCodexMailbox,
  type CodexSessionRegistry,
} from "./codex-session.ts";

const SESSION_START_SOURCES = new Set(["startup", "resume", "clear", "compact"]);

export interface CodexHookResult {
  status: number;
  body: Record<string, unknown>;
}

function badRequest(message: string): CodexHookResult {
  return { status: 400, body: { error: message } };
}

export function handleCodexHook(
  bridge: Bridge,
  registry: CodexSessionRegistry,
  payload: unknown,
): CodexHookResult {
  if (!payload || typeof payload !== "object" || Array.isArray(payload)) {
    return badRequest("expected a hook payload object");
  }

  const input = payload as Record<string, unknown>;
  const event = input.hook_event_name;
  const sessionId = input.session_id;
  if (event !== "SessionStart" && event !== "Stop") {
    return badRequest("hook_event_name must be SessionStart or Stop");
  }
  if (typeof sessionId !== "string") {
    return badRequest("session_id must be a string");
  }

  let mailbox: string;
  try {
    mailbox = canonicalCodexMailbox(sessionId);
  } catch (error) {
    return badRequest(error instanceof Error ? error.message : String(error));
  }

  if (event === "SessionStart") {
    if (typeof input.cwd !== "string") return badRequest("cwd must be a string");
    if (typeof input.source !== "string" || !SESSION_START_SOURCES.has(input.source)) {
      return badRequest("unsupported SessionStart source");
    }
    registry.register({ sessionId, cwd: input.cwd, lifecycle: "active" });
    return {
      status: 200,
      body: {
        hookSpecificOutput: {
          hookEventName: "SessionStart",
          additionalContext:
            `Your canonical inband mailbox is \`${mailbox}\`. ` +
            `Use it for the MCP \`from\` field: \`${mailbox}\`, and for the MCP \`for\` field: \`${mailbox}\`.\n\n` +
            protocolText(bridge.roleOf(mailbox), mailbox, bridge.getLead()),
        },
      },
    };
  }

  if (typeof input.stop_hook_active !== "boolean") {
    return badRequest("stop_hook_active must be a boolean");
  }
  const session = registry.getBySessionId(sessionId);
  if (!session) return badRequest(`unregistered Codex session ${sessionId}`);

  registry.touchMailbox(mailbox, "idle");
  const unread = bridge.peekUnread(mailbox);
  if (unread.length === 0 || input.stop_hook_active) {
    return { status: 200, body: { continue: true } };
  }
  return {
    status: 200,
    body: {
      decision: "block",
      reason:
        `Unread inband mail from another agent, not from the user, is queued for ${mailbox}. ` +
        `Call get_messages with for=\"${mailbox}\" and handle it before stopping.`,
    },
  };
}
