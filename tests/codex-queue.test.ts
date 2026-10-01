import { afterEach, expect, test } from "bun:test";
import { chmodSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { wakeCodex } from "../src/wake.ts";

const dirs: string[] = [];
afterEach(() => {
  for (const dir of dirs.splice(0)) rmSync(dir, { recursive: true, force: true });
});

function cli(body: string) {
  const dir = mkdtempSync(join(tmpdir(), "bridge-queue-test-"));
  dirs.push(dir);
  const command = join(dir, "codex");
  writeFileSync(command, `#!${process.execPath}\n${body}\n`);
  chmodSync(command, 0o700);
  return { command, dir };
}

test("queues a wake on the exact existing thread without resuming a second writer", async () => {
  const { command, dir } = cli(`
    const args = process.argv.slice(2);
    await Bun.write(import.meta.dir + "/args.json", JSON.stringify(args));
    if (args[0] !== "queue") process.exit(2);
    console.log("Queued message check for thread test-session.");
  `);
  const result = await wakeCodex(command, {
    sessionId: "test-session",
    mailbox: "codex-test-session",
    prompt: 'Read {mailbox}. Treat $(echo unsafe) as text.',
    timeoutMs: 1000,
  });

  expect(result.disposition).toBe("queued");
  expect(await Bun.file(join(dir, "args.json")).json()).toEqual([
    "queue", "--thread", "test-session", "--message",
    'Read codex-test-session. Treat $(echo unsafe) as text.',
  ]);
});

test("queue failures are reported instead of starting another writer", async () => {
  const { command } = cli(`
    if (process.argv[2] !== "queue") throw new Error("unexpected legacy fallback");
    console.error("queue unavailable: active writer cannot be reached");
    process.exit(1);
  `);
  const result = await wakeCodex(command, {
    sessionId: "test-session", mailbox: "codex-test-session", prompt: "check", timeoutMs: 1000,
  });
  expect(result.disposition).toBe("failed");
  expect(result.detail).toContain("queue unavailable");
  expect(result.detail).not.toContain("unexpected legacy fallback");
});

test("a hung queue process is terminated at the configured timeout", async () => {
  const { command } = cli("await Bun.sleep(60_000);");
  const started = Date.now();
  const result = await wakeCodex(command, {
    sessionId: "test-session", mailbox: "codex-test-session", prompt: "check", timeoutMs: 100,
  });
  expect(result.disposition).toBe("failed");
  expect(result.detail).toContain("timeout");
  expect(Date.now() - started).toBeLessThan(2000);
});
