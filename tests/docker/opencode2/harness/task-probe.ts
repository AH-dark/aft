import { existsSync } from "node:fs";
import { Database, SQLiteError } from "bun:sqlite";

import type { TaskProbe, TaskState } from "./process-observer.js";

interface BashTaskRow {
  task_id: string;
  status: string;
  pgid: number | null;
  metadata: string | null;
}

export class AftTaskProbe implements TaskProbe {
  readonly databasePath: string;
  #lastRows = new Map<string, BashTaskRow>();

  constructor(databasePath: string) {
    this.databasePath = databasePath;
  }

  async states(): Promise<TaskState[]> {
    if (!existsSync(this.databasePath)) return [];
    try {
      return this.#readStates(true);
    } catch (error) {
      if (!(error instanceof SQLiteError)) throw error;
      // Extended SQLite codes retain the primary result in the low byte.
      const code = error.errno & 0xff;
      if (code !== 8 && code !== 14) throw error; // SQLITE_READONLY / SQLITE_CANTOPEN
      // WAL recovery or shared-memory initialization can require write access.
      // Retry only the reads with a writable connection; a second failure escapes.
      return this.#readStates(false);
    }
  }

  #readStates(readonly: boolean): TaskState[] {
    const database = Database.open(this.databasePath, {
      readonly,
      readwrite: !readonly,
      create: false,
      strict: true,
    });
    try {
      // AFT writes this database while the probe reads it. Without a busy
      // timeout a read that lands during one of those writes fails at once
      // with "database is locked" instead of waiting for the write to finish.
      database.exec("PRAGMA busy_timeout = 5000");
      const table = database
        .query<{ name: string }, []>(
          "SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'bash_tasks'",
        )
        .get();
      if (!table) return [];
      const rows = database
        .query<BashTaskRow, []>(
          "SELECT task_id, status, pgid, metadata FROM bash_tasks WHERE harness = 'opencode' ORDER BY started_at",
        )
        .all();
      this.#lastRows = new Map(rows.map((row) => [row.task_id, row]));
      return rows.map((row) => {
        let statusReason: string | undefined;
        if (row.metadata) {
          try {
            const metadata = JSON.parse(row.metadata) as { status_reason?: unknown };
            if (typeof metadata.status_reason === "string") statusReason = metadata.status_reason;
          } catch {}
        }
        return {
          id: row.task_id,
          status: row.status,
          status_reason: statusReason,
          pgid: row.pgid ?? undefined,
        };
      });
    } finally {
      database.close();
    }
  }

  async cancel(id: string): Promise<void> {
    await this.states();
    const row = this.#lastRows.get(id);
    if (!row?.pgid) return;
    try {
      process.kill(-row.pgid, "SIGTERM");
    } catch (error) {
      if ((error as NodeJS.ErrnoException).code !== "ESRCH") throw error;
    }
  }
}
