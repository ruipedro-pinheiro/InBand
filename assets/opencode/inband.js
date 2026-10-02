/**
 * @file inband.js
 * @brief The InBand plugin for OpenCode.
 *
 * @details OpenCode gives each plugin tool the sessionID of the session that calls it.
 * The model cannot change this value. The plugin gives the sessionID to the `inband` binary,
 * and the binary signs each request for that session.
 * OpenCode loads only JavaScript plugins, so the plugin is thin: all the logic is in the binary.
 *
 * The plugin also does two more things:
 * - it puts the InBand identity and protocol of each session in its system prompt;
 * - it runs the /lead, /join and /solo commands that the user types.
 */

import { spawn } from "node:child_process";
import { tool } from "@opencode-ai/plugin";

/** @brief The `inband` binary. The installer writes its full path here. */
const BIN = process.env.INBAND_BIN || "inband";

/** @brief The time that the plugin keeps the protocol text of a session. */
const CONTEXT_TTL_MS = 60_000;

/** @brief The commands that change the team of a session. */
const TEAM_COMMANDS = new Set(["lead", "join", "solo"]);

/**
 * @brief Runs the `inband` binary.
 *
 * @param args The arguments, for example `["opencode", "tools"]`.
 * @param input The text for the standard input.
 * @param signal Stops the binary when OpenCode cancels the call.
 * @return `{ ok, text }`: `ok` is true for exit code 0. `text` is the output, or the error text.
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
 * @brief Changes one JSON Schema property into a zod type.
 *
 * @details The binary describes the tool arguments with JSON Schema. OpenCode wants zod.
 *
 * @param schema The JSON Schema of the argument.
 * @param required False makes the argument optional.
 * @return The zod type.
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
 * @brief Makes the OpenCode tools from the tool list of the binary.
 *
 * @details Each tool is `inband_<name>`. Each call gives the sessionID of the caller to the binary.
 *
 * @return The tools, by name. No tools when the binary does not run.
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

/**
 * @brief The plugin: the InBand tools, the protocol in the system prompt, and the team commands.
 */
export const InBand = async () => {
  const tools = await loadTools();
  const contexts = new Map();

  /**
   * @brief Gives the identity and the protocol of a session.
   *
   * @details The request also binds the mailbox to the session, so that wakes go to this session.
   * The text stays in memory for one minute.
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

    /** @brief Adds the InBand identity and protocol to the system prompt of each request. */
    async "experimental.chat.system.transform"(input, output) {
      if (input.sessionID) output.system.push(await sessionContext(input.sessionID));
    },

    /**
     * @brief Runs /lead, /join and /solo, and replaces the command text with the result.
     *
     * @details The model then tells the user what changed. The next request gets the new protocol.
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
