import { existsSync, readFileSync } from "fs";
import { join } from "path";

const LEGACY_PREFIX = "AGENT_BRIDGE_";
const PREFIX = "INBAND_";

// Installs made before the rename keep AGENT_BRIDGE_* names. They count as INBAND_* unless set.
export function applyLegacyEnv(env: Record<string, string | undefined> = Bun.env): void {
  for (const [name, value] of Object.entries(env)) {
    if (!name.startsWith(LEGACY_PREFIX) || value === undefined) continue;
    const current = `${PREFIX}${name.slice(LEGACY_PREFIX.length)}`;
    if (env[current] === undefined) env[current] = value;
  }
}

function defaultTokenFile(env: Record<string, string | undefined>): string | undefined {
  if (env.INBAND_TOKENS_FILE) return env.INBAND_TOKENS_FILE;
  if (!env.HOME) return undefined;
  const current = join(env.HOME, ".local/share/mcp-servers/inband/tokens.env");
  const legacy = join(env.HOME, ".local/share/mcp-servers/agent-bridge/tokens.env");
  return existsSync(current) || !existsSync(legacy) ? current : legacy;
}

export function loadTokenEnvFile(env: Record<string, string | undefined> = Bun.env): boolean {
  applyLegacyEnv(env);
  const path = defaultTokenFile(env);
  if (!path) return false;

  let text: string;
  try {
    text = readFileSync(path, "utf8");
  } catch {
    return false;
  }
  try {
    return readTokenLines(text, env);
  } finally {
    applyLegacyEnv(env);
  }
}

function readTokenLines(text: string, env: Record<string, string | undefined>): boolean {
  for (const line of text.split(/\r?\n/)) {
    const trimmed = line.trim();
    if (!trimmed || trimmed.startsWith("#")) continue;
    const match = trimmed.match(/^([A-Z0-9_]+)=(.*)$/);
    if (!match || env[match[1]] !== undefined) continue;
    let value = match[2];
    if (
      (value.startsWith('"') && value.endsWith('"')) ||
      (value.startsWith("'") && value.endsWith("'"))
    ) {
      value = value.slice(1, -1);
    }
    env[match[1]] = value;
  }
  return true;
}

export function clientTokenFromEnv(
  clientId: string,
  env: Record<string, string | undefined> = Bun.env,
): string | undefined {
  const scoped = `INBAND_${clientId.toUpperCase().replace(/[^A-Z0-9]/g, "_")}_TOKEN`;
  return env[scoped] ?? env.INBAND_TOKEN;
}
