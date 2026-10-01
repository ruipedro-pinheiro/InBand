import { describe, expect, test } from "bun:test";
import { existsSync, mkdtempSync, rmSync, statSync } from "fs";
import { tmpdir } from "os";
import { join } from "path";
import { openDb } from "../src/db.ts";

describe("database file security", () => {
  test("creates SQLite files readable only by the current OS user", () => {
    const dir = mkdtempSync(join(tmpdir(), "inband-db-"));
    try {
      const path = join(dir, "bridge.db");
      const db = openDb(path);
      db.query(`INSERT INTO agents(name, first_seen, last_seen) VALUES ('claude', 'now', 'now')`).run();
      db.close();

      for (const candidate of [path, `${path}-wal`, `${path}-shm`]) {
        if (!existsSync(candidate)) continue;
        expect(statSync(candidate).mode & 0o777).toBe(0o600);
      }
    } finally {
      rmSync(dir, { recursive: true, force: true });
    }
  });
});
