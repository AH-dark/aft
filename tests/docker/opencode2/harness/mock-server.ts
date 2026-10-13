import { createRequire } from "node:module";
import { appendFileSync, mkdirSync, writeFileSync } from "node:fs";
import { dirname } from "node:path";

import type {
  RecordedMockExchange,
  ScenarioDefinition,
  ScriptedTurn,
  ToolCallPlan,
} from "./types.js";
import { fail } from "./errors.js";
import { asRecord } from "./util.js";

interface LLMockLike {
  url: string;
  on(
    matcher: { sequenceIndex?: number; predicate?: (request: unknown) => boolean },
    response: unknown,
    options?: unknown,
  ): void;
  onMessage(pattern: string | RegExp, response: unknown): void;
  getRequests(): unknown[];
  start(): Promise<void>;
  stop(): Promise<void>;
}

interface LLMockConstructor {
  new (options: { port: number }): LLMockLike;
}

export interface DeterministicMockHooks {
  beforeTurn?: (turn: ScriptedTurn, request: unknown) => void | Promise<void>;
  afterRequest?: (exchange: RecordedMockExchange, turn?: ScriptedTurn) => void | Promise<void>;
  afterResponse?: (exchange: RecordedMockExchange) => void | Promise<void>;
}

const TURN_PLACEHOLDER = /\{\{(permission_id|session_id|task_id)(?::([^{}]+))?\}\}/g;

function materializeValue(value: unknown, values: Readonly<Record<string, string>>): unknown {
  if (typeof value === "string") {
    return value.replace(
      TURN_PLACEHOLDER,
      (placeholder, kind: string, qualifier: string | undefined) => {
        const key = qualifier ? `${kind}:${qualifier}` : kind;
        const replacement = values[key];
        if (!replacement) {
          fail("scenario_invalid", `scripted turn placeholder has no value: ${placeholder}`);
        }
        return replacement;
      },
    );
  }
  if (Array.isArray(value)) return value.map((entry) => materializeValue(entry, values));
  const record = asRecord(value);
  if (!record) return value;
  return Object.fromEntries(
    Object.entries(record).map(([key, entry]) => [key, materializeValue(entry, values)]),
  );
}

export function materializeTurnPlaceholders(
  turn: ScriptedTurn,
  values: Readonly<Record<string, string>>,
): void {
  if (turn.response.kind === "text") {
    turn.response.content = materializeValue(turn.response.content, values) as string;
    return;
  }
  for (const call of turn.response.calls) {
    call.arguments = materializeValue(call.arguments, values) as Record<string, unknown>;
  }
}

/** Which OpenCode generation a scenario leg runs on. */
export type HostGeneration = "v1" | "v2";

/**
 * AFT tools an OpenCode 2 host registers under its own names. OpenCode 2
 * renders its native command and diff rows only for `shell` and `patch`, so
 * AFT registers its `bash` and `apply_patch` under those names there (the
 * host's own shell and patch plugins are removed by the isolation config).
 * OpenCode 1 keeps AFT's names.
 */
const V2_HOST_TOOL_NAMES: Readonly<Record<string, string>> = {
  bash: "shell",
  apply_patch: "patch",
};

/**
 * The name a host offers a scenario's tool under. Scenarios name AFT's tools
 * as AFT does on OpenCode 1; the model has to call them by the name the host
 * leg actually registered.
 */
export function hostToolName(name: string, generation: HostGeneration = "v1"): string {
  if (name === "ast_search") return "ast_grep_search";
  if (name === "ast_replace") return "ast_grep_replace";
  if (generation === "v2") return V2_HOST_TOOL_NAMES[name] ?? name;
  return name;
}

export function hostToolArguments(
  name: string,
  input: Record<string, unknown>,
): Record<string, unknown> {
  const bare = name.startsWith("aft_") ? name.slice(4) : name;
  if (!new Set(["read", "write", "edit"]).has(bare) || !("filePath" in input)) return input;
  const { filePath, ...rest } = input;
  return { path: filePath, ...rest };
}

function mockResponse(turn: ScriptedTurn, generation: HostGeneration): Record<string, unknown> {
  if (turn.response.kind === "text") return { content: turn.response.content };
  return {
    toolCalls: turn.response.calls.map((call) => ({
      id: call.id,
      name: hostToolName(call.name, generation),
      arguments: JSON.stringify(hostToolArguments(call.name, call.arguments)),
    })),
  };
}

export async function observeThenRespond(
  turn: ScriptedTurn,
  request: unknown,
  index: number,
  hooks: DeterministicMockHooks,
  generation: HostGeneration = "v1",
): Promise<{ exchange: RecordedMockExchange; response: Record<string, unknown> }> {
  await hooks.beforeTurn?.(turn, request);
  const exchange: RecordedMockExchange = {
    index,
    label: turn.label,
    request,
    response: undefined,
    observed_at: new Date().toISOString(),
  };
  await hooks.afterRequest?.(exchange, turn);
  const response = mockResponse(turn, generation);
  exchange.response = response;
  await hooks.afterResponse?.(exchange);
  return { exchange, response };
}

export function isTitleGenerationRequest(request: unknown): boolean {
  const messages = asRecord(request)?.messages;
  if (!Array.isArray(messages)) return false;
  return messages.some((value) => {
    const message = asRecord(value);
    return (
      message?.role === "system" &&
      typeof message.content === "string" &&
      message.content.includes("You are a title generator")
    );
  });
}

/** Check the actual provider request, not a model name inferred from the fixture. */
export function assertRequestToolSurface(
  exchange: RecordedMockExchange,
  expected: { model: string; present: readonly string[]; absent: readonly string[] },
): string[] {
  const request = asRecord(exchange.request);
  if (request?.model !== expected.model) {
    fail("host_failed", `tool surface request used ${String(request?.model)}, expected ${expected.model}`);
  }
  if (!Array.isArray(request.tools) || request.tools.length === 0) {
    fail("host_failed", "tool surface request contains no tools");
  }
  const names = request.tools.flatMap((value) => {
    const entry = asRecord(value);
    const fn = asRecord(entry?.function) ?? entry;
    return typeof fn?.name === "string" ? [fn.name] : [];
  }).sort();
  for (const name of expected.present) {
    if (!names.includes(name)) fail("host_failed", `tool surface request is missing ${name}`, { model: request.model, names });
  }
  for (const name of expected.absent) {
    if (names.includes(name)) fail("host_failed", `tool surface request unexpectedly includes ${name}`, { model: request.model, names });
  }
  return names;
}

export class DeterministicScenarioMock {
  readonly scenario: ScenarioDefinition;
  readonly turnLogPath: string;
  readonly exchanges: RecordedMockExchange[] = [];
  readonly hooks: DeterministicMockHooks;
  #mock?: LLMockLike;

  /** The host generation this mock answers; it decides the tool names it calls. */
  readonly hostGeneration: HostGeneration;

  constructor(
    scenario: ScenarioDefinition,
    turnLogPath: string,
    hooks: DeterministicMockHooks = {},
    hostGeneration: HostGeneration = "v1",
  ) {
    this.scenario = scenario;
    this.turnLogPath = turnLogPath;
    this.hooks = hooks;
    this.hostGeneration = hostGeneration;
  }

  get url(): string {
    if (!this.#mock) throw new Error("deterministic mock has not started");
    return this.#mock.url;
  }

  get requests(): unknown[] {
    return this.#mock?.getRequests() ?? [];
  }

  async start(port = 0): Promise<void> {
    mkdirSync(dirname(this.turnLogPath), { recursive: true });
    writeFileSync(this.turnLogPath, "");
    const require = createRequire(import.meta.url);
    const loaded = require("@copilotkit/aimock") as { LLMock?: LLMockConstructor };
    if (!loaded.LLMock) throw new Error("@copilotkit/aimock does not export LLMock");
    const mock = new loaded.LLMock({ port });
    this.#mock = mock;
    mock.on(
      { predicate: isTitleGenerationRequest },
      { content: "Scenario title" },
    );
    let nextTurn = 0;
    for (const [index, turn] of this.scenario.turns.entries()) {
      mock.on(
        {
          predicate: (request) =>
            !isTitleGenerationRequest(request) && nextTurn === index,
        },
        async (request: unknown) => {
          nextTurn = index + 1;
          const { exchange, response } = await observeThenRespond(
            turn,
            request,
            index,
            this.hooks,
            this.hostGeneration,
          );
          appendFileSync(this.turnLogPath, `${turn.label}\n`);
          this.exchanges.push(exchange);
          return response;
        },
        turn.delay_ms ? { streamingProfile: { ttft: turn.delay_ms, tps: 1_000 } } : undefined,
      );
    }
    mock.on(
      {
        predicate: (request) =>
          !isTitleGenerationRequest(request) && nextTurn >= this.scenario.turns.length,
      },
      { content: "Scenario complete" },
    );
    await mock.start();
  }

  async stop(): Promise<void> {
    const mock = this.#mock;
    this.#mock = undefined;
    if (mock) await mock.stop();
  }
}

function messageToolCallId(message: Record<string, unknown>): string | undefined {
  const value = message.tool_call_id ?? message.toolCallId;
  return typeof value === "string" ? value : undefined;
}

export function toolResultForCall(
  exchanges: readonly RecordedMockExchange[],
  callId: string,
): { text: string; event: Record<string, unknown> } | undefined {
  const hostCallId = callId.slice(0, 40);
  for (const exchange of exchanges) {
    const messages = asRecord(exchange.request)?.messages;
    if (!Array.isArray(messages)) continue;
    for (const messageValue of messages) {
      const message = asRecord(messageValue);
      if (message?.role !== "tool" || messageToolCallId(message) !== hostCallId) continue;
      const content = message.content;
      if (typeof content === "string") return { text: content, event: message };
      if (Array.isArray(content)) {
        const text = content
          .map((part) => asRecord(part)?.text)
          .filter((value): value is string => typeof value === "string")
          .join("");
        return { text, event: message };
      }
    }
  }
  return undefined;
}

export function toolCallsInTurn(turn: ScriptedTurn): ToolCallPlan[] {
  return turn.response.kind === "tool_calls" ? turn.response.calls : [];
}
