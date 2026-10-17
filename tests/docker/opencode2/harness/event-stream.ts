import { fail } from "./errors.js";
import type { ApiControlPlan } from "./types.js";

/**
 * A recording of the host's own event stream for one scenario.
 *
 * The stream is opened as soon as the shared server is up and stays open for
 * the whole scenario, so an event that is created and answered in the same
 * millisecond is still recorded. Polling a list endpoint cannot see one of
 * those: by the time the poll lands the request is gone.
 */
export interface HostEvent {
  type: string;
  data: Record<string, unknown>;
}

function parseEventBlock(block: string): HostEvent | undefined {
  const payload = block
    .split("\n")
    .filter((line) => line.startsWith("data:"))
    .map((line) => line.slice(5).trim())
    .join("");
  if (!payload) return undefined;
  try {
    const parsed = JSON.parse(payload) as { type?: unknown; data?: unknown };
    if (typeof parsed.type !== "string") return undefined;
    const data =
      parsed.data && typeof parsed.data === "object" && !Array.isArray(parsed.data)
        ? (parsed.data as Record<string, unknown>)
        : {};
    return { type: parsed.type, data };
  } catch {
    return undefined;
  }
}

/**
 * The `session.tool.called` event for one call, if the host has sent it.
 *
 * Host releases name the call `id` or `callID` in the event data; both carry
 * the id the model gave the call, which is the scripted call id.
 */
export function findToolCalled(
  events: readonly HostEvent[],
  callId: string,
): HostEvent | undefined {
  return events.find(
    (event) =>
      event.type === "session.tool.called" &&
      (event.data.id === callId || event.data.callID === callId),
  );
}

export class HostEventRecorder {
  readonly events: HostEvent[] = [];
  #controller = new AbortController();
  #reading?: Promise<void>;
  /** Why the stream stopped early, when it did. */
  failure?: string;

  async start(endpoint: string, password: string): Promise<void> {
    const authorization = `Basic ${Buffer.from(`opencode:${password}`).toString("base64")}`;
    const response = await fetch(new URL("/api/event", endpoint), {
      headers: { authorization, accept: "text/event-stream" },
      signal: this.#controller.signal,
    });
    if (!response.ok || !response.body) {
      this.failure = `event stream returned ${response.status}`;
      return;
    }
    const body = response.body;
    this.#reading = (async () => {
      const decoder = new TextDecoder();
      let pending = "";
      try {
        for await (const chunk of body as unknown as AsyncIterable<Uint8Array>) {
          pending += decoder.decode(chunk, { stream: true });
          const blocks = pending.split("\n\n");
          pending = blocks.pop() ?? "";
          for (const block of blocks) {
            const event = parseEventBlock(block);
            if (event) this.events.push(event);
          }
        }
      } catch (error) {
        if (!this.#controller.signal.aborted) {
          this.failure = error instanceof Error ? error.message : String(error);
        }
      }
    })();
  }

  async stop(): Promise<void> {
    this.#controller.abort();
    await this.#reading?.catch(() => undefined);
  }

  ofType(type: string): HostEvent[] {
    return this.events.filter((event) => event.type === type);
  }

  /**
   * Wait for the host to report that it has started executing a tool call.
   *
   * `session.tool.called` is the host's own record that a call's execution
   * began. Before it, an interrupt cancels a step that has not yet run the
   * call: the session simply goes idle as "interrupted" with no error part,
   * so nothing about the tool is exercised. Returns the event, or undefined
   * when the host announced no start for that call within the time allowed.
   */
  async awaitToolCalled(callId: string, timeoutMs: number): Promise<HostEvent | undefined> {
    const deadline = Date.now() + timeoutMs;
    while (true) {
      const started = findToolCalled(this.events, callId);
      if (started) return started;
      if (Date.now() >= deadline) return undefined;
      await Bun.sleep(25);
    }
  }

  /**
   * The id of the session the host created for this scenario.
   *
   * Taken from `session.created` on the stream rather than a list poll: the
   * stream is already open, so the id is known the moment the session exists,
   * with no request per attempt.
   */
  async awaitSessionId(timeoutMs: number): Promise<string | undefined> {
    const deadline = Date.now() + timeoutMs;
    while (Date.now() < deadline) {
      for (const event of this.ofType("session.created")) {
        const id = event.data.sessionID ?? event.data.id;
        if (typeof id === "string" && id.length > 0) return id;
      }
      await Bun.sleep(25);
    }
    return undefined;
  }
}

/**
 * Hold a control until the host has started executing the call it targets.
 *
 * A control is sent once its turn has been served, but serving a turn only
 * hands the host the call: the host still has to open a step and dispatch the
 * tool. An interrupt sent in that window cancels a step that never ran the
 * call, and the session goes idle with no error part at all, so the row
 * measures how fast the control command started rather than what an
 * interrupted tool does. Waiting for the host's own `session.tool.called` for
 * the call makes the control land on a call that is in flight. A host that
 * never reports the start fails the row by name instead of sending the control
 * anyway. What the wait saw is handed to `record` as
 * `control-<id>-tool-readiness.json` either way.
 */
export async function awaitControlToolCall(
  scenarioId: string,
  control: ApiControlPlan,
  hostEvents: HostEventRecorder | undefined,
  record: (name: string, value: unknown) => Promise<void>,
): Promise<void> {
  const gate = control.wait_for_tool_call;
  if (!gate) return;
  if (!hostEvents) {
    fail("host_failed", `${scenarioId}: control ${control.id} needs the host event stream`);
  }
  const waitStartedAt = Date.now();
  const started = await hostEvents.awaitToolCalled(gate.call_id, gate.timeout_ms);
  const readiness = {
    call_id: gate.call_id,
    timeout_ms: gate.timeout_ms,
    waited_ms: Date.now() - waitStartedAt,
    observed: started ?? null,
    observed_types: [...new Set(hostEvents.events.map((event) => event.type))].sort(),
    event_stream_failure: hostEvents.failure ?? null,
  };
  await record(`control-${control.id}-tool-readiness.json`, readiness);
  if (!started) {
    fail(
      "host_failed",
      `${scenarioId}: control ${control.id} waited ${gate.timeout_ms}ms for the host to start call ${gate.call_id}, and no session.tool.called for it arrived; the control was not sent`,
      readiness,
    );
  }
}
