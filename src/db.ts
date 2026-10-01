import { Database } from "bun:sqlite";
import { chmodSync, closeSync, existsSync, mkdirSync, openSync } from "fs";
import { dirname } from "path";

export interface MessageRow {
  id: number;
  sender: string;
  recipient: string;
  content: string;
  created_at: string;
  /** Role of the sender when the message was sent. Null for messages sent before roles existed. */
  sender_role: "lead" | "worker" | null;
}

export function openDb(path: string): Database {
  mkdirSync(dirname(path), { recursive: true });
  if (path !== ":memory:") {
    closeSync(openSync(path, "a", 0o600));
    chmodSync(path, 0o600);
  }
  const db = new Database(path, { create: true });
  db.exec("PRAGMA journal_mode = WAL;");
  db.exec("PRAGMA foreign_keys = ON;");
  db.exec(`
    CREATE TABLE IF NOT EXISTS messages (
      id INTEGER PRIMARY KEY AUTOINCREMENT,
      sender TEXT NOT NULL,
      recipient TEXT NOT NULL,
      content TEXT NOT NULL,
      created_at TEXT NOT NULL
    );
    CREATE TABLE IF NOT EXISTS deliveries (
      message_id INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
      recipient TEXT NOT NULL,
      read_at TEXT,
      PRIMARY KEY (message_id, recipient)
    );
    CREATE INDEX IF NOT EXISTS idx_deliveries_unread
      ON deliveries(recipient) WHERE read_at IS NULL;
    CREATE TABLE IF NOT EXISTS agents (
      name TEXT PRIMARY KEY,
      first_seen TEXT NOT NULL,
      last_seen TEXT,
      online INTEGER NOT NULL DEFAULT 0,
      presence_at TEXT,
      wake_info TEXT
    );
    CREATE TABLE IF NOT EXISTS wakes (
      id INTEGER PRIMARY KEY AUTOINCREMENT,
      recipient TEXT NOT NULL,
      created_at TEXT NOT NULL,
      ok INTEGER NOT NULL,
      detail TEXT
    );
    CREATE TABLE IF NOT EXISTS codex_sessions (
      mailbox TEXT PRIMARY KEY,
      family TEXT NOT NULL CHECK (family = 'codex'),
      session_id TEXT NOT NULL UNIQUE,
      display_label TEXT NOT NULL UNIQUE,
      cwd TEXT NOT NULL,
      lifecycle TEXT NOT NULL,
      registered_at TEXT NOT NULL,
      last_seen TEXT NOT NULL,
      FOREIGN KEY (mailbox) REFERENCES agents(name) ON DELETE CASCADE
    );
    CREATE INDEX IF NOT EXISTS idx_codex_sessions_recent
      ON codex_sessions(family, last_seen DESC);
    CREATE TABLE IF NOT EXISTS settings (
      key TEXT PRIMARY KEY,
      value TEXT NOT NULL,
      updated_at TEXT NOT NULL
    );
  `);
  const messageColumns = db.query(`PRAGMA table_info(messages)`).all() as { name: string }[];
  if (!messageColumns.some((column) => column.name === "sender_role")) {
    db.exec(`ALTER TABLE messages ADD COLUMN sender_role TEXT`);
  }
  if (path !== ":memory:") {
    for (const candidate of [path, `${path}-wal`, `${path}-shm`]) {
      if (existsSync(candidate)) chmodSync(candidate, 0o600);
    }
  }
  return db;
}
