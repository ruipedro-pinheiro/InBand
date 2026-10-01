#!/usr/bin/env bun

import { Server } from "@modelcontextprotocol/sdk/server/index.js";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import { signInbandRequest } from "./auth.ts";
import { buildSubscribeUrl, channelInstructions, createMailboxResolver, readChannelConfig } from "./channel-config.ts";
import { clientTokenFromEnv, loadTokenEnvFile } from "./token-env.ts";

const POLL_SECONDS = 290; // daemon caps /subscribe at 300
loadTokenEnvFile();
const config = readChannelConfig();
const resolveMailbox = createMailboxResolver(config);

const mcp = new Server(
  { name: "inband-channel", version: "1.0.0" },
  {
    capabilities: { experimental: { "claude/channel": {} } },
    instructions: channelInstructions(config.mailbox),
  },
);

const initialized = new Promise<void>((resolve) => {
  mcp.oninitialized = resolve;
});
await mcp.connect(new StdioServerTransport());
await initialized;

interface Row {
  id: number;
  sender: string;
  recipient: string;
  content: string;
  created_at: string;
  sender_role?: string | null;
}

let afterId = 0;
let lastMailbox: string | undefined;
while (true) {
  const mailbox = resolveMailbox();
  if (!mailbox) {
    await Bun.sleep(250);
    continue;
  }
  if (mailbox !== lastMailbox) {
    afterId = 0;
    lastMailbox = mailbox;
  }
  const changed = new AbortController();
  const watcher = setInterval(() => {
    if (resolveMailbox() !== mailbox) changed.abort();
  }, 250);
  try {
    const url = buildSubscribeUrl({ bridgeUrl: config.bridgeUrl, mailbox }, POLL_SECONDS, afterId);
    const headers: Record<string, string> = {};
    const clientId = Bun.env.INBAND_CLIENT_ID ?? "claude";
    const token = clientTokenFromEnv(clientId);
    if (token) {
      Object.assign(
        headers,
        signInbandRequest({
          clientId,
          token,
          method: "GET",
          url,
        }),
      );
    }
    const res = await fetch(url, {
      headers,
      signal: AbortSignal.any([changed.signal, AbortSignal.timeout((POLL_SECONDS + 15) * 1000)]),
    });
    if (!res.ok) throw new Error(`GET /subscribe -> ${res.status}`);
    const { messages } = (await res.json()) as { messages: Row[] };
    let completedBatch = true;
    for (const m of messages) {
      if (resolveMailbox() !== mailbox) {
        completedBatch = false;
        break;
      }
      if (m.recipient !== mailbox) continue;
      await mcp.notification({
        method: "notifications/claude/channel",
        params: {
          content: m.content,
          meta: {
            from: m.sender,
            from_role: m.sender_role ?? "worker",
            to: m.recipient,
            reply_via: "send_message",
            sent_at: m.created_at,
          },
        },
      });
    }
    // Advance only after the whole batch reaches stdio. A failed batch can replay.
    if (completedBatch) for (const m of messages) afterId = Math.max(afterId, m.id);
  } catch {
    if (!changed.signal.aborted) await Bun.sleep(5000);
  } finally {
    clearInterval(watcher);
  }
}
