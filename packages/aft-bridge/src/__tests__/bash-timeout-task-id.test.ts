/// <reference path="../bun-test.d.ts" />

/**
 * A standalone `bash` call can outlast the bridge's per-call deadline while
 * AFT still starts the command and answers later with its task id (issue
 * #577). The error the agent sees must name that task id when the late reply
 * arrives, admit that the outcome is unknown when no reply comes, and never
 * claim that no task exists.
 */

import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { BinaryBridge, isBridgeTransportTimeout } from "../bridge.js";
import { adaptToolError, BASH_TRANSPORT_DISPOSITION } from "../error-contract.js";
import { cachedExecutable } from "./test-utils/cached-executable.js";

let workDir: string;

beforeEach(() => {
  workDir = mkdtempSync(join(tmpdir(), "aft-bash-timeout-task-id-"));
});

afterEach(() => {
  rmSync(workDir, { recursive: true, force: true });
});

/**
 * Fake engine. `bash` requests choose their behaviour by shell command text:
 * "late" starts a task and answers 300ms later, "silent" never answers,
 * "refused" answers late with an error and no task, and anything else answers
 * at once. `bash_status` reports any task the fake has started.
 */
const FAKE_ENGINE = `#!/usr/bin/env node
process.stdin.setEncoding("utf8");
let buffer = "";
const tasks = new Map();
let next = 1;
function write(frame) {
  process.stdout.write(JSON.stringify(frame) + "\\n");
}
process.stdin.on("data", (chunk) => {
  buffer += chunk;
  let newline;
  while ((newline = buffer.indexOf("\\n")) !== -1) {
    const req = JSON.parse(buffer.slice(0, newline));
    buffer = buffer.slice(newline + 1);
    const params = req.params || {};
    if (req.command === "bash") {
      const shell = params.command;
      if (shell === "silent") continue;
      if (shell === "refused") {
        setTimeout(() => write({ id: req.id, success: false, code: "spawn_failed", message: "sandbox refused the command" }), 300);
        continue;
      }
      const taskId = "bash-fake" + next++;
      tasks.set(taskId, shell === "late" ? "running" : "completed");
      if (shell === "late") {
        setTimeout(() => write({ id: req.id, success: true, task_id: taskId, status: "running", output: "" }), 300);
      } else {
        write({ id: req.id, success: true, task_id: taskId, status: "completed", output: "fast output" });
      }
    } else if (req.command === "bash_status") {
      const status = tasks.get(req.task_id);
      write(status
        ? { id: req.id, success: true, task_id: req.task_id, status }
        : { id: req.id, success: false, code: "task_not_found", message: "no such task" });
    } else {
      write({ id: req.id, success: true, warnings: [], version: "99.0.0" });
    }
  }
});
`;

function bridgeFor(graceMs: number): BinaryBridge {
  return new BinaryBridge(cachedExecutable(FAKE_ENGINE), workDir, {
    timeoutMs: 5_000,
    maxRestarts: 0,
    lateShellReplyGraceMs: graceMs,
  });
}

async function timedOutBash(bridge: BinaryBridge, shell: string): Promise<Error> {
  const outcome = await bridge
    .send(
      "bash",
      { command: shell, session_id: "s1" },
      { transportTimeoutMs: 60, keepBridgeOnTimeout: true },
    )
    .then(
      () => null,
      (error: unknown) => error,
    );
  if (!(outcome instanceof Error)) throw new Error(`bash ${shell} did not time out`);
  return adaptToolError("bash", outcome) as Error;
}

describe("standalone bash call that outlasts the bridge deadline", () => {
  test("a reply after the deadline puts the task id in the error", async () => {
    const bridge = bridgeFor(5_000);
    try {
      await bridge.send("configure", { project_root: workDir }, { timeoutMs: 5_000 });
      const error = await timedOutBash(bridge, "late");

      // The engine really does hold a running task for the timed-out call.
      const status = await bridge.send("bash_status", { task_id: "bash-fake1" });
      expect(status).toMatchObject({ success: true, status: "running" });

      expect(isBridgeTransportTimeout(error)).toBe(true);
      expect(error.message).toContain('Request "bash"');
      expect(error.message).toContain("timed out after 60ms");
      expect(error.message).toContain("task bash-fake1");
      expect(error.message).toContain("Use bash_status or bash_watch with task ID bash-fake1");
      expect(error.message).not.toContain(BASH_TRANSPORT_DISPOSITION);
      expect(error.message).not.toContain("no task");
    } finally {
      await bridge.shutdown();
    }
  });

  test("a reply that never comes yields the honest may-not-have-started text", async () => {
    const bridge = bridgeFor(100);
    try {
      await bridge.send("configure", { project_root: workDir }, { timeoutMs: 5_000 });
      const error = await timedOutBash(bridge, "silent");

      expect(isBridgeTransportTimeout(error)).toBe(true);
      expect(error.message).toContain(
        "The command may not have started; check bash_status before rerunning.",
      );
      expect(error.message).not.toContain(BASH_TRANSPORT_DISPOSITION);
      expect(error.message).not.toContain("no task");
      expect(error.message).not.toContain("no background task");
    } finally {
      await bridge.shutdown();
    }
  });

  test("a late error reply without a task id reports AFT's own answer", async () => {
    const bridge = bridgeFor(5_000);
    try {
      await bridge.send("configure", { project_root: workDir }, { timeoutMs: 5_000 });
      const error = await timedOutBash(bridge, "refused");

      expect(error.message).toContain("spawn_failed: sandbox refused the command");
      expect(error.message).toContain("no task ID was assigned");
      expect(error.message).not.toContain(BASH_TRANSPORT_DISPOSITION);
    } finally {
      await bridge.shutdown();
    }
  });

  test("a fast bash call is unchanged", async () => {
    const bridge = bridgeFor(5_000);
    try {
      const response = await bridge.send(
        "bash",
        { command: "echo fast", session_id: "s1" },
        { transportTimeoutMs: 2_000, keepBridgeOnTimeout: true },
      );
      expect(response).toMatchObject({
        success: true,
        task_id: "bash-fake1",
        status: "completed",
        output: "fast output",
      });
    } finally {
      await bridge.shutdown();
    }
  });
});
