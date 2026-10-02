// InBand for OpenCode.
//
// OpenCode gives each plugin tool the sessionID of the session that calls it, and the model cannot
// change it. This plugin passes that sessionID to the `inband` binary, which signs every request
// for that session. All the logic stays in the binary: OpenCode loads only JavaScript plugins.
//
// It also gives each session its InBand identity and protocol in the system prompt, and runs the
// /lead, /join and /solo commands that the user types.

import { spawn } from "node:child_process";
import { tool } from "@opencode-ai/plugin";

const BIN = process.env.INBAND_BIN || "inband";
const CONTEXT_TTL_MS = 60_000;
const TEAM_COMMANDS = new Set(["lead", "join", "solo"]);

function run(args, input, signal) {
  return new Promise((resolve) => {
    const child = spawn(BIN, args, { stdio: ["pipe", "pipe", "pipe"], signal });
    let out = "";
    let err = "";
    child.stdout.on("data", (chunk) => (out += chunk));
    child.stderr.on("data", (chunk) => (err += chunk));
    child.on("error", (error) => resolve({ ok: false, text: `inband: ${error.message}` }));
    child.on("close", (code) => resolve({ ok: code === 0, text: (code === 0 ? out : err || out).trim() }));
    child.stdin.end(input ?? "");
  });
}

// The daemon describes its tools with JSON Schema; OpenCode wants zod.
function zodOf(schema, required) {
  const z = tool.schema;
  let type;
  switch (schema.type) {
    case "string": type = z.string(); break;
    case "integer": type = z.number().int(); break;
    case "number": type = z.number(); break;
    case "boolean": type = z.boolean(); break;
    default: type = z.any();
  }
  if (schema.description) type = type.describe(schema.description);
  return required ? type : type.optional();
}

export const InBand = async () => {
  const listed = await run(["opencode", "tools"]);
  let tools = [];
  try {
    tools = listed.ok ? JSON.parse(listed.text) : [];
  } catch {}
  if (!listed.ok) console.error(`[inband] no tools: ${listed.text}`);

  const definitions = {};
  for (const { name, description, inputSchema } of tools) {
    const required = new Set(inputSchema?.required ?? []);
    const args = {};
    for (const [key, schema] of Object.entries(inputSchema?.properties ?? {})) {
      args[key] = zodOf(schema, required.has(key));
    }
    definitions[`inband_${name}`] = tool({
      description,
      args,
      async execute(values, context) {
        const result = await run(
          ["opencode", "--session", context.sessionID, "tool", name],
          JSON.stringify(values),
          context.abort,
        );
        return result.text;
      },
    });
  }

  const contexts = new Map();
  async function sessionContext(sessionID) {
    const cached = contexts.get(sessionID);
    if (cached && Date.now() - cached.at < CONTEXT_TTL_MS) return cached.text;
    const result = await run(["opencode", "--session", sessionID, "context"]);
    const text = result.ok ? result.text : `InBand is not available for this session: ${result.text}`;
    contexts.set(sessionID, { text, at: Date.now() });
    return text;
  }

  return {
    tool: definitions,
    async "experimental.chat.system.transform"(input, output) {
      if (input.sessionID) output.system.push(await sessionContext(input.sessionID));
    },
    async "command.execute.before"(input, output) {
      if (!TEAM_COMMANDS.has(input.command)) return;
      const result = await run(["opencode", "--session", input.sessionID, "team", input.command, input.arguments ?? ""]);
      contexts.delete(input.sessionID);
      const text = result.ok
        ? `${result.text}\n\nTell the user in one line what changed.`
        : `The InBand command failed: ${result.text}\n\nTell the user this error in one line.`;
      for (const part of output.parts) if (part.type === "text") part.text = text;
    },
  };
};
