import { describe, expect, test } from "bun:test";
import { realpathSync } from "node:fs";
import { parse } from "node:path";
import { BinaryBridge, compareSemver } from "../bridge.js";
import { canonicalizeProjectRoot, trimProjectRootSeparators } from "../project-identity.js";

const silentLogger = { log() {}, warn() {}, error() {} };
function bridgeWith(options: ConstructorParameters<typeof BinaryBridge>[2] = {}) {
  return new BinaryBridge("/unused/aft", process.cwd(), { logger: silentLogger, ...options });
}

describe("bridge notification ownership", () => {
  test("empty synchronous warnings release the initiating client", async () => {
    const bridge = bridgeWith({ onConfigureWarnings() {} });
    const internals = bridge as any;
    internals.configureWarningClients.set("s", {});
    await internals.deliverConfigureWarnings({ warnings: [] }, { session_id: "s" });
    expect(internals.configureWarningClients.size).toBe(0);
    await bridge.shutdown();
  });

  test("synchronous warnings retain the client until an empty deferred frame", async () => {
    const client = {};
    const bridge = bridgeWith({ onConfigureWarnings() {} });
    const internals = bridge as any;
    internals.configureWarningClients.set("s", client);
    await internals.deliverConfigureWarnings(
      { warnings: [{ tool: "bash", message: "warning" }], warnings_pending: true },
      { session_id: "s" },
    );
    expect(internals.configureWarningClients.get("s")).toBe(client);
    await internals.handleConfigureWarningsFrame({ session_id: "s", warnings: [] });
    expect(internals.configureWarningClients.size).toBe(0);
    await bridge.shutdown();
  });

  test("warning delivery readiness does not await a host notification promise", async () => {
    const bridge = bridgeWith({ onConfigureWarnings: () => new Promise<void>(() => {}) });
    const internals = bridge as any;
    let settled = false;
    void internals
      .deliverConfigureWarnings(
        { warnings: [{ tool: "bash", message: "warning" }] },
        { session_id: "s" },
      )
      .then(() => {
        settled = true;
      });
    for (let i = 0; i < 10; i++) await Promise.resolve();
    expect(settled).toBe(true);
    await bridge.shutdown();
  });

  test("rejected pushed bash callbacks are observed without an unhandled rejection", async () => {
    const warnings: string[] = [];
    const bridge = bridgeWith({
      logger: {
        ...silentLogger,
        warn: (message: string) => {
          warnings.push(message);
        },
      },
      onBashLongRunning: async () => {
        throw new Error("async host failed");
      },
      onBashPatternMatch: async () => {
        throw new Error("pattern host failed");
      },
    });
    try {
      (bridge as any).onStdoutData('{"type":"bash_long_running"}\n{"type":"bash_pattern_match"}\n');
      for (let i = 0; i < 10; i++) await Promise.resolve();
      expect(warnings.join("\n")).toContain("async host failed");
      expect(warnings.join("\n")).toContain("pattern host failed");
    } finally {
      await bridge.shutdown();
    }
  });

  test("pushed bash callback errors are isolated from JSON parsing", async () => {
    const warnings: string[] = [];
    const bridge = bridgeWith({
      logger: {
        ...silentLogger,
        warn: (message: string) => {
          warnings.push(message);
        },
      },
      onBashCompletion: () => {
        throw new Error("host callback failed");
      },
    });
    (bridge as any).onStdoutData('{"type":"bash_completed","task_id":"t"}\n');
    for (let i = 0; i < 10; i++) await Promise.resolve();
    expect(warnings.join("\n")).toContain("host callback failed");
    expect(warnings.join("\n")).not.toContain("Failed to parse stdout");
    await bridge.shutdown();
  });
});

describe("bridge SemVer", () => {
  test("build metadata does not alter precedence", () => {
    expect(compareSemver("1.2.3+build.7", "1.2.3")).toBe(0);
    expect(compareSemver("1.2.3+build.7", "1.2.4")).toBeLessThan(0);
  });
  test("hyphenated prerelease identifiers and ASCII ordering are preserved", () => {
    expect(compareSemver("1.2.3-rc-2", "1.2.3-rc-1")).toBeGreaterThan(0);
    expect(compareSemver("1.2.3-Z", "1.2.3-a")).toBeLessThan(0);
  });
  test("malformed versions cannot bypass a minimum-version gate", () => {
    for (const version of ["1.2.x", "1.2", "01.2.3", "1.2.3-01", "1.2.3+"]) {
      expect(() => compareSemver(version, "1.2.3")).toThrow(/version/i);
    }
  });
});

test("filesystem root remains root rather than the host cwd", () => {
  const root = parse(process.cwd()).root;
  expect(process.cwd()).not.toBe(root);
  expect(canonicalizeProjectRoot(root)).toBe(realpathSync(root));
  expect(canonicalizeProjectRoot(`${root}//`)).toBe(realpathSync(root));
});

test("platform root separators are preserved for drive and UNC roots", () => {
  expect(trimProjectRootSeparators("c:\\\\", "win32")).toBe("c:\\");
  expect(trimProjectRootSeparators("C:/", "win32")).toBe("C:/");
  expect(trimProjectRootSeparators("\\\\server\\share\\\\", "win32")).toBe("\\\\server\\share\\");
  expect(trimProjectRootSeparators("C:\\repo\\\\", "win32")).toBe("C:\\repo");
});

describe("bridge stdout byte limits", () => {
  test("a burst of individually bounded frames is not one oversized frame", async () => {
    let completions = 0;
    const bridge = bridgeWith({
      onBashCompletion: () => {
        completions++;
      },
    });
    const frame = `${JSON.stringify({ type: "bash_completed", task_id: "t", command: "x".repeat(512 * 1024) })}\n`;
    (bridge as any).onStdoutData(frame.repeat(130));
    expect(completions).toBe(130);
    await bridge.shutdown();
  });
  test("an oversized complete multibyte frame is rejected in bytes", async () => {
    let completions = 0;
    const bridge = bridgeWith({
      onBashCompletion: () => {
        completions++;
      },
      maxRestarts: 0,
    });
    (bridge as any).onStdoutData(
      `${JSON.stringify({ type: "bash_completed", task_id: "t", command: "é".repeat(33 * 1024 * 1024) })}\n`,
    );
    expect(completions).toBe(0);
    await bridge.shutdown();
  });
});

test("successful traffic at zero restarts does not allocate a reset timer", async () => {
  const bridge = bridgeWith();
  (bridge as any).scheduleRestartCountReset();
  expect((bridge as any).restartResetTimer).toBeNull();
  await bridge.shutdown();
});

test("shutdown cancels the eager restart timer without leaving scheduled work", async () => {
  const originalSet = globalThis.setTimeout;
  const originalClear = globalThis.clearTimeout;
  const pending = new Set<object>();
  globalThis.setTimeout = (() => {
    const timer = { unref() {} };
    pending.add(timer);
    return timer;
  }) as unknown as typeof setTimeout;
  globalThis.clearTimeout = ((timer: object) => {
    pending.delete(timer);
  }) as unknown as typeof clearTimeout;
  const bridge = bridgeWith({ maxRestarts: 1 });
  try {
    (bridge as any).handleCrash();
    expect(pending.size).toBe(2);
    await bridge.shutdown();
    expect(pending.size).toBe(0);
  } finally {
    await bridge.shutdown();
    globalThis.setTimeout = originalSet;
    globalThis.clearTimeout = originalClear;
  }
});
