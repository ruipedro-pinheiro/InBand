import { readFileSync } from "fs";
import { join } from "path";

function defaultTokenFile(env: Record<string, string | undefined>): string | undefined {
  if (env.AGENT_BRIDGE_TOKENS_FILE) return env.AGENT_BRIDGE_TOKENS_FILE;
  return env.HOME ? join(env.HOME, ".local/share/mcp-servers/agent-bridge/tokens.env") : undefined;
}

export function loadTokenEnvFile(env: Record<string, string | undefined> = Bun.env): boolean {
  const path = defaultTokenFile(env);
  if (!path) return false;

  let text: string;
  try {
    text = readFileSync(path, "utf8");
  } catch {
    return false;
  }

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
  const scoped = `AGENT_BRIDGE_${clientId.toUpperCase().replace(/[^A-Z0-9]/g, "_")}_TOKEN`;
  return env[scoped] ?? env.AGENT_BRIDGE_TOKEN;
}
