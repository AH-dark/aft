import { expect, test } from "bun:test";
import { spawn, spawnSync } from "node:child_process";
import { mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { join, resolve } from "node:path";
import { OpenCode } from "@opencode/client";
import { Effect } from "effect";

import { V2SessionDelivery } from "../../wakes/session-delivery.js";

const hostBinary = process.env.AFT_V2_HOST_BINARY;

async function until(predicate: () => Promise<boolean>): Promise<void> {
  const deadline = Date.now() + 30000;
  while (Date.now() < deadline) {
    if (await predicate()) return;
    await Bun.sleep(50);
  }
  throw new Error("V2 host did not reach the expected state within 30 seconds");
}

test.skipIf(!hostBinary)(
  "real V2 completion wakes idle and steers busy without creating user messages",
  async () => {
    const tempParent = resolve(import.meta.dir, "../../../tmp");
    await mkdir(tempParent, { recursive: true });
    const root = await mkdtemp(join(tempParent, "synthetic-wake-"));
    const project = join(root, "project");
    const config = join(root, "config", "opencode");
    const paths = ["home", "config", "data", "state", "cache", "temp"].map((name) =>
      join(root, name),
    );
    await Promise.all([...paths, project, config].map((path) => mkdir(path, { recursive: true })));
    const git = spawnSync("git", ["init", "-q"], { cwd: project, encoding: "utf8" });
    if (git.status !== 0) throw new Error(`could not initialize isolated project: ${git.stderr}`);
    type ModelRequest = { messages: Array<{ role: string; content: unknown }> };
    const requests: ModelRequest[] = [];
    let releaseStep = () => {};
    const heldStep = new Promise<void>((done) => {
      releaseStep = done;
    });
    const model = Bun.serve({
      hostname: "127.0.0.1",
      port: 0,
      async fetch(request) {
        const body = (await request.json()) as ModelRequest;
        requests.push(body);
        if (body.messages.at(-1)?.content === "Hold this step.") await heldStep;
        const chunk = (delta: Record<string, unknown>, finishReason: string | null) =>
          `data: ${JSON.stringify({
            id: "wake-response",
            object: "chat.completion.chunk",
            created: 1,
            model: "wake-model",
            choices: [{ index: 0, delta, finish_reason: finishReason }],
          })}\n\n`;
        return new Response(
          chunk({ role: "assistant", content: "Completion received." }, null) +
            chunk({}, "stop") +
            "data: [DONE]\n\n",
          { headers: { "content-type": "text/event-stream" } },
        );
      },
    });
    // An explicit service port plus throwaway HOME/XDG roots prevents the CLI
    // from discovering or modifying the operator's managed OpenCode service.
    const portProbe = Bun.serve({ hostname: "127.0.0.1", port: 0, fetch: () => new Response("") });
    const port = portProbe.port;
    portProbe.stop(true);
    await writeFile(join(config, "service.json"), JSON.stringify({ hostname: "127.0.0.1", port }));
    await writeFile(
      join(config, "opencode.json"),
      JSON.stringify({
        model: "wake-test/wake-model",
        small_model: "wake-test/wake-model",
        plugin: [],
        provider: {
          "wake-test": {
            npm: "@ai-sdk/openai-compatible",
            options: { baseURL: `${model.url}v1`, apiKey: "test-only" },
            models: {
              "wake-model": { name: "Wake model", limit: { context: 128000, output: 4096 } },
            },
          },
        },
      }),
    );
    let logs = "";
    const child = spawn(hostBinary as string, ["serve", "--service", "--print-logs"], {
      cwd: project,
      env: {
        PATH: process.env.PATH,
        HOME: paths[0],
        OPENCODE_TEST_HOME: paths[0],
        XDG_CONFIG_HOME: paths[1],
        XDG_DATA_HOME: paths[2],
        XDG_STATE_HOME: paths[3],
        XDG_CACHE_HOME: paths[4],
        TMPDIR: paths[5],
        NODE_ENV: "test",
        OPENCODE_DISABLE_DEFAULT_PLUGINS: "true",
        OPENCODE_DB: "synthetic-wake.db",
        OPENCODE_DISABLE_MODELS_FETCH: "true",
      },
      stdio: ["ignore", "pipe", "pipe"],
    });
    child.stdout?.on("data", (chunk) => {
      logs += String(chunk);
    });
    child.stderr?.on("data", (chunk) => {
      logs += String(chunk);
    });
    try {
      let password: string | undefined;
      await until(async () => {
        if (child.exitCode !== null) throw new Error(`V2 host exited: ${logs}`);
        const state = await readFile(join(root, "state", "opencode", "service.json"), "utf8").catch(
          () => undefined,
        );
        if (!state) return false;
        password = JSON.parse(state).password;
        return true;
      });
      const headers: Record<string, string> =
        password === undefined
          ? {}
          : { authorization: `Basic ${Buffer.from(`opencode:${password}`).toString("base64")}` };
      const client = OpenCode.make({ baseUrl: `http://127.0.0.1:${port}`, headers });
      await until(async () => {
        if (child.exitCode !== null) throw new Error(`V2 host exited: ${logs}`);
        return fetch(`http://127.0.0.1:${port}/api/info`, { headers })
          .then((response) => response.ok)
          .catch(() => false);
      });
      const info = (await fetch(`http://127.0.0.1:${port}/api/info`, { headers }).then((response) =>
        response.json(),
      )) as { version: string };
      console.log(`[v2-synthetic-host] OpenCode ${info.version}`);
      await client.location.get({ location: { directory: project } });
      const session = await client.session.create({
        title: "Synthetic wake",
        location: { directory: project },
      });
      const delivery = new V2SessionDelivery({
        get: (input) => Effect.tryPromise(() => client.session.get(input)),
        prompt: (input) =>
          Effect.tryPromise(() =>
            client.session.prompt({
              ...input,
              metadata: input.metadata as Parameters<typeof client.session.prompt>[0]["metadata"],
            }),
          ),
        synthetic: (input) =>
          Effect.tryPromise(() =>
            client.session.synthetic({
              ...input,
              metadata: input.metadata as Parameters<
                typeof client.session.synthetic
              >[0]["metadata"],
            }),
          ),
      });
      const text =
        "<system-reminder>\n[BACKGROUND BASH COMPLETED]\n- task bash-real (exit 0): done\n</system-reminder>";
      expect(
        await Effect.runPromise(
          delivery.completion({
            sessionID: session.id,
            text,
            taskIDs: ["bash-real"],
            metadata: { kind: "bash_completion", status: "completed" },
          }),
        ),
      ).toBe("delivered");
      await until(async () => {
        const messages = await client.session.context({ sessionID: session.id });
        return messages.some((message) => message.type === "assistant");
      });
      await client.session.wait({ sessionID: session.id });
      const messages = await client.session.context({ sessionID: session.id });
      const wake = messages.find(
        (message) => message.type === "synthetic" && message.text === text,
      );
      expect(wake).toMatchObject({
        type: "synthetic",
        text,
        description: "Background bash completed",
        metadata: {
          source: "aft",
          kind: "bash_completion",
          task_ids: ["bash-real"],
          status: "completed",
        },
      });
      expect(messages.filter((message) => message.type === "user")).toEqual([]);
      expect(
        requests.some((request) =>
          request.messages.some((message) => message.role === "user" && message.content === text),
        ),
      ).toBe(true);
      if (!wake) throw new Error("the completion was not projected as a synthetic message");
      expect(
        await client.session.message.get({ sessionID: session.id, messageID: wake.id }),
      ).toMatchObject({ type: "synthetic", text });
      const readback = await client.message.list({ sessionID: session.id });
      expect(
        readback.data.some((message) => message.type === "synthetic" && message.text === text),
      ).toBe(true);
      await client.session.prompt({ sessionID: session.id, text: "Continue." });
      await client.session.wait({ sessionID: session.id });
      expect(requests.at(-1)?.messages.some((message) => message.content === text)).toBe(true);

      await client.session.prompt({ sessionID: session.id, text: "Hold this step." });
      await until(async () => requests.at(-1)?.messages.at(-1)?.content === "Hold this step.");
      const busyText = "[BACKGROUND BASH COMPLETED] bash-busy-real: done";
      await Effect.runPromise(
        delivery.completion({ sessionID: session.id, taskIDs: ["bash-busy-real"], text: busyText }),
      );
      const duringStep = await client.session.context({ sessionID: session.id });
      expect(
        duringStep.some((message) => message.type === "synthetic" && message.text === busyText),
      ).toBe(false);
      releaseStep();
      await client.session.wait({ sessionID: session.id });
      const afterStep = await client.session.context({ sessionID: session.id });
      expect(
        afterStep.some((message) => message.type === "synthetic" && message.text === busyText),
      ).toBe(true);
      expect(requests.at(-1)?.messages.some((message) => message.content === busyText)).toBe(true);
      expect(requests.at(-1)?.messages.some((message) => message.content === text)).toBe(true);

      for (const settlement of ["clear", "commit"] as const) {
        const reverted = await client.session.create({
          title: "Reverted wake",
          location: { directory: project },
        });
        const seed = await client.session.prompt({ sessionID: reverted.id, text: "Seed turn." });
        await client.session.wait({ sessionID: reverted.id });
        await client.session.revert.stage({
          sessionID: reverted.id,
          messageID: seed.id,
          files: false,
        });
        const revertedText = `[BACKGROUND BASH COMPLETED] bash-revert-${settlement}: done`;
        const rawText = `raw synthetic during revert: ${settlement}`;
        await client.session.synthetic({
          sessionID: reverted.id,
          text: rawText,
          delivery: "steer",
        });
        expect((await client.session.get({ sessionID: reverted.id })).revert).toBeDefined();
        expect(
          (await client.session.inbox.list({ sessionID: reverted.id })).some(
            (item) => item.type === "synthetic" && item.payload.text === rawText,
          ),
        ).toBe(true);
        expect(
          requests.some((request) =>
            request.messages.some((message) => message.content === rawText),
          ),
        ).toBe(false);
        const revertedWake = {
          sessionID: reverted.id,
          taskIDs: [`bash-revert-${settlement}`],
          text: revertedText,
        };
        await expect(Effect.runPromise(delivery.completion(revertedWake))).rejects.toThrow(
          "staged revert",
        );
        await client.session.revert[settlement]({ sessionID: reverted.id });
        await client.session.wait({ sessionID: reverted.id });
        if (settlement === "commit") {
          // The host deletes synthetic inbox entries after the committed
          // boundary. AFT must withhold admission until the revert settles.
          expect(await client.session.inbox.list({ sessionID: reverted.id })).toEqual([]);
          expect(
            requests.some((request) =>
              request.messages.some((message) => message.content === rawText),
            ),
          ).toBe(false);
        } else {
          const cancelled = await client.session.context({ sessionID: reverted.id });
          expect(
            cancelled.some((message) => message.type === "synthetic" && message.text === rawText),
          ).toBe(true);
        }
        expect(await Effect.runPromise(delivery.completion(revertedWake))).toBe("delivered");
        await client.session.wait({ sessionID: reverted.id });
        const settled = await client.session.context({ sessionID: reverted.id });
        expect(
          settled.filter(
            (message) => message.type === "synthetic" && message.text === revertedText,
          ),
        ).toHaveLength(1);
        expect(
          requests.some((request) =>
            request.messages.some((message) => message.content === revertedText),
          ),
        ).toBe(true);
      }
    } catch (error) {
      throw new Error(`${String(error)}\nV2 host output:\n${logs}`, { cause: error });
    } finally {
      releaseStep();
      child.kill("SIGTERM");
      await new Promise<void>((done) => {
        if (child.exitCode !== null) return done();
        child.once("exit", () => done());
        setTimeout(() => child.kill("SIGKILL"), 2000).unref();
      });
      model.stop(true);
      await rm(root, { recursive: true, force: true });
    }
  },
  90000,
);
