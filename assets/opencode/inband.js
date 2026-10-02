/**
 * @file The InBand plugin for OpenCode.
 *
 * OpenCode gives each plugin tool the sessionID of the session that calls it, and the model cannot
 * change this value. The plugin gives the sessionID to the `inband` binary, which signs each
 * request for that session. OpenCode loads only JavaScript plugins, so the plugin stays thin: all
 * the logic is in the binary.
 *
 * The plugin also puts the InBand identity and protocol of each session in its system prompt, and
 * runs the /lead, /join and /solo commands that the user types.
 */

import { spawn } from "node:child_process";
import { tool } from "@opencode-ai/plugin";

/** The `inband` binary. The installer writes its full path here. */
const BIN = process.env.INBAND_BIN || "inband";

/** How long the plugin keeps the protocol text of a session. */
const CONTEXT_TTL_MS = 60_000;

/** The commands that change the team of a session. */
const TEAM_COMMANDS = new Set(["lead", "join", "solo"]);

/**
 * Runs the `inband` binary.
 *
 * @param {string[]} args The arguments, for example `["opencode", "tools"]`.
 * @param {string} [input] The text for the standard input.
 * @param {AbortSignal} [signal] Stops the binary when OpenCode cancels the call.
 * @returns {Promise<{ok: boolean, text: string}>} `ok` for exit code 0; `text` is the output, or
 *   the error text.
 */
function runInband(args, input, signal) {
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

/**
 * Converts one JSON Schema property to a zod type: the binary describes the tool arguments with
 * JSON Schema, and OpenCode wants zod.
 *
 * @param {object} schema The JSON Schema of the argument.
 * @param {boolean} required False makes the argument optional.
 * @returns {import("zod").ZodTypeAny}
 */
function toZod(schema, required) {
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

/**
 * Creates the OpenCode tools `inband_<name>` from the tool list of the binary. Each call gives the
 * sessionID of the caller to the binary.
 *
 * @returns {Promise<object>} The tools by name; none when the binary cannot run.
 */
async function loadTools() {
  const listed = await runInband(["opencode", "tools"]);
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
      args[key] = toZod(schema, required.has(key));
    }
    definitions[`inband_${name}`] = tool({
      description,
      args,
      async execute(values, context) {
        const result = await runInband(
          ["opencode", "--session", context.sessionID, "tool", name],
          JSON.stringify(values),
          context.abort,
        );
        return result.text;
      },
    });
  }
  return definitions;
}

/** The plugin: the InBand tools, the protocol in the system prompt, and the team commands. */
export const InBand = async () => {
  const tools = await loadTools();
  const contexts = new Map();

  /**
   * Returns the identity and the protocol of a session, kept for one minute. The request also binds
   * the mailbox to the session, so that wakes go to this session.
   *
   * @param {string} sessionID
   * @returns {Promise<string>}
   */
  async function sessionContext(sessionID) {
    const cached = contexts.get(sessionID);
    if (cached && Date.now() - cached.at < CONTEXT_TTL_MS) return cached.text;
    const result = await runInband(["opencode", "--session", sessionID, "context"]);
    const text = result.ok ? result.text : `InBand is not available for this session: ${result.text}`;
    contexts.set(sessionID, { text, at: Date.now() });
    return text;
  }

  return {
    tool: tools,

    /** Adds the InBand identity and protocol to the system prompt of each request. */
    async "experimental.chat.system.transform"(input, output) {
      if (input.sessionID) output.system.push(await sessionContext(input.sessionID));
    },

    /**
     * Runs /lead, /join and /solo, and replaces the command text with the result: the model then
     * tells the user what changed, and the next request gets the new protocol.
     */
    async "command.execute.before"(input, output) {
      if (!TEAM_COMMANDS.has(input.command)) return;
      const result = await runInband(["opencode", "--session", input.sessionID, "team", input.command, input.arguments ?? ""]);
      contexts.delete(input.sessionID);
      const text = result.ok
        ? `${result.text}\n\nTell the user in one line what changed.`
        : `The InBand command failed: ${result.text}\n\nTell the user this error in one line.`;
      for (const part of output.parts) if (part.type === "text") part.text = text;
    },
  };
};
