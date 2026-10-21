import { expect, test } from "bun:test";
import { Effect } from "effect";

import { type V2SessionAdmission, V2SessionDelivery } from "../wakes/session-delivery.js";

function inboxHost() {
  type Input = { text: string; delivery?: "steer" | "queue"; resume?: boolean };
  type Message = { type: "user" | "synthetic"; text: string };
  const inbox: Array<Message & { delivery: "steer" | "queue" }> = [];
  const messages: Message[] = [];
  let active = false;
  let scheduled = false;
  let turns = 0;
  let wakeRequests = 0;
  let reverted = false;

  // OpenCode 2.0.27's Session.synthetic and Session.prompt both durably admit
  // input before waking execution. Admission does not itself run a model step;
  // the runner promotes steering input at the next step boundary.
  const admit = (type: Message["type"], input: Input) =>
    Effect.sync(() => {
      if (type === "user" && reverted) {
        inbox.length = 0;
        reverted = false;
      }
      inbox.push({ type, text: input.text, delivery: input.delivery ?? "steer" });
      if (input.resume !== false && !reverted) {
        wakeRequests++;
        if (!active) scheduled = true;
      }
      return { type };
    });
  const session: V2SessionAdmission = {
    get: () => Effect.sync(() => ({ revert: reverted ? { messageID: "boundary" } : undefined })),
    prompt: (input) => admit("user", input),
    synthetic: (input) => admit("synthetic", input),
  };
  return {
    session,
    messages,
    inbox,
    turns: () => turns,
    wakeRequests: () => wakeRequests,
    step() {
      if (!active && !scheduled) return false;
      if (!active) turns++;
      active = true;
      scheduled = false;
      for (let index = 0; index < inbox.length; ) {
        const input = inbox[index];
        if (input?.delivery !== "steer") {
          index++;
          continue;
        }
        messages.push({ type: input.type, text: input.text });
        inbox.splice(index, 1);
      }
      return true;
    },
    idle() {
      active = false;
    },
    stageRevert() {
      reverted = true;
    },
    commitRevert() {
      // OpenCode's RevertEvent.Committed projector deletes inbox entries
      // enqueued after the revert boundary as well as projected messages.
      inbox.length = 0;
      messages.length = 0;
      reverted = false;
    },
    cancelRevert() {
      reverted = false;
      scheduled = true;
    },
    modelText: () => messages.map((message) => message.text),
  };
}

test("V2 completion wakes an idle turn as synthetic text and survives later turns", async () => {
  const host = inboxHost();
  const delivery = new V2SessionDelivery(host.session);
  const text = "<system-reminder>\n[BACKGROUND BASH COMPLETED] bash-idle\n</system-reminder>";

  expect(
    await Effect.runPromise(delivery.completion({ sessionID: "s", taskIDs: ["bash-idle"], text })),
  ).toBe("delivered");
  expect(host.inbox).toEqual([{ type: "synthetic", text, delivery: "steer" }]);
  expect(host.messages).toEqual([]);
  expect(host.wakeRequests()).toBe(1);
  expect(host.step()).toBe(true);
  expect(host.turns()).toBe(1);
  expect(host.messages).toEqual([{ type: "synthetic", text }]);
  expect(host.modelText()).toEqual([text]);

  host.idle();
  await Effect.runPromise(
    host.session.prompt({ sessionID: "s", text: "next turn", delivery: "steer" }),
  );
  expect(host.step()).toBe(true);
  expect(host.turns()).toBe(2);
  expect(host.modelText()).toEqual([text, "next turn"]);
});

test("V2 completion steers a busy turn at its next step without scheduling a second turn", async () => {
  const host = inboxHost();
  const delivery = new V2SessionDelivery(host.session);
  await Effect.runPromise(host.session.prompt({ sessionID: "s", text: "work", delivery: "steer" }));
  expect(host.step()).toBe(true);

  await Effect.runPromise(
    delivery.completion({ sessionID: "s", taskIDs: ["bash-busy"], text: "done" }),
  );
  expect(host.modelText()).toEqual(["work"]);
  expect(host.step()).toBe(true);
  expect(host.turns()).toBe(1);
  expect(host.messages).toEqual([
    { type: "user", text: "work" },
    { type: "synthetic", text: "done" },
  ]);
  expect(
    await Effect.runPromise(
      delivery.completion({ sessionID: "s", taskIDs: ["bash-busy"], text: "done" }),
    ),
  ).toBe("already-delivered");
  expect(host.wakeRequests()).toBe(2);
});

test("V2 status admission stays parked until a later wake promotes its text", async () => {
  const host = inboxHost();
  const delivery = new V2SessionDelivery(host.session);
  await Effect.runPromise(delivery.status({ sessionID: "s", text: "still running" }));
  expect(host.inbox).toEqual([{ type: "synthetic", text: "still running", delivery: "steer" }]);
  expect(host.wakeRequests()).toBe(0);
  expect(host.step()).toBe(false);
  await Effect.runPromise(
    delivery.completion({ sessionID: "s", taskIDs: ["bash-status"], text: "finished" }),
  );
  expect(host.step()).toBe(true);
  expect(host.modelText()).toEqual(["still running", "finished"]);
});

test("V2 completion stays retryable while a staged revert could discard synthetic input", async () => {
  for (const settlement of ["cancel", "commit"] as const) {
    const host = inboxHost();
    const delivery = new V2SessionDelivery(host.session);
    host.stageRevert();
    await Effect.runPromise(host.session.synthetic({ sessionID: "s", text: "raw pending" }));
    expect(host.step()).toBe(false);
    const wake = { sessionID: "s", taskIDs: ["bash-reverted"], text: "completed" };
    expect((await Effect.runPromiseExit(delivery.completion(wake)))._tag).toBe("Failure");
    expect(host.inbox).toEqual([{ type: "synthetic", text: "raw pending", delivery: "steer" }]);
    if (settlement === "commit") host.commitRevert();
    else host.cancelRevert();
    expect(await Effect.runPromise(delivery.completion(wake))).toBe("delivered");
    expect(host.step()).toBe(true);
    expect(host.modelText().filter((text) => text === "completed")).toHaveLength(1);
    expect(await Effect.runPromise(delivery.completion(wake))).toBe("already-delivered");
  }
});
