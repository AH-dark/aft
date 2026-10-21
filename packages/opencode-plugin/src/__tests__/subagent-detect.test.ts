/// <reference path="../bun-test.d.ts" />
import { afterEach, describe, expect, test } from "bun:test";
import { Effect } from "effect";
import {
  _resetSubagentCacheForTest,
  _setSubagentClockForTest,
  FAILED_LOOKUP_RETRY_MS,
  resolveIsSubagent,
  resolveSubagentStatus,
} from "../shared/subagent-detect.js";

let timeoutFixtureId = 0;

// Each timeout fixture owns its cache and clock, including when stress cases
// overlap. A gated host cannot finish merely because the runner was slow.
async function assertLookupReturnsBeforeHost(kind: "v1" | "v2") {
  const fixture = await import(
    `../shared/subagent-detect.js?timeout-fixture=${timeoutFixtureId++}`
  );
  fixture._setSubagentClockForTest(() => 1_000_000);
  let calls = 0;
  let completed = false;
  let release: (() => void) | undefined;
  const hostLookup = async () => {
    calls += 1;
    await new Promise<void>((resolve) => {
      release = resolve;
    });
    completed = true;
    return { id: "ses_timeout", parentID: "ses_parent" };
  };
  const client =
    kind === "v2"
      ? { location: { directory: "/cwd" }, session: { get: () => Effect.promise(hostLookup) } }
      : { session: { get: async () => ({ data: await hostLookup() }) } };
  try {
    expect(await fixture.resolveIsSubagent(client, "ses_timeout", "/cwd")).toBe(false);
    expect(calls).toBe(1);
    expect(completed).toBe(false);
    // A timeout caches a provisional answer rather than starting another
    // unresolved host lookup. Freeze only the cache clock, not the timer.
    expect(await fixture.resolveIsSubagent(client, "ses_timeout", "/cwd")).toBe(false);
    expect(calls).toBe(1);
  } finally {
    release?.();
  }
}

afterEach(() => {
  _resetSubagentCacheForTest();
});

describe("subagent-detect", () => {
  test("returns false when sessionId is empty", async () => {
    const result = await resolveIsSubagent({}, "", "/cwd");
    expect(result).toBe(false);
  });

  test("returns false when sessionId is undefined", async () => {
    const result = await resolveIsSubagent({}, undefined, "/cwd");
    expect(result).toBe(false);
  });

  test("returns false when client lacks session.get", async () => {
    const result = await resolveIsSubagent({}, "ses_foo", "/cwd");
    expect(result).toBe(false);
  });

  test("returns true when SDK returns non-empty parentID", async () => {
    const client = {
      session: {
        get: async (_input: { path: { id: string }; query?: { directory?: string } }) => ({
          data: { id: "ses_child", parentID: "ses_parent" },
        }),
      },
    };
    const result = await resolveIsSubagent(client, "ses_child", "/cwd");
    expect(result).toBe(true);
  });

  test("calls SDK with path: { id } shape (NOT flat sessionID) and omits directory query", async () => {
    // Regression: the SDK schema is `{ path: { id }, query?: { directory } }`.
    // Passing a flat `{ sessionID, directory }` caused the SDK to receive
    // `id = undefined` and return a different session whose parentID was
    // undefined — silently breaking the subagent gate.
    let lastInput: unknown;
    const client = {
      session: {
        get: async (input: unknown) => {
          lastInput = input;
          return { data: { id: "ses_child", parentID: "ses_parent" } };
        },
      },
    };
    await resolveIsSubagent(client, "ses_child", "/some/cwd");
    expect(lastInput).toEqual({ path: { id: "ses_child" } });
    // Specifically: no `directory` should leak through. Looking up a session
    // by ID is an identity query, not a directory-scoped one.
    expect((lastInput as { query?: unknown }).query).toBeUndefined();
    expect((lastInput as { sessionID?: unknown }).sessionID).toBeUndefined();
    expect((lastInput as { directory?: unknown }).directory).toBeUndefined();
  });

  test("returns false when SDK returns empty parentID", async () => {
    const client = {
      session: {
        get: async (_input: { path: { id: string } }) => ({
          data: { id: "ses_root", parentID: "" },
        }),
      },
    };
    const result = await resolveIsSubagent(client, "ses_root", "/cwd");
    expect(result).toBe(false);
  });

  test("returns false when SDK returns missing parentID", async () => {
    const client = {
      session: {
        get: async (_input: { path: { id: string } }) => ({
          data: { id: "ses_root" }, // no parentID at all
        }),
      },
    };
    const result = await resolveIsSubagent(client, "ses_root", "/cwd");
    expect(result).toBe(false);
  });

  test("handles SDK response shape without `data` wrapper", async () => {
    // ThrowOnError: true variant returns the Session directly, not wrapped.
    const client = {
      session: {
        get: async (_input: { path: { id: string } }) => ({
          id: "ses_child",
          parentID: "ses_parent",
        }),
      },
    };
    const result = await resolveIsSubagent(client, "ses_child", "/cwd");
    expect(result).toBe(true);
  });

  test("caches result so second call does not hit SDK", async () => {
    let calls = 0;
    const client = {
      session: {
        get: async (_input: { path: { id: string } }) => {
          calls += 1;
          return { data: { id: "ses_x", parentID: "ses_parent" } };
        },
      },
    };
    const first = await resolveIsSubagent(client, "ses_x", "/cwd");
    const second = await resolveIsSubagent(client, "ses_x", "/cwd");
    expect(first).toBe(true);
    expect(second).toBe(true);
    expect(calls).toBe(1);
  });

  // Failed lookups stay unknown for the retry window, then ask the host again.
  test("after an SDK error, calls within the retry window make no lookup; a later call retries", async () => {
    let clock = 1_000_000;
    _setSubagentClockForTest(() => clock);
    let calls = 0;
    let shouldThrow = true;
    const client = {
      session: {
        get: async (_input: { path: { id: string } }) => {
          calls += 1;
          if (shouldThrow) throw new Error("transient SDK failure");
          return { data: { id: "ses_y", parentID: "ses_parent" } };
        },
      },
    };
    const first = await resolveIsSubagent(client, "ses_y", "/cwd");
    expect(first).toBe(false); // error path defaults to false
    expect(calls).toBe(1);

    shouldThrow = false;
    clock += FAILED_LOOKUP_RETRY_MS - 1;
    expect(await resolveIsSubagent(client, "ses_y", "/cwd")).toBe(false);
    expect(calls).toBe(1);

    clock += 1;
    expect(await resolveIsSubagent(client, "ses_y", "/cwd")).toBe(true);
    expect(calls).toBe(2);
    // A real answer is kept for good.
    clock += FAILED_LOOKUP_RETRY_MS * 10;
    expect(await resolveIsSubagent(client, "ses_y", "/cwd")).toBe(true);
    expect(calls).toBe(2);
  });

  test("preserves `this` binding when calling SDK session.get (regression: this._client)", async () => {
    // Mirrors the real OpenCode SDK shape where Session.get is a class
    // method that depends on `this._client`. Extracting the function
    // reference and calling it without binding crashes with
    // "undefined is not an object (evaluating 'this._client')".
    class FakeSessionApi {
      private readonly _client = { ok: true };
      async get(input: { path: { id: string } }) {
        if (!this._client?.ok)
          throw new Error("undefined is not an object (evaluating 'this._client')");
        return { data: { id: input.path.id, parentID: "ses_parent" } };
      }
    }
    const client = { session: new FakeSessionApi() };
    const result = await resolveIsSubagent(client, "ses_bind", "/cwd");
    expect(result).toBe(true);
  });

  test("returns false when SDK call returns undefined", async () => {
    const client = {
      session: {
        get: async () => undefined,
      },
    };
    const result = await resolveIsSubagent(client, "ses_nil", "/cwd");
    expect(result).toBe(false);
  });

  describe("OpenCode 2 plugin context", () => {
    // The OpenCode 2 runtime hands tools the plugin context itself as
    // `client`. That context has no SDK `client`; it has a `location` and a
    // `session` domain whose `get` takes `{ sessionID }` and returns an Effect
    // resolving to the bare session record. This double carries only those
    // capabilities, so a detector that still speaks the V1 SDK shape cannot
    // pass by accident.
    function v2Context(records: Record<string, { id: string; parentID?: string }>) {
      const inputs: unknown[] = [];
      const context = {
        location: { directory: "/cwd" },
        session: {
          get: (input: { sessionID?: string }) => {
            inputs.push(input);
            const record = input?.sessionID ? records[input.sessionID] : undefined;
            return record
              ? Effect.succeed(record)
              : Effect.fail(new Error(`session ${JSON.stringify(input?.sessionID)} not found`));
          },
        },
      };
      return { context, inputs };
    }

    test("classifies a child session as a subagent", async () => {
      const { context, inputs } = v2Context({
        ses_child: { id: "ses_child", parentID: "ses_parent" },
      });
      expect(await resolveIsSubagent(context, "ses_child", "/cwd")).toBe(true);
      expect(inputs).toEqual([{ sessionID: "ses_child" }]);
    });

    test("classifies a root session as primary", async () => {
      const { context } = v2Context({ ses_root: { id: "ses_root" } });
      expect(await resolveIsSubagent(context, "ses_root", "/cwd")).toBe(false);
    });

    test("a failed V2 lookup stays unknown until retry, never a cached primary", async () => {
      let clock = 1_000_000;
      _setSubagentClockForTest(() => clock);
      const records: Record<string, { id: string; parentID?: string }> = {};
      const { context, inputs } = v2Context(records);
      expect(await resolveSubagentStatus(context, "ses_late", "/cwd")).toBe("unknown");
      records.ses_late = { id: "ses_late", parentID: "ses_parent" };
      expect(await resolveSubagentStatus(context, "ses_late", "/cwd")).toBe("unknown");
      expect(inputs).toHaveLength(1);
      clock += FAILED_LOOKUP_RETRY_MS;
      expect(await resolveIsSubagent(context, "ses_late", "/cwd")).toBe(true);
      expect(inputs).toHaveLength(2);
    });

    test("an OpenCode 2 lookup that never answers is abandoned after the timeout", async () => {
      await assertLookupReturnsBeforeHost("v2");
    }, 30_000);
  });

  test("concurrent first calls share one host lookup", async () => {
    let calls = 0;
    let release: (() => void) | undefined;
    const client = {
      session: {
        get: async (_input: { path: { id: string } }) => {
          calls += 1;
          await new Promise<void>((resolve) => {
            release = resolve;
          });
          return { data: { id: "ses_burst", parentID: "ses_parent" } };
        },
      },
    };
    const results = Promise.all(
      Array.from({ length: 5 }, () => resolveIsSubagent(client, "ses_burst", "/cwd")),
    );
    await Promise.resolve();
    release?.();
    expect(await results).toEqual([true, true, true, true, true]);
    expect(calls).toBe(1);
  });

  test("a host lookup that never answers lets the call proceed before the host completes", async () => {
    await assertLookupReturnsBeforeHost("v1");
  }, 30_000);

  test("caches negative result (primary session) so repeat calls are O(1)", async () => {
    let calls = 0;
    const client = {
      session: {
        get: async (_input: { path: { id: string } }) => {
          calls += 1;
          return { data: { id: "ses_primary" } }; // no parentID
        },
      },
    };
    await resolveIsSubagent(client, "ses_primary", "/cwd");
    await resolveIsSubagent(client, "ses_primary", "/cwd");
    await resolveIsSubagent(client, "ses_primary", "/cwd");
    expect(calls).toBe(1);
  });
});

test("V2 timed-out role lookup interrupts its fiber and retries the unknown answer", async () => {
  _resetSubagentCacheForTest();
  let clock = 0;
  _setSubagentClockForTest(() => clock);
  let interrupted = false;
  let calls = 0;
  const host = {
    location: { directory: process.cwd() },
    session: {
      get: () => {
        calls += 1;
        return calls === 1
          ? Effect.never.pipe(
              Effect.ensuring(
                Effect.sync(() => {
                  interrupted = true;
                }),
              ),
            )
          : Effect.succeed({ parentID: "parent" });
      },
    },
  };
  expect(await resolveIsSubagent(host, "timeout-child")).toBe(false);
  await Bun.sleep(20);
  expect(interrupted).toBe(true);
  clock += FAILED_LOOKUP_RETRY_MS;
  expect(await resolveIsSubagent(host, "timeout-child")).toBe(true);
  expect(calls).toBe(2);
});
