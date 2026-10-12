import { expect, test } from "bun:test";
import { spawn } from "node:child_process";
import { createRpcClient } from "../../helpers/rpc-client";

function spawnRpcFixture(script: string) {
  const child = spawn("node", ["-e", script], { stdio: ["pipe", "pipe", "pipe"] });
  let stdout = "";
  child.stdout.on("data", (chunk) => {
    stdout += String(chunk);
  });
  const closed = new Promise<void>((resolve) => child.once("close", () => resolve()));
  const client = createRpcClient(child);
  return {
    child,
    client,
    closed,
    stdout: () => stdout,
    async cleanup() {
      if (child.exitCode === null && child.signalCode === null) child.kill("SIGKILL");
      await closed;
    },
  };
}

test("RPC teardown retires UI callbacks before buffered notifications drain", async () => {
  const fixture = spawnRpcFixture(`
    const send = (message) => process.stdout.write(JSON.stringify(message) + '\\n');
    process.on('SIGTERM', () => {
      send({ type: 'extension_ui_request', method: 'notify', id: 'late-notify' });
      process.exitCode = 0;
      process.stdin.destroy();
    });
    process.stdin.resume();
    send({ type: 'ready' });
  `);
  let uiCalls = 0;
  const replyErrors: string[] = [];
  fixture.client.onExtensionUIRequest((request) => {
    uiCalls += 1;
    try {
      fixture.client.sendExtensionUIResponse({ id: request.id as string, confirmed: true });
    } catch (error) {
      replyErrors.push(String(error));
    }
  });
  try {
    await fixture.client.waitForEvent((event) => event.type === "ready");
    await fixture.client.close();
    await fixture.closed;
    expect(fixture.stdout()).toContain('"id":"late-notify"');
    expect({ uiCalls, replyErrors }).toEqual({ uiCalls: 0, replyErrors: [] });
    expect(fixture.child.exitCode).toBe(0);
  } finally {
    await fixture.cleanup();
  }
});

test("RPC live UI requests still dispatch and accept responses", async () => {
  const fixture = spawnRpcFixture(`
    const send = (message) => process.stdout.write(JSON.stringify(message) + '\\n');
    let input = '';
    process.stdin.on('data', (chunk) => {
      input += chunk;
      const newline = input.indexOf('\\n');
      if (newline < 0) return;
      const response = JSON.parse(input.slice(0, newline));
      send({ type: 'ui-response-received', id: response.id, confirmed: response.confirmed });
    });
    send({ type: 'extension_ui_request', method: 'confirm', id: 'live-confirm' });
  `);
  let uiCalls = 0;
  fixture.client.onExtensionUIRequest((request) => {
    uiCalls += 1;
    fixture.client.sendExtensionUIResponse({ id: request.id as string, confirmed: true });
  });
  try {
    const response = await fixture.client.waitForEvent(
      (event) => event.type === "ui-response-received",
    );
    expect(uiCalls).toBe(1);
    expect(response.id).toBe("live-confirm");
    expect(response.confirmed).toBe(true);
  } finally {
    await fixture.client.close();
    await fixture.cleanup();
  }
});

test("RPC unexpected child exit still rejects pending work with its exit cause", async () => {
  const fixture = spawnRpcFixture(`
    process.stdin.once('data', () => process.exit(7));
    process.stdout.write(JSON.stringify({ type: 'ready' }) + '\\n');
  `);
  try {
    await fixture.client.waitForEvent((event) => event.type === "ready");
    await expect(fixture.client.sendCommand({ type: "exit-without-response" })).rejects.toThrow(
      "Pi RPC exited before completing pending work (code=7, signal=null)",
    );
  } finally {
    await fixture.client.close();
    await fixture.cleanup();
  }
});
