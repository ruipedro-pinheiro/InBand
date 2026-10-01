import { describe, expect, test } from "bun:test";
import { mkdtempSync, rmSync, writeFileSync } from "fs";
import { tmpdir } from "os";
import { join } from "path";
import { clientTokenFromEnv, loadTokenEnvFile } from "../src/token-env.ts";

describe("token env loading", () => {
  test("loads scoped client tokens from a private env file without overwriting existing env", () => {
    const dir = mkdtempSync(join(tmpdir(), "agent-bridge-tokens-"));
    try {
      const file = join(dir, "tokens.env");
      writeFileSync(
        file,
        [
          "AGENT_BRIDGE_CLAUDE_TOKEN=from-file",
          "AGENT_BRIDGE_CODEX_TOKEN='quoted-file-token'",
          "IGNORED lowercase=value",
          "",
        ].join("\n"),
        { mode: 0o600 },
      );
      const env: Record<string, string | undefined> = {
        AGENT_BRIDGE_TOKENS_FILE: file,
        AGENT_BRIDGE_CLAUDE_TOKEN: "already-set",
      };

      expect(loadTokenEnvFile(env)).toBe(true);
      expect(clientTokenFromEnv("claude", env)).toBe("already-set");
      expect(clientTokenFromEnv("codex", env)).toBe("quoted-file-token");
    } finally {
      rmSync(dir, { recursive: true, force: true });
    }
  });

  test("prefers the client token and uses AGENT_BRIDGE_TOKEN only as a fallback", () => {
    const env = { AGENT_BRIDGE_TOKEN: "generic", AGENT_BRIDGE_CLAUDE_TOKEN: "claude" };
    expect(clientTokenFromEnv("claude", env)).toBe("claude");
    expect(clientTokenFromEnv("codex", env)).toBe("generic");
  });
});
