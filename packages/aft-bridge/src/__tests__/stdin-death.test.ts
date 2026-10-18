import { expect, test } from "bun:test";
import { spawnSync } from "node:child_process";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { pathToFileURL } from "node:url";

for (const runtime of [process.execPath, "node"]) {
  test(`stdin writes survive child death under ${runtime === "node" ? "Node" : "Bun"}`, () => {
    const dir = mkdtempSync(join(tmpdir(), "aft-stdin-death-"));
    try {
      const child = join(dir, "exit-now.cjs");
      writeFileSync(child, "process.exit(0);\n");
      const node = spawnSync("node", ["-p", "process.execPath"], { encoding: "utf8" });
      expect(node.status).toBe(0);
      const nodeBinary = node.stdout.trim();
      const moduleUrl = pathToFileURL(join(import.meta.dir, "../../dist/bridge.js")).href;
      const runner = join(dir, "host.mjs");
      writeFileSync(
        runner,
        `
import { BinaryBridge } from ${JSON.stringify(moduleUrl)};
const bridge = new BinaryBridge(${JSON.stringify(nodeBinary)}, ${JSON.stringify(dir)}, {
  timeoutMs: 1000, maxRestarts: 0, logger: { log() {}, warn() {}, error() {} },
  childEnv: { NODE_OPTIONS: ${JSON.stringify(`--require ${JSON.stringify(child)}`)} }
});
const writes = Array.from({length: 32}, () => bridge.send("version", { padding: "x".repeat(128 * 1024) }));
await Promise.allSettled(writes);
await bridge.shutdown();
console.log("host survived");
`,
      );
      const result = spawnSync(runtime, [runner], { encoding: "utf8", timeout: 5000 });
      expect(result.error).toBeUndefined();
      expect({ status: result.status, stderr: result.stderr }).toEqual({ status: 0, stderr: "" });
      expect(result.stdout).toContain("host survived");
    } finally {
      rmSync(dir, { recursive: true, force: true });
    }
  });
}
