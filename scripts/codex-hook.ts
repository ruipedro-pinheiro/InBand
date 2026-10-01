import { signInbandRequest } from "../src/auth.ts";
import { normalizeLoopbackHttpBaseUrl } from "../src/config.ts";
import { applyLegacyEnv, clientTokenFromEnv, loadTokenEnvFile } from "../src/token-env.ts";

applyLegacyEnv(process.env);
const rawUrl = process.env.INBAND_CODEX_HOOK_URL ?? "http://127.0.0.1:7447/codex/hook";
const configuredTimeout = Number(process.env.INBAND_CODEX_HOOK_TIMEOUT_MS ?? 2000);
const timeoutMs = Number.isFinite(configuredTimeout) && configuredTimeout > 0 ? configuredTimeout : 2000;

function failOpen(error: unknown): void {
  console.error(`[inband codex hook] ${error instanceof Error ? error.message : String(error)}`);
  console.log("{}");
}

try {
  loadTokenEnvFile(process.env);
  const url = normalizeLoopbackHttpBaseUrl(rawUrl, process.env);
  const text = await Bun.stdin.text();
  const payload = JSON.parse(text) as unknown;
  if (!payload || typeof payload !== "object" || Array.isArray(payload)) {
    throw new Error("stdin must contain one JSON object");
  }
  const requestBody = JSON.stringify(payload);
  const headers: Record<string, string> = { "content-type": "application/json" };
  const clientId = process.env.INBAND_CLIENT_ID ?? "codex";
  const token = clientTokenFromEnv(clientId, process.env);
  if (token) {
    Object.assign(headers, signInbandRequest({ clientId, token, method: "POST", url, body: payload }));
  }

  const response = await fetch(url, {
    method: "POST",
    headers,
    body: requestBody,
    signal: AbortSignal.timeout(timeoutMs),
  });
  if (!response.ok) throw new Error(`bridge returned HTTP ${response.status}`);

  const responseBody = (await response.json()) as unknown;
  if (!responseBody || typeof responseBody !== "object" || Array.isArray(responseBody)) {
    throw new Error("bridge returned invalid JSON");
  }
  console.log(JSON.stringify(responseBody));
} catch (error) {
  failOpen(error);
}

export {};
