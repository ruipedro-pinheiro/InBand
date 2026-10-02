import { createHash, createHmac, randomBytes, timingSafeEqual } from "node:crypto";
import type { AuthInfo } from "@modelcontextprotocol/sdk/server/auth/types.js";

export interface AuthClientConfig {
  token?: string;
  tokenEnv?: string;
  agents: string[];
  /** Agent patterns listed by ping and /health. Defaults to `agents`. Grants no mailbox access. */
  directory?: string[];
  admin?: boolean;
}

export interface AuthConfig {
  required: boolean;
  clients: Record<string, AuthClientConfig>;
}

export type AuthMode = "bearer" | "hmac" | "disabled";

export interface InbandAuthInfo extends AuthInfo {
  extra: {
    agents: string[];
    directory?: string[];
    admin: boolean;
    mode: AuthMode;
  };
}

interface ResolvedClient {
  clientId: string;
  token: string;
  agents: string[];
  directory: string[];
  admin: boolean;
}

export interface AuthRuntime {
  required: boolean;
  clients: Map<string, ResolvedClient>;
  nonceWindowMs: number;
  seenNonces: Map<string, number>;
}

export interface RequestToSign {
  method: string;
  url: string;
  body?: unknown;
}

export interface SignRequestInput extends RequestToSign {
  clientId: string;
  token: string;
  nowMs?: number;
  nonce?: string;
}

export const AUTH_CLIENT_HEADER = "x-inband-client";
export const AUTH_TIMESTAMP_HEADER = "x-inband-timestamp";
export const AUTH_NONCE_HEADER = "x-inband-nonce";
export const AUTH_SIGNATURE_HEADER = "x-inband-signature";

const TOKEN_MIN_LENGTH = 32;
const NONCE_RE = /^[a-zA-Z0-9._:-]{6,128}$/;
const AGENT_PATTERN_RE = /^(\*|[a-z0-9_-]{1,64}\*?)$/;

function stableStringify(value: unknown): string {
  if (value === undefined) return "";
  if (value === null || typeof value !== "object") return JSON.stringify(value);
  if (Array.isArray(value)) return `[${value.map((item) => stableStringify(item)).join(",")}]`;
  const obj = value as Record<string, unknown>;
  return `{${Object.keys(obj)
    .sort()
    .map((key) => `${JSON.stringify(key)}:${stableStringify(obj[key])}`)
    .join(",")}}`;
}

function bodyDigest(body: unknown): string {
  return createHash("sha256").update(stableStringify(body)).digest("hex");
}

function pathAndQuery(rawUrl: string): string {
  const url = new URL(rawUrl, "http://127.0.0.1");
  return `${url.pathname}${url.search}`;
}

function signingPayload(input: RequestToSign & { clientId: string; timestamp: string; nonce: string }): string {
  return [
    input.method.toUpperCase(),
    pathAndQuery(input.url),
    bodyDigest(input.body),
    input.timestamp,
    input.nonce,
    input.clientId,
  ].join("\n");
}

function constantEqual(a: string, b: string): boolean {
  const left = createHash("sha256").update(a).digest();
  const right = createHash("sha256").update(b).digest();
  return timingSafeEqual(left, right) && a.length === b.length;
}

function tokenFromConfig(clientId: string, client: AuthClientConfig, env: Record<string, string | undefined>): string {
  const token = client.tokenEnv ? env[client.tokenEnv] : client.token;
  if (!token) throw new Error(`auth client "${clientId}" has no token`);
  if (token.length < TOKEN_MIN_LENGTH) {
    throw new Error(`auth client "${clientId}" token is too short; expected at least ${TOKEN_MIN_LENGTH} chars`);
  }
  if (/(change[-_ ]?me|placeholder|example|secret|password)/i.test(token) || /^token$/i.test(token)) {
    throw new Error(`auth client "${clientId}" token is a placeholder`);
  }
  return token;
}

export function buildAuthRuntime(
  config: AuthConfig | undefined,
  env: Record<string, string | undefined> = Bun.env,
): AuthRuntime {
  if (!config) return { required: false, clients: new Map(), nonceWindowMs: 300_000, seenNonces: new Map() };

  const clients = new Map<string, ResolvedClient>();
  const tokens = new Set<string>();
  for (const [clientId, client] of Object.entries(config.clients ?? {})) {
    if (!/^[a-z0-9_-]{1,64}$/.test(clientId)) {
      throw new Error(`invalid auth client id "${clientId}"`);
    }
    if (!Array.isArray(client.agents) || client.agents.length === 0) {
      throw new Error(`auth client "${clientId}" must allow at least one agent pattern`);
    }
    const agents = client.agents.map((agent) => agent.trim().toLowerCase());
    if (!agents.every((agent) => AGENT_PATTERN_RE.test(agent))) {
      throw new Error(`auth client "${clientId}" contains an invalid agent pattern`);
    }
    const directory = client.directory?.map((agent) => agent.trim().toLowerCase()) ?? agents;
    if (directory.length === 0 || !directory.every((agent) => AGENT_PATTERN_RE.test(agent))) {
      throw new Error(`auth client "${clientId}" contains an invalid directory pattern`);
    }
    const token = tokenFromConfig(clientId, client, env);
    if (tokens.has(token)) throw new Error(`duplicate auth token configured for client "${clientId}"`);
    tokens.add(token);
    clients.set(clientId, {
      clientId,
      token,
      agents,
      directory,
      admin: Boolean(client.admin),
    });
  }

  if (config.required && clients.size === 0) {
    throw new Error("auth.required is true but no auth clients are configured");
  }
  return { required: Boolean(config.required), clients, nonceWindowMs: 300_000, seenNonces: new Map() };
}

function authInfo(client: ResolvedClient, mode: AuthMode): InbandAuthInfo {
  return {
    token: "[redacted]",
    clientId: client.clientId,
    scopes: client.admin ? ["admin"] : client.agents.map((pattern) => `agent:${pattern}`),
    extra: {
      agents: client.admin ? ["*"] : client.agents,
      directory: client.admin ? ["*"] : client.directory,
      admin: client.admin,
      mode,
    },
  };
}

export function disabledAuthInfo(): InbandAuthInfo {
  return {
    token: "[auth-disabled]",
    clientId: "auth-disabled",
    scopes: ["admin"],
    extra: { agents: ["*"], directory: ["*"], admin: true, mode: "disabled" },
  };
}

export function authenticateAuthorizationHeader(runtime: AuthRuntime, authorization: string | undefined): InbandAuthInfo {
  if (!runtime.required && !authorization) return disabledAuthInfo();
  if (!authorization) throw new Error("missing Authorization header");

  const match = authorization.match(/^Bearer\s+(.+)$/i);
  if (!match) throw new Error("missing bearer token");

  for (const client of runtime.clients.values()) {
    if (constantEqual(match[1], client.token)) return authInfo(client, "bearer");
  }
  throw new Error("invalid bearer token");
}

function getHeader(headers: Record<string, string | undefined>, name: string): string | undefined {
  const value = headers[name] ?? headers[name.toLowerCase()] ?? headers[name.toUpperCase()];
  if (value !== undefined || !name.startsWith("x-inband-")) return value;
  // Hooks installed before the rename send x-agent-bridge-* headers with the same signature.
  const legacy = name.replace("x-inband-", "x-agent-bridge-");
  return headers[legacy] ?? headers[legacy.toLowerCase()];
}

function purgeOldNonces(runtime: AuthRuntime, nowMs: number): void {
  const cutoff = nowMs - runtime.nonceWindowMs;
  for (const [key, seenAt] of runtime.seenNonces) {
    if (seenAt < cutoff) runtime.seenNonces.delete(key);
  }
}

export function signInbandRequest(input: SignRequestInput): Record<string, string> {
  const timestamp = String(input.nowMs ?? Date.now());
  const nonce = input.nonce ?? randomBytes(16).toString("hex");
  const signature = createHmac("sha256", input.token)
    .update(signingPayload({ ...input, timestamp, nonce }))
    .digest("hex");
  return {
    [AUTH_CLIENT_HEADER]: input.clientId,
    [AUTH_TIMESTAMP_HEADER]: timestamp,
    [AUTH_NONCE_HEADER]: nonce,
    [AUTH_SIGNATURE_HEADER]: `sha256=${signature}`,
  };
}

export function authenticateSignedRequest(
  runtime: AuthRuntime,
  request: RequestToSign & { headers: Record<string, string | undefined> },
  nowMs = Date.now(),
): InbandAuthInfo {
  if (!runtime.required) return disabledAuthInfo();

  const clientId = getHeader(request.headers, AUTH_CLIENT_HEADER);
  const timestamp = getHeader(request.headers, AUTH_TIMESTAMP_HEADER);
  const nonce = getHeader(request.headers, AUTH_NONCE_HEADER);
  const signature = getHeader(request.headers, AUTH_SIGNATURE_HEADER);
  if (!clientId || !timestamp || !nonce || !signature) {
    throw new Error("missing inband signed auth headers");
  }

  const client = runtime.clients.get(clientId);
  if (!client) throw new Error("unknown inband auth client");
  const timestampMs = Number(timestamp);
  if (!Number.isFinite(timestampMs) || Math.abs(nowMs - timestampMs) > runtime.nonceWindowMs) {
    throw new Error("stale inband signed request");
  }
  if (!NONCE_RE.test(nonce)) throw new Error("invalid inband auth nonce");

  purgeOldNonces(runtime, nowMs);
  const nonceKey = `${clientId}:${nonce}`;
  if (runtime.seenNonces.has(nonceKey)) throw new Error("inband signed request replay detected");

  const expected = signInbandRequest({
    clientId,
    token: client.token,
    method: request.method,
    url: request.url,
    body: request.body,
    nowMs: timestampMs,
    nonce,
  })[AUTH_SIGNATURE_HEADER];
  if (!constantEqual(signature, expected)) throw new Error("invalid inband request signature");

  runtime.seenNonces.set(nonceKey, nowMs);
  return authInfo(client, "hmac");
}

export function authenticateRequest(
  runtime: AuthRuntime,
  request: RequestToSign & { headers: Record<string, string | undefined> },
): InbandAuthInfo {
  const authorization = getHeader(request.headers, "authorization");
  if (authorization) return authenticateAuthorizationHeader(runtime, authorization);
  return authenticateSignedRequest(runtime, request);
}

export function agentMatchesPattern(agentRaw: string, patternRaw: string): boolean {
  const agent = agentRaw.trim().toLowerCase();
  const pattern = patternRaw.trim().toLowerCase();
  if (pattern === "*") return true;
  if (pattern.endsWith("*")) return agent.startsWith(pattern.slice(0, -1));
  return agent === pattern;
}

export function assertAgentAuthorized(auth: InbandAuthInfo, agent: string, field: string): void {
  if (auth.extra.admin) return;
  if (auth.extra.agents.some((pattern) => agentMatchesPattern(agent, pattern))) return;
  throw new Error(`auth client "${auth.clientId}" is not authorized to use ${field}="${agent}"`);
}

export function assertFamilyAuthorized(auth: InbandAuthInfo, prefix: string, field: string): void {
  if (auth.extra.admin) return;
  const familyPattern = `${prefix.trim().toLowerCase()}-*`;
  if (auth.extra.agents.some((pattern) => pattern === "*" || pattern === familyPattern)) return;
  throw new Error(`auth client "${auth.clientId}" is not authorized to use ${field}="${prefix}"`);
}

export function assertAdmin(auth: InbandAuthInfo): void {
  if (!auth.extra.admin) throw new Error(`auth client "${auth.clientId}" is not authorized for admin operations`);
}

export function visibleAgentPatterns(auth: InbandAuthInfo): string[] | undefined {
  return auth.extra.admin ? undefined : auth.extra.agents;
}

// Presence listing only. History and mailbox access keep using visibleAgentPatterns.
export function directoryAgentPatterns(auth: InbandAuthInfo): string[] | undefined {
  if (auth.extra.admin) return undefined;
  const directory = auth.extra.directory ?? auth.extra.agents;
  return directory.includes("*") ? undefined : directory;
}
