import type { AuthConfig } from "./auth.ts";
import type { BridgeConfig } from "./bridge.ts";
import type { CodexWakeTarget, OpencodeWakeTarget, WakeTarget } from "./wake.ts";

const LOOPBACK_BIND_HOSTS = new Set(["127.0.0.1", "localhost", "::1"]);
const AGENT_NAME_RE = /^[a-z0-9_-]{1,64}$/;
const AGENT_PATTERN_RE = /^(\*|[a-z0-9_-]{1,64}\*?)$/;

function unsafeEnabled(env: Record<string, string | undefined>, name: string): boolean {
  return /^(1|true|yes)$/i.test(env[name] ?? "");
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

function expectString(value: unknown, path: string): string {
  if (typeof value !== "string") throw new Error(`${path} must be a string`);
  return value;
}

function expectInteger(value: unknown, path: string, min: number, max: number): number {
  if (typeof value !== "number" || !Number.isInteger(value) || value < min || value > max) {
    throw new Error(`${path} must be an integer from ${min} to ${max}`);
  }
  return value;
}

export function isLoopbackBindHost(host: string): boolean {
  return LOOPBACK_BIND_HOSTS.has(host);
}

export function resolveBindHost(env: Record<string, string | undefined> = Bun.env): string {
  const host = env.AGENT_BRIDGE_BIND?.trim() || "127.0.0.1";
  if (isLoopbackBindHost(host)) return host;
  if (unsafeEnabled(env, "AGENT_BRIDGE_UNSAFE_REMOTE_BIND")) return host;
  throw new Error(
    `refusing non-loopback bind host "${host}"; set AGENT_BRIDGE_UNSAFE_REMOTE_BIND=1 only behind real auth and firewalling`,
  );
}

function isLoopbackUrlHost(hostname: string): boolean {
  const host = hostname.toLowerCase();
  if (host === "localhost" || host === "[::1]" || host === "::1") return true;
  return /^127(?:\.\d{1,3}){3}$/.test(host);
}

export function normalizeLoopbackHttpBaseUrl(
  raw: string,
  env: Record<string, string | undefined> = Bun.env,
): string {
  const text = raw.trim();
  let url: URL;
  try {
    url = new URL(text);
  } catch {
    throw new Error(`invalid URL "${raw}"`);
  }
  if (url.protocol !== "http:") {
    throw new Error(`expected http URL for local agent endpoint "${raw}"`);
  }
  if (!isLoopbackUrlHost(url.hostname) && !unsafeEnabled(env, "AGENT_BRIDGE_UNSAFE_REMOTE_URLS")) {
    throw new Error(
      `refusing non-loopback agent endpoint "${raw}"; set AGENT_BRIDGE_UNSAFE_REMOTE_URLS=1 only for a trusted private endpoint`,
    );
  }
  url.pathname = url.pathname.replace(/\/+$/, "");
  if (url.pathname === "/") url.pathname = "";
  url.search = "";
  url.hash = "";
  return url.toString().replace(/\/+$/, "");
}

function validatePrompt(value: unknown, path: string): string {
  const prompt = expectString(value, path);
  if (prompt.trim().length === 0) throw new Error(`${path} must not be empty`);
  if (Buffer.byteLength(prompt, "utf8") > 16_384) throw new Error(`${path} is too large`);
  return prompt;
}

function validateCommand(value: unknown, path: string): string {
  const command = expectString(value, path).trim();
  if (!command) throw new Error(`${path} must not be empty`);
  if (!/^[A-Za-z0-9_./-]+$/.test(command)) {
    throw new Error(`${path} must be an executable path or name, not a shell command`);
  }
  return command;
}

function validateWakeTarget(name: string, value: unknown, env: Record<string, string | undefined>): WakeTarget {
  if (!isRecord(value)) throw new Error(`wake.${name} must be an object`);
  const type = expectString(value.type, `wake.${name}.type`);
  const common = {
    prompt: validatePrompt(value.prompt, `wake.${name}.prompt`),
    debounceSeconds: expectInteger(value.debounceSeconds, `wake.${name}.debounceSeconds`, 1, 3600),
    maxWakesPerHour: expectInteger(value.maxWakesPerHour, `wake.${name}.maxWakesPerHour`, 1, 3600),
  };
  if (type === "opencode") {
    return {
      type,
      baseUrl: normalizeLoopbackHttpBaseUrl(expectString(value.baseUrl, `wake.${name}.baseUrl`), env),
      ...common,
    } satisfies OpencodeWakeTarget;
  }
  if (type === "codex") {
    const retryDelays = value.retryDelaysSeconds;
    if (!Array.isArray(retryDelays) || retryDelays.length > 16) {
      throw new Error(`wake.${name}.retryDelaysSeconds must be an array with at most 16 entries`);
    }
    return {
      type,
      command: validateCommand(value.command, `wake.${name}.command`),
      retryDelaysSeconds: retryDelays.map((delay, index) =>
        expectInteger(delay, `wake.${name}.retryDelaysSeconds[${index}]`, 1, 3600),
      ),
      ...common,
    } satisfies CodexWakeTarget;
  }
  throw new Error(`wake.${name}.type must be "opencode" or "codex"`);
}

function validateAuthConfig(value: unknown): AuthConfig | undefined {
  if (value === undefined) return undefined;
  if (!isRecord(value)) throw new Error("auth must be an object");
  const clientsRaw = value.clients;
  if (!isRecord(clientsRaw)) throw new Error("auth.clients must be an object");
  const clients: AuthConfig["clients"] = {};
  for (const [clientId, rawClient] of Object.entries(clientsRaw)) {
    if (!AGENT_NAME_RE.test(clientId)) throw new Error(`invalid auth client id "${clientId}"`);
    if (!isRecord(rawClient)) throw new Error(`auth.clients.${clientId} must be an object`);
    const agents = rawClient.agents;
    if (!Array.isArray(agents) || agents.length === 0 || !agents.every((item) => typeof item === "string")) {
      throw new Error(`auth.clients.${clientId}.agents must be a non-empty string array`);
    }
    const normalizedAgents = agents.map((agent) => agent.trim().toLowerCase());
    if (!normalizedAgents.every((agent) => AGENT_PATTERN_RE.test(agent))) {
      throw new Error(`auth.clients.${clientId}.agents contains an invalid agent pattern`);
    }
    const directory = rawClient.directory;
    if (
      directory !== undefined &&
      (!Array.isArray(directory) ||
        directory.length === 0 ||
        !directory.every((item) => typeof item === "string" && AGENT_PATTERN_RE.test(item.trim().toLowerCase())))
    ) {
      throw new Error(`auth.clients.${clientId}.directory must be a non-empty array of agent patterns`);
    }
    const token = rawClient.token === undefined ? undefined : expectString(rawClient.token, `auth.clients.${clientId}.token`);
    const tokenEnv =
      rawClient.tokenEnv === undefined ? undefined : expectString(rawClient.tokenEnv, `auth.clients.${clientId}.tokenEnv`);
    if (!token && !tokenEnv) throw new Error(`auth.clients.${clientId} must define token or tokenEnv`);
    clients[clientId] = {
      token,
      tokenEnv,
      agents: normalizedAgents,
      ...(directory === undefined ? {} : { directory: directory.map((agent: string) => agent.trim().toLowerCase()) }),
      admin: Boolean(rawClient.admin),
    };
  }
  return { required: value.required !== false, clients };
}

export function loadBridgeConfigFromText(
  raw: string,
  env: Record<string, string | undefined> = Bun.env,
): BridgeConfig {
  let parsed: unknown;
  try {
    parsed = JSON.parse(raw);
  } catch (error) {
    throw new Error(error instanceof Error ? error.message : String(error));
  }
  if (!isRecord(parsed)) throw new Error("config must be a JSON object");

  const wakeRaw = parsed.wake;
  if (!isRecord(wakeRaw)) throw new Error("wake must be an object");
  const wake: Record<string, WakeTarget> = {};
  for (const [name, target] of Object.entries(wakeRaw)) {
    if (!AGENT_NAME_RE.test(name)) throw new Error(`invalid wake target name "${name}"`);
    wake[name] = validateWakeTarget(name, target, env);
  }

  return {
    port: expectInteger(parsed.port, "port", 1, 65535),
    maxMessageBytes: expectInteger(parsed.maxMessageBytes, "maxMessageBytes", 1, 1_048_576),
    auth: validateAuthConfig(parsed.auth),
    wake,
  };
}
