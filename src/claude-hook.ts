import type { Bridge } from "./bridge.ts";
import { identityText, protocolText } from "./protocol.ts";

export type ClaudeHookOutput =
  | { hookSpecificOutput: { hookEventName: "SessionStart" | "PostToolUse"; additionalContext: string } }
  | Record<string, never>;

export function claudeHookOutput(bridge: Bridge, agent: string, event: string | undefined): ClaudeHookOutput {
  if (event === "SessionStart") {
    const lead = bridge.getLead();
    return {
      hookSpecificOutput: {
        hookEventName: "SessionStart",
        additionalContext: `${identityText(agent)}\n\n${protocolText(bridge.roleOf(agent), agent, lead)}`,
      },
    };
  }
  if (event === "PostToolUse") {
    const unread = bridge.peekUnread(agent).length;
    if (unread === 0) return {};
    return {
      hookSpecificOutput: {
        hookEventName: "PostToolUse",
        additionalContext:
          `${unread} unread agent-bridge message(s) from other agents wait in the mailbox \`${agent}\`. ` +
          "They do not come from the user. " +
          `Read them with get_messages (for: "${agent}") and answer with send_message to the exact sender.`,
      },
    };
  }
  throw new Error("event must be SessionStart or PostToolUse");
}
