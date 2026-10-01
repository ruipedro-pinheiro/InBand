export interface JsonRpcLineTransport {
  send(message: unknown): Promise<void>;
  receive(timeoutMs: number): Promise<unknown>;
  close(): Promise<void>;
}

export type CodexWakeDisposition = "started" | "queued" | "deferred-active-turn" | "failed";

export interface CodexWakeResult {
  disposition: CodexWakeDisposition;
  detail: string;
}

type JsonObject = Record<string, unknown>;

function isObject(value: unknown): value is JsonObject {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

async function request(
  transport: JsonRpcLineTransport,
  id: number,
  method: string,
  params: unknown,
  timeoutMs: number,
): Promise<unknown> {
  await transport.send({ jsonrpc: "2.0", id, method, params });
  const deadline = Date.now() + timeoutMs;

  while (true) {
    const remaining = deadline - Date.now();
    if (remaining <= 0) throw new Error(`${method} failed: timeout`);

    let message: unknown;
    try {
      message = await transport.receive(remaining);
    } catch (error) {
      throw new Error(`${method} failed: ${errorMessage(error)}`);
    }
    if (!isObject(message)) throw new Error(`${method} failed: malformed JSON-RPC response`);

    if (!("id" in message) || message.id !== id) continue;
    if (isObject(message.error)) {
      const code = message.error.code ?? "unknown";
      const text = message.error.message ?? "unknown error";
      throw new Error(`${method} JSON-RPC error ${code}: ${text}`);
    }
    if (!("result" in message)) throw new Error(`${method} failed: malformed JSON-RPC response`);
    return message.result;
  }
}

export async function wakeCodexThread(
  transport: JsonRpcLineTransport,
  input: { sessionId: string; mailbox: string; prompt: string; timeoutMs: number },
): Promise<CodexWakeResult> {
  try {
    await request(
      transport,
      1,
      "initialize",
      {
        clientInfo: { name: "agent-bridge", version: "1.0.0" },
        capabilities: { experimentalApi: true },
      },
      input.timeoutMs,
    );
    await transport.send({ jsonrpc: "2.0", method: "initialized", params: {} });

    const resumed = await request(
      transport,
      2,
      "thread/resume",
      { threadId: input.sessionId },
      input.timeoutMs,
    );
    if (!isObject(resumed) || !isObject(resumed.thread) || !isObject(resumed.thread.status)) {
      return { disposition: "failed", detail: "thread/resume failed: malformed response" };
    }

    const status = resumed.thread.status.type;
    if (status === "active") {
      return {
        disposition: "deferred-active-turn",
        detail: `thread ${input.sessionId} is active`,
      };
    }
    if (status !== "idle") {
      return {
        disposition: "failed",
        detail: `thread ${input.sessionId} status is ${String(status)}`,
      };
    }

    await request(
      transport,
      3,
      "turn/start",
      {
        threadId: input.sessionId,
        input: [
          {
            type: "text",
            text: input.prompt.includes("{mailbox}")
              ? input.prompt.replaceAll("{mailbox}", input.mailbox)
              : `${input.prompt}\n\nMailbox: ${input.mailbox}`,
          },
        ],
      },
      input.timeoutMs,
    );
    return { disposition: "started", detail: `started turn for ${input.mailbox}` };
  } catch (error) {
    return { disposition: "failed", detail: errorMessage(error) };
  } finally {
    await transport.close().catch(() => {});
  }
}

interface QueuedLine {
  value?: unknown;
  error?: Error;
}

class ProcessJsonRpcLineTransport implements JsonRpcLineTransport {
  private queue: QueuedLine[] = [];
  private waiter: ((line: QueuedLine) => void) | null = null;
  private closed = false;
  private stderr = "";
  private lingerTimer?: ReturnType<typeof setTimeout>;

  constructor(
    private process: any,
    private lingerUntilTurnCompleted = false,
  ) {
    void this.pumpStdout();
    void this.pumpStderr();
    void process.exited.then((code: number) => {
      if (!this.closed) {
        const suffix = this.stderr ? `: ${this.stderr}` : "";
        this.push({ error: new Error(`proxy exited early (${code})${suffix}`) });
      }
    });
  }

  async send(message: unknown): Promise<void> {
    if (this.closed) throw new Error("proxy is closed");
    this.process.stdin.write(`${JSON.stringify(message)}\n`);
    this.process.stdin.flush();
  }

  async receive(timeoutMs: number): Promise<unknown> {
    const queued = this.queue.shift();
    if (queued) return this.unwrap(queued);

    const line = await new Promise<QueuedLine>((resolve) => {
      const timer = setTimeout(() => {
        if (this.waiter === onLine) this.waiter = null;
        resolve({ error: new Error("timeout") });
      }, timeoutMs);
      const onLine = (value: QueuedLine) => {
        clearTimeout(timer);
        resolve(value);
      };
      this.waiter = onLine;
    });
    return this.unwrap(line);
  }

  async close(): Promise<void> {
    if (this.closed) return;
    this.closed = true;
    if (this.lingerUntilTurnCompleted) {
      const completed = this.queue.some(
        (line) => isObject(line.value) && line.value.method === "turn/completed",
      );
      this.queue = [];
      if (completed) this.finishLingeringProcess();
      else this.lingerTimer = setTimeout(() => this.finishLingeringProcess(), 30 * 60 * 1000);
      return;
    }
    this.finishProcess();
  }

  private finishProcess(): void {
    try {
      this.process.stdin.end();
    } catch {}
    try {
      this.process.kill();
    } catch {}
    if (this.waiter) {
      const waiter = this.waiter;
      this.waiter = null;
      waiter({ error: new Error("proxy closed") });
    }
  }

  private finishLingeringProcess(): void {
    if (this.lingerTimer) clearTimeout(this.lingerTimer);
    this.lingerTimer = undefined;
    this.finishProcess();
  }

  private unwrap(line: QueuedLine): unknown {
    if (line.error) throw line.error;
    return line.value;
  }

  private push(line: QueuedLine): void {
    if (this.waiter) {
      const waiter = this.waiter;
      this.waiter = null;
      waiter(line);
    } else {
      this.queue.push(line);
    }
  }

  private async pumpStdout(): Promise<void> {
    const reader = this.process.stdout.getReader();
    const decoder = new TextDecoder();
    let buffer = "";
    try {
      while (true) {
        const { done, value } = await reader.read();
        if (done) break;
        buffer += decoder.decode(value, { stream: true });
        while (true) {
          const newline = buffer.indexOf("\n");
          if (newline < 0) break;
          const line = buffer.slice(0, newline).trim();
          buffer = buffer.slice(newline + 1);
          if (!line) continue;
          try {
            const message = JSON.parse(line) as unknown;
            if (
              this.closed &&
              this.lingerUntilTurnCompleted &&
              isObject(message) &&
              message.method === "turn/completed"
            ) {
              this.finishLingeringProcess();
            } else if (!this.closed) {
              this.push({ value: message });
            }
          } catch {
            if (!this.closed) this.push({ error: new Error("malformed JSON") });
          }
        }
      }
    } catch (error) {
      if (!this.closed) this.push({ error: new Error(`proxy stdout failed: ${errorMessage(error)}`) });
    }
  }

  private async pumpStderr(): Promise<void> {
    const reader = this.process.stderr.getReader();
    const decoder = new TextDecoder();
    try {
      while (this.stderr.length < 4096) {
        const { done, value } = await reader.read();
        if (done) break;
        this.stderr = (this.stderr + decoder.decode(value, { stream: true })).slice(0, 4096);
      }
    } catch {}
  }
}

async function runDaemonStart(command: string, timeoutMs: number): Promise<void> {
  const child = Bun.spawn([command, "app-server", "daemon", "start"], {
    stdin: "ignore",
    stdout: "pipe",
    stderr: "pipe",
  });
  const timer = setTimeout(() => child.kill(), timeoutMs);
  const [code, stdout, stderr] = await Promise.all([
    child.exited,
    new Response(child.stdout).text(),
    new Response(child.stderr).text(),
  ]).finally(() => clearTimeout(timer));
  const detail = `${stdout}\n${stderr}`.trim();
  if (code !== 0 && !detail.toLowerCase().includes("already running")) {
    throw new Error(`codex app-server daemon start failed (${code}): ${detail.slice(0, 4096)}`);
  }
}

export async function createCodexAppServerTransport(
  command: string,
  timeoutMs = 5000,
): Promise<JsonRpcLineTransport> {
  await runDaemonStart(command, timeoutMs);
  const proxy = Bun.spawn([command, "app-server", "proxy"], {
    stdin: "pipe",
    stdout: "pipe",
    stderr: "pipe",
  });
  return new ProcessJsonRpcLineTransport(proxy);
}

export function createCodexStdioTransport(command: string): JsonRpcLineTransport {
  const appServer = Bun.spawn([command, "app-server", "--stdio"], {
    stdin: "pipe",
    stdout: "pipe",
    stderr: "pipe",
  });
  return new ProcessJsonRpcLineTransport(appServer, true);
}
