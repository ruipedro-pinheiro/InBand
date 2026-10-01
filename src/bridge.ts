import type { Database } from "bun:sqlite";
import { agentMatchesPattern, type AuthConfig } from "./auth.ts";
import { CODEX_FAMILY, CodexSessionRegistry } from "./codex-session.ts";
import type { CodexWakeResult } from "./codex-app-server.ts";
import type { MessageRow } from "./db.ts";
import type { AgentRole } from "./protocol.ts";
import {
  wakeCodex,
  wakeOpencode,
  type CodexWakeTarget,
  type WakeTarget,
} from "./wake.ts";

export interface BridgeConfig {
  port: number;
  maxMessageBytes: number;
  auth?: AuthConfig;
  wake: Record<string, WakeTarget>;
}

export interface WakeDispatchInput {
  recipient: string;
  sessionId?: string;
  mailbox?: string;
  prompt: string;
}

export interface BridgeRuntime {
  dispatchWake?: (target: WakeTarget, input: WakeDispatchInput) => Promise<CodexWakeResult>;
  now?: () => Date;
  setTimeout?: (callback: () => void, delayMs: number) => ReturnType<typeof setTimeout>;
  clearTimeout?: (timer: ReturnType<typeof setTimeout>) => void;
}

const AGENT_NAME_RE = /^[a-z0-9_-]{1,64}$/;
const CANONICAL_CODEX_MAILBOX_RE =
  /^codex-[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/;
const MAX_PENDING_WAITS_PER_AGENT = 8;
const MAX_PENDING_SUBSCRIPTIONS_PER_TARGET = 8;

interface Waiter {
  resolve: () => void;
  timer: ReturnType<typeof setTimeout>;
}

interface FamilyWaiter {
  prefix: string;
  exact?: boolean;
  afterId: number;
  resolve: (rows: MessageRow[]) => void;
  timer: ReturnType<typeof setTimeout>;
}

interface CodexRetryState {
  nextDelayIndex: number;
  timer?: ReturnType<typeof setTimeout>;
}

async function dispatchWakeDefault(
  target: WakeTarget,
  input: WakeDispatchInput,
): Promise<CodexWakeResult> {
  if (target.type === "opencode") return wakeOpencode(target);
  if (!input.sessionId || !input.mailbox) {
    return { disposition: "failed", detail: "missing registered Codex wake identity" };
  }
  return wakeCodex(target.command, {
    sessionId: input.sessionId,
    mailbox: input.mailbox,
    prompt: input.prompt,
  });
}

export class Bridge {
  private waiters = new Map<string, Waiter[]>();
  private familyWaiters: FamilyWaiter[] = [];
  private startedAt: string;
  private codexRetries = new Map<string, CodexRetryState>();
  private runtime: Required<BridgeRuntime>;

  constructor(
    private db: Database,
    private config: BridgeConfig,
    private codexSessions = new CodexSessionRegistry(db),
    runtime: BridgeRuntime = {},
  ) {
    this.runtime = {
      dispatchWake: runtime.dispatchWake ?? dispatchWakeDefault,
      now: runtime.now ?? (() => new Date()),
      setTimeout: runtime.setTimeout ?? ((callback, delayMs) => setTimeout(callback, delayMs)),
      clearTimeout: runtime.clearTimeout ?? ((timer) => clearTimeout(timer)),
    };
    this.startedAt = this.runtime.now().toISOString();
  }

  normalizeAgent(raw: string, field: string): string {
    const name = raw.trim().toLowerCase();
    if (!AGENT_NAME_RE.test(name)) {
      throw new Error(`invalid ${field} "${raw}": expected 1-64 chars of [a-z0-9_-]`);
    }
    return name;
  }

  touchAgent(name: string): void {
    this.requireConcreteAgentIdentity(name);
    if (this.isCanonicalCodexMailbox(name)) {
      this.requireRegisteredCodexMailbox(name);
      this.codexSessions.touchMailbox(name);
      return;
    }
    const now = this.runtime.now().toISOString();
    this.db
      .query(
        `INSERT INTO agents(name, first_seen, last_seen) VALUES (?1, ?2, ?2)
         ON CONFLICT(name) DO UPDATE SET last_seen = ?2`,
      )
      .run(name, now);
  }

  send(fromRaw: string, toRaw: string, content: string) {
    const from = this.normalizeAgent(fromRaw, "from");
    const requestedTo =
      toRaw.trim().toLowerCase() === "all" ? "all" : this.normalizeAgent(toRaw, "to");
    this.requireConcreteAgentIdentity(from);
    const size = Buffer.byteLength(content, "utf8");
    if (size === 0) throw new Error("content is empty");
    if (size > this.config.maxMessageBytes) {
      throw new Error(`content is ${size} bytes; max is ${this.config.maxMessageBytes}`);
    }

    const now = new Date().toISOString();

    // atomic, a concurrent registration must not change the target session
    const routeAndInsert = this.db.transaction(() => {
      this.requireRegisteredCodexMailbox(from);

      let resolvedTo = requestedTo;
      if (requestedTo === CODEX_FAMILY) {
        const session = this.codexSessions.mostRecent();
        if (!session) {
          throw new Error(`cannot send to "${CODEX_FAMILY}": no registered Codex session`);
        }
        resolvedTo = session.mailbox;
      } else {
        this.requireRegisteredCodexMailbox(requestedTo);
      }

      if (resolvedTo === from) throw new Error("cannot send a message to yourself");

      this.touchAgent(from);

      let recipients: string[];
      if (resolvedTo === "all") {
        recipients = (
          this.db.query(`SELECT name FROM agents WHERE name != ?1`).all(from) as { name: string }[]
        ).map((r) => r.name);
      } else {
        this.touchAgentIfNew(resolvedTo);
        recipients = [resolvedTo];
      }

      const senderRole = this.roleOf(from);
      const { lastInsertRowid } = this.db
        .query(
          `INSERT INTO messages(sender, recipient, content, created_at, sender_role) VALUES (?1, ?2, ?3, ?4, ?5)`,
        )
        .run(from, resolvedTo, content, now, senderRole);
      const id = Number(lastInsertRowid);
      const deliver = this.db.query(
        `INSERT INTO deliveries(message_id, recipient, read_at) VALUES (?1, ?2, NULL)`,
      );
      for (const r of recipients) deliver.run(id, r);
      return { messageId: id, resolvedTo, recipients, senderRole };
    });
    const { messageId, resolvedTo, recipients, senderRole } = routeAndInsert();

    if (this.familyWaiters.length > 0) {
      const remaining: FamilyWaiter[] = [];
      for (const fw of this.familyWaiters) {
        const rows: MessageRow[] = recipients
          .filter((r) => messageId > fw.afterId && (r === fw.prefix || (!fw.exact && r.startsWith(fw.prefix + "-"))))
          .map((r) => ({
            id: messageId,
            sender: from,
            recipient: r,
            content,
            created_at: now,
            sender_role: senderRole,
          }));
        if (rows.length > 0) {
          clearTimeout(fw.timer);
          fw.resolve(rows);
        } else {
          remaining.push(fw);
        }
      }
      this.familyWaiters = remaining;
    }

    const wakes: Record<string, string> = {};
    const warnings: string[] = [];
    for (const recipient of recipients) {
      if (this.resolveWaiters(recipient)) {
        wakes[recipient] = "delivered-to-waiting-agent";
      } else {
        wakes[recipient] = this.maybeWake(recipient);
      }
      const p = this.presenceOf(recipient);
      if (p === "offline") {
        warnings.push(
          `"${recipient}" is DISCONNECTED (its session ended). The message is queued and will only be read if that session comes back; do not wait for a reply.`,
        );
      }
    }

    return {
      messageId,
      sentAt: now,
      requestedTo,
      resolvedTo,
      deliveredTo: recipients,
      notify: wakes,
      warnings: warnings.length > 0 ? warnings : undefined,
    };
  }

  getLead(): string | null {
    const row = this.db.query(`SELECT value FROM settings WHERE key = 'lead'`).get() as { value: string } | null;
    return row?.value ?? null;
  }

  roleOf(name: string): AgentRole {
    return this.getLead() === name ? "lead" : "worker";
  }

  // The user picks the lead. The previous lead gets a notice so it stops acting as lead.
  setLead(mailboxRaw: string): { lead: string; previous: string | null } {
    const mailbox = this.normalizeAgent(mailboxRaw, "from");
    this.touchAgent(mailbox);
    const previous = this.getLead();
    this.db
      .query(
        `INSERT INTO settings(key, value, updated_at) VALUES ('lead', ?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = ?1, updated_at = ?2`,
      )
      .run(mailbox, this.runtime.now().toISOString());
    if (previous && previous !== mailbox) {
      try {
        this.send(mailbox, previous, `${mailbox} is now the lead. You are a worker from now on.`);
      } catch (error) {
        // A previous lead whose Codex session is gone cannot receive mail. The change still applies.
        console.error(`[lead] could not notify ${previous}:`, error instanceof Error ? error.message : error);
      }
    }
    return { lead: mailbox, previous: previous === mailbox ? null : previous };
  }

  private touchAgentIfNew(name: string): void {
    const now = new Date().toISOString();
    this.db
      .query(
        `INSERT INTO agents(name, first_seen, last_seen) VALUES (?1, ?2, NULL)
         ON CONFLICT(name) DO NOTHING`,
      )
      .run(name, now);
  }

  private isCanonicalCodexMailbox(name: string): boolean {
    return CANONICAL_CODEX_MAILBOX_RE.test(name);
  }

  private requireConcreteAgentIdentity(name: string): void {
    if (name === CODEX_FAMILY) {
      throw new Error(`"${CODEX_FAMILY}" is a recipient-only alias, not an agent identity`);
    }
  }

  private requireRegisteredCodexMailbox(name: string): void {
    if (this.isCanonicalCodexMailbox(name) && !this.codexSessions.getByMailbox(name)) {
      throw new Error(`Codex mailbox "${name}" is not registered`);
    }
  }

  peekUnread(forRaw: string): MessageRow[] {
    const recipient = this.normalizeAgent(forRaw, "for");
    this.touchAgent(recipient);
    return this.db
      .query(
        `SELECT m.id, m.sender, m.recipient, m.content, m.created_at, m.sender_role
         FROM deliveries d JOIN messages m ON m.id = d.message_id
         WHERE d.recipient = ?1 AND d.read_at IS NULL
         ORDER BY m.id ASC`,
      )
      .all(recipient) as MessageRow[];
  }

  fetchUnread(forRaw: string): MessageRow[] {
    const recipient = this.normalizeAgent(forRaw, "for");
    this.touchAgent(recipient);
    const now = new Date().toISOString();
    const read = this.db.transaction(() => {
      const rows = this.db
        .query(
          `SELECT m.id, m.sender, m.recipient, m.content, m.created_at, m.sender_role
           FROM deliveries d JOIN messages m ON m.id = d.message_id
           WHERE d.recipient = ?1 AND d.read_at IS NULL
           ORDER BY m.id ASC`,
        )
        .all(recipient) as MessageRow[];
      if (rows.length > 0) {
        this.db
          .query(`UPDATE deliveries SET read_at = ?1 WHERE recipient = ?2 AND read_at IS NULL`)
          .run(now, recipient);
      }
      return rows;
    });
    const rows = read();
    if (this.isCanonicalCodexMailbox(recipient) && this.unreadCount(recipient) === 0) {
      this.cancelCodexRetry(recipient);
    }
    return rows;
  }

  // no progressToken, the client timer expires at 60 s (SDK default)
  // with one, progress acts as keepalive (sst/opencode PR #32477)
  // capped anyway, OpenCode #35207 and #31235 wedge on a call that never returns
  private static readonly MAX_WAIT_SECONDS = 50;
  private static readonly MAX_LONG_WAIT_SECONDS = 1800;

  async waitForMessages(
    forRaw: string,
    timeoutSeconds: number,
    signal?: AbortSignal,
    longWait = false,
  ): Promise<MessageRow[]> {
    const recipient = this.normalizeAgent(forRaw, "for");
    const cap = longWait ? Bridge.MAX_LONG_WAIT_SECONDS : Bridge.MAX_WAIT_SECONDS;
    const timeout = Math.min(Math.max(Math.floor(timeoutSeconds), 5), cap);

    // peek only, a reply lost to a dead socket must not mark anything read
    const immediate = this.peekUnread(recipient);
    if (immediate.length > 0) return immediate;

    let aborted = false;
    await new Promise<void>((resolve) => {
      const list = this.waiters.get(recipient) ?? [];
      if (list.length >= MAX_PENDING_WAITS_PER_AGENT) {
        throw new Error(`too many pending waits for "${recipient}"`);
      }
      const waiter: Waiter = {
        resolve,
        timer: this.runtime.setTimeout(() => {
          this.removeWaiter(recipient, waiter);
          resolve();
        }, timeout * 1000),
      };
      list.push(waiter);
      this.waiters.set(recipient, list);

      // dropped connection, release the waiter without consuming
      if (signal) {
        const onAbort = () => {
          aborted = true;
          this.runtime.clearTimeout(waiter.timer);
          this.removeWaiter(recipient, waiter);
          resolve();
        };
        if (signal.aborted) onAbort();
        else signal.addEventListener("abort", onAbort, { once: true });
      }
    });

    if (aborted) return []; // dead connection: nothing peeked, nothing lost
    return this.peekUnread(recipient);
  }

  async subscribeFamily(
    prefixRaw: string,
    timeoutSeconds: number,
    onClose?: (cleanup: () => void) => void,
    afterId?: number,
  ): Promise<MessageRow[]> {
    const prefix = this.normalizeAgent(prefixRaw, "prefix");
    return this.subscribeChannel(prefix, false, timeoutSeconds, onClose, afterId);
  }

  async subscribeMailbox(
    mailboxRaw: string,
    timeoutSeconds: number,
    onClose?: (cleanup: () => void) => void,
    afterId?: number,
  ): Promise<MessageRow[]> {
    const mailbox = this.normalizeAgent(mailboxRaw, "mailbox");
    this.touchAgent(mailbox);
    return this.subscribeChannel(mailbox, true, timeoutSeconds, onClose, afterId);
  }

  private async subscribeChannel(
    prefix: string,
    exact: boolean,
    timeoutSeconds: number,
    onClose?: (cleanup: () => void) => void,
    afterId?: number,
  ): Promise<MessageRow[]> {
    // Old shims omit the cursor. Keep their live-only behavior to avoid replay loops.
    if (afterId !== undefined) {
      if (!Number.isSafeInteger(afterId) || afterId < 0) throw new Error("invalid after_id");
      const queued = this.db.query(
        `SELECT m.id, m.sender, d.recipient, m.content, m.created_at, m.sender_role
         FROM deliveries d JOIN messages m ON m.id = d.message_id
         WHERE d.read_at IS NULL AND m.id > ?1
           AND (d.recipient = ?2 OR (?3 = 0 AND substr(d.recipient, 1, length(?2) + 1) = ?2 || '-'))
         ORDER BY m.id ASC, d.recipient ASC`,
      ).all(afterId, prefix, exact ? 1 : 0) as MessageRow[];
      if (queued.length > 0) return queued;
    }
    const timeout = Math.min(Math.max(Math.floor(timeoutSeconds), 1), 300);
    return new Promise<MessageRow[]>((resolve) => {
      const matchingWaiters = this.familyWaiters.filter((fw) => fw.prefix === prefix && fw.exact === exact);
      if (matchingWaiters.length >= MAX_PENDING_SUBSCRIPTIONS_PER_TARGET) {
        throw new Error(`too many pending subscriptions for "${prefix}"`);
      }
      const fw: FamilyWaiter = {
        prefix,
        exact,
        afterId: afterId ?? 0,
        resolve,
        timer: this.runtime.setTimeout(() => {
          this.removeFamilyWaiter(fw);
          resolve([]);
        }, timeout * 1000),
      };
      this.familyWaiters.push(fw);
      onClose?.(() => {
        this.runtime.clearTimeout(fw.timer);
        this.removeFamilyWaiter(fw);
        resolve([]); // no-op if already resolved by a send
      });
    });
  }

  private removeFamilyWaiter(fw: FamilyWaiter): void {
    const idx = this.familyWaiters.indexOf(fw);
    if (idx >= 0) this.familyWaiters.splice(idx, 1);
  }

  private resolveWaiters(recipient: string): boolean {
    const list = this.waiters.get(recipient);
    if (!list || list.length === 0) return false;
    this.waiters.delete(recipient);
    for (const w of list) {
      this.runtime.clearTimeout(w.timer);
      w.resolve();
    }
    return true;
  }

  private removeWaiter(recipient: string, waiter: Waiter): void {
    const list = this.waiters.get(recipient);
    if (!list) return;
    const idx = list.indexOf(waiter);
    if (idx >= 0) list.splice(idx, 1);
    if (list.length === 0) this.waiters.delete(recipient);
  }

  private maybeWake(recipient: string): string {
    const isCodex = this.isCanonicalCodexMailbox(recipient);
    const target = this.config.wake[isCodex ? CODEX_FAMILY : recipient];
    if (!target) return "no-wake-configured";

    if (isCodex && target.type !== "codex") return "wake-misconfigured: codex target required";
    if (!isCodex && target.type !== "opencode") return "wake-misconfigured: opencode target required";

    const suppressed = this.wakeSuppression(recipient, target);
    if (suppressed) return suppressed;

    if (target.type === "codex") {
      if (this.codexRetries.has(recipient)) return "wake-retry-pending";
      this.codexRetries.set(recipient, { nextDelayIndex: 0 });
      void this.runCodexWake(recipient, target);
      return "wake-dispatched";
    }

    void this.runtime
      .dispatchWake(target, { recipient, prompt: target.prompt })
      .then((result) => this.recordWake(recipient, result))
      .catch((error) =>
        this.recordWake(recipient, {
          disposition: "failed",
          detail: error instanceof Error ? error.message : String(error),
        }),
      );
    return "wake-dispatched";
  }

  private wakeSuppression(recipient: string, target: WakeTarget): string | null {
    const now = this.runtime.now().getTime();

    const debounceSeconds = target.debounceSeconds;
    const maxWakesPerHour = target.maxWakesPerHour;

    const oneHourAgo = new Date(now - 3_600_000).toISOString();
    const { n: wakesLastHour } = this.db
      .query(`SELECT COUNT(*) AS n FROM wakes WHERE recipient = ?1 AND created_at > ?2`)
      .get(recipient, oneHourAgo) as { n: number };
    if (wakesLastHour >= maxWakesPerHour) {
      return `wake-suppressed: ${wakesLastHour} wakes in the last hour (cap ${maxWakesPerHour})`;
    }

    const debounceCutoff = new Date(now - debounceSeconds * 1000).toISOString();
    const recent = this.db
      .query(`SELECT COUNT(*) AS n FROM wakes WHERE recipient = ?1 AND created_at > ?2 AND ok = 1`)
      .get(recipient, debounceCutoff) as { n: number };
    if (recent.n > 0) return `wake-debounced (last wake < ${debounceSeconds}s ago)`;
    return null;
  }

  private async runCodexWake(recipient: string, target: CodexWakeTarget): Promise<void> {
    const state = this.codexRetries.get(recipient);
    if (!state) return;
    if (this.unreadCount(recipient) === 0) {
      this.cancelCodexRetry(recipient);
      return;
    }

    const suppressed = this.wakeSuppression(recipient, target);
    if (suppressed) {
      this.cancelCodexRetry(recipient);
      return;
    }
    const session = this.codexSessions.getByMailbox(recipient);
    if (!session) {
      this.cancelCodexRetry(recipient);
      return;
    }

    let result: CodexWakeResult;
    try {
      result = await this.runtime.dispatchWake(target, {
        recipient,
        sessionId: session.session_id,
        mailbox: session.mailbox,
        prompt: target.prompt,
      });
    } catch (error) {
      result = {
        disposition: "failed",
        detail: error instanceof Error ? error.message : String(error),
      };
    }
    this.recordWake(recipient, result);

    if (this.codexRetries.get(recipient) !== state) return;
    if (result.disposition === "started" || result.disposition === "queued" || this.unreadCount(recipient) === 0) {
      this.cancelCodexRetry(recipient);
      return;
    }
    if (state.nextDelayIndex >= target.retryDelaysSeconds.length) {
      this.cancelCodexRetry(recipient);
      return;
    }

    const delayMs = target.retryDelaysSeconds[state.nextDelayIndex++] * 1000;
    state.timer = this.runtime.setTimeout(() => {
      state.timer = undefined;
      void this.runCodexWake(recipient, target);
    }, delayMs);
  }

  private recordWake(recipient: string, result: CodexWakeResult): void {
    const ok = result.disposition === "started" || result.disposition === "queued";
    const detail = `${result.disposition}: ${result.detail}`;
    this.db
      .query(`INSERT INTO wakes(recipient, created_at, ok, detail) VALUES (?1, ?2, ?3, ?4)`)
      .run(recipient, this.runtime.now().toISOString(), ok ? 1 : 0, detail);
    console.error(`[wake] ${recipient}: ${ok ? "OK" : "FAIL"} - ${detail}`);
  }

  private unreadCount(recipient: string): number {
    return (
      this.db
        .query(`SELECT COUNT(*) AS n FROM deliveries WHERE recipient = ?1 AND read_at IS NULL`)
        .get(recipient) as { n: number }
    ).n;
  }

  private cancelCodexRetry(recipient: string): void {
    const state = this.codexRetries.get(recipient);
    if (!state) return;
    if (state.timer) this.runtime.clearTimeout(state.timer);
    this.codexRetries.delete(recipient);
  }

  reconcileCodexWakes(): void {
    for (const session of this.codexSessions.listWithUnread()) {
      if (session.unread > 0) this.maybeWake(session.mailbox);
    }
  }

  setPresence(nameRaw: string, online: boolean): void {
    const name = this.normalizeAgent(nameRaw, "agent");
    this.requireConcreteAgentIdentity(name);
    const now = new Date().toISOString();

    if (this.isCanonicalCodexMailbox(name)) {
      const setRegisteredCodexPresence = this.db.transaction(() => {
        this.requireRegisteredCodexMailbox(name);
        this.codexSessions.touchMailbox(name);
        this.db
          .query(`UPDATE agents SET online = ?1, presence_at = ?2 WHERE name = ?3`)
          .run(online ? 1 : 0, now, name);
      });
      setRegisteredCodexPresence();
      return;
    }

    this.db
      .query(
        `INSERT INTO agents(name, first_seen, last_seen, online, presence_at)
         VALUES (?1, ?2, ?2, ?3, ?2)
         ON CONFLICT(name) DO UPDATE SET online = ?3, presence_at = ?2, last_seen = ?2`,
      )
      .run(name, now, online ? 1 : 0);
  }

  private presenceOf(name: string): "online" | "offline" | "unknown" {
    if (this.waiters.has(name)) return "online"; // long-polling right now = strongest proof
    const row = this.db
      .query(`SELECT online, presence_at FROM agents WHERE name = ?1`)
      .get(name) as { online: number; presence_at: string | null } | null;
    if (!row || row.presence_at === null) return "unknown";
    return row.online ? "online" : "offline";
  }

  history(limit: number, beforeId?: number, visiblePatterns?: string[]) {
    const capped = Math.min(Math.max(Math.floor(limit), 1), 500);
    const rawRows = (
      beforeId
        ? this.db
            .query(
              `SELECT id, sender, recipient, content, created_at, sender_role FROM messages
               WHERE id < ?1 ORDER BY id DESC LIMIT ?2`,
            )
            .all(beforeId, capped)
        : this.db
            .query(
              `SELECT id, sender, recipient, content, created_at, sender_role FROM messages
               ORDER BY id DESC LIMIT ?1`,
            )
            .all(capped)
    ) as MessageRow[];
    const rows = visiblePatterns
      ? rawRows.filter((row) =>
          visiblePatterns.some(
            (pattern) =>
              agentMatchesPattern(row.sender, pattern) || agentMatchesPattern(row.recipient, pattern),
          ),
        )
      : rawRows;
    const total = visiblePatterns
      ? (
          this.db
            .query(`SELECT sender, recipient FROM messages`)
            .all() as Array<{ sender: string; recipient: string }>
        ).filter((row) =>
          visiblePatterns.some(
            (pattern) =>
              agentMatchesPattern(row.sender, pattern) || agentMatchesPattern(row.recipient, pattern),
          ),
        ).length
      : (this.db.query(`SELECT COUNT(*) AS n FROM messages`).get() as { n: number }).n;
    return { messages: rows.reverse(), total };
  }

  // visiblePatterns selects the listed agents. wakePatterns selects the wake log, which stays scoped to mailbox access.
  status(fromRaw?: string, visiblePatterns?: string[], wakePatterns: string[] | undefined = visiblePatterns) {
    if (fromRaw) this.touchAgent(this.normalizeAgent(fromRaw, "from"));
    const lead = this.getLead();
    const agents = (
      this.db.query(`SELECT name, first_seen, last_seen FROM agents ORDER BY name`).all() as {
        name: string;
        first_seen: string;
        last_seen: string | null;
      }[]
    )
      .filter(
        (a) =>
          !visiblePatterns || visiblePatterns.some((pattern) => agentMatchesPattern(a.name, pattern)),
      )
      .map((a) => {
        const presence = this.presenceOf(a.name);
        const codexSession = this.codexSessions.getByMailbox(a.name);
        // kill -9 never fires SessionEnd, so long-idle "online" becomes stale
        // an agent inside a long wait is alive, never downgrade it
        const idleSeconds = a.last_seen
          ? Math.floor((Date.now() - Date.parse(a.last_seen)) / 1000)
          : null;
        const waitingNow = (this.waiters.get(a.name)?.length ?? 0) > 0;
        const connected =
          presence === "online" && !waitingNow && idleSeconds !== null && idleSeconds > 1800
            ? "stale (online but idle >30min - possible crash)"
            : presence;
        return {
          ...a,
          ...(codexSession
            ? {
                display_label: codexSession.display_label,
                cwd: codexSession.cwd,
                lifecycle: codexSession.lifecycle,
              }
            : {}),
          role: a.name === lead ? "lead" : "worker",
          connected,
          idle_seconds: idleSeconds,
          waiting_now: waitingNow,
          unread: (
            this.db
              .query(`SELECT COUNT(*) AS n FROM deliveries WHERE recipient = ?1 AND read_at IS NULL`)
              .get(a.name) as { n: number }
          ).n,
        };
      });
    let lastWakes = this.db
      .query(`SELECT recipient, created_at, ok, detail FROM wakes ORDER BY id DESC LIMIT 5`)
      .all() as Array<{ recipient: string; created_at: string; ok: number; detail: string }>;
    if (wakePatterns) {
      lastWakes = lastWakes.filter((wake) =>
        wakePatterns.some((pattern) => agentMatchesPattern(wake.recipient, pattern)),
      );
    }
    return { daemon: "agent-bridge", startedAt: this.startedAt, lead, agents, lastWakes };
  }

  clear(confirm: string) {
    if (confirm !== "wipe") {
      throw new Error('refusing to clear: pass confirm="wipe" to delete all messages');
    }
    const { n } = this.db.query(`SELECT COUNT(*) AS n FROM messages`).get() as { n: number };
    this.db.exec(`DELETE FROM deliveries; DELETE FROM messages;`);
    return { cleared: n };
  }
}
