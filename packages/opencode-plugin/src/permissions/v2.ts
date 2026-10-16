import type { PermissionDomain } from "@opencode/plugin/effect/permission";
import { Cause, Effect } from "effect";

import type { V2ExecutionContext, V2PermissionRequest } from "../tools/definitions/v2.js";

type AssertInput = Parameters<PermissionDomain["assert"]>[0];
export class PermissionDeniedError extends Error {
  override readonly name = "PermissionDeniedError";
}

export class PermissionRulesUnavailableError extends Error {
  override readonly name = "PermissionRulesUnavailableError";
}

/** Rejecting a prompt without feedback ends the model's current step, not just the tool call. */
export class PermissionStepAbort extends Error {
  constructor(readonly original: unknown) {
    super("OpenCode ended the current step");
  }
}

export type V2PermissionEffect = "allow" | "deny" | "ask";

/** One ordered `{action, resource, effect}` entry of an OpenCode permission ruleset. */
export interface V2PermissionRule {
  readonly action: string;
  readonly resource: string;
  readonly effect: V2PermissionEffect;
}

export interface V2AgentRecord {
  readonly permissions?: readonly V2PermissionRule[];
}

export interface V2SessionRecord {
  readonly agent?: string;
  readonly permissions?: readonly V2PermissionRule[];
}

/**
 * The two plugin domains AFT reads to learn the host's configured answer.
 *
 * OpenCode 2 hands a plugin `agent` and `session` domains whose records carry
 * the same ordered permission rulesets the host itself evaluates, so a tool can
 * resolve "allow", "deny", or "ask" without a round trip to the user.
 */
export interface V2PermissionHostContext {
  readonly permission: Pick<PermissionDomain, "assert">;
  readonly agent: {
    get(input: { agentID: string }): Effect.Effect<{ data: V2AgentRecord }, unknown>;
  };
  readonly session: {
    get(input: { sessionID: string }): Effect.Effect<V2SessionRecord | undefined, unknown>;
  };
}

/**
 * OpenCode's permission pattern matcher, kept byte-compatible with the host.
 *
 * Both the action and the resource are matched with it, `*` spans any run of
 * characters, `?` spans one, and a pattern ending in " *" also matches the
 * bare prefix so a `bash` rule like "git push *" still covers "git push".
 */
function wildcardMatch(input: string, pattern: string): boolean {
  const normalized = input.replaceAll("\\", "/");
  let escaped = pattern
    .replaceAll("\\", "/")
    .replaceAll(/[.+^${}()|[\]\\]/g, "\\$&")
    .replaceAll(/\*/g, ".*")
    .replaceAll(/\?/g, ".");
  if (escaped.endsWith(" .*")) escaped = `${escaped.slice(0, -3)}( .*)?`;
  return new RegExp(`^${escaped}$`, process.platform === "win32" ? "si" : "s").test(normalized);
}

/**
 * Last matching rule wins; an unmatched action falls through to "ask".
 *
 * Reverse iteration rather than `findLast` because this package targets ES2022.
 */
function matchingRule(
  action: string,
  resource: string,
  rules: readonly V2PermissionRule[],
): V2PermissionRule {
  for (let index = rules.length - 1; index >= 0; index -= 1) {
    const rule = rules[index];
    if (rule && wildcardMatch(action, rule.action) && wildcardMatch(resource, rule.resource)) {
      return rule;
    }
  }
  return { action, resource: "*", effect: "ask" };
}

export interface V2PermissionDecision {
  readonly effect: V2PermissionEffect;
  /** The resource that decided the outcome, absent when every resource allowed. */
  readonly resource?: string;
  /** The rule that decided the outcome, absent when every resource allowed. */
  readonly rule?: V2PermissionRule;
}

/**
 * Resolve one AFT permission request against a host ruleset.
 *
 * Mirrors OpenCode's own evaluation: any denied resource denies the whole
 * request, otherwise any resource that still wants a prompt makes the request
 * an "ask", and only a request whose resources are all allowed is allowed.
 *
 * One deliberate difference: OpenCode also folds in the user's saved "always"
 * grants before the allow/ask split, and those are not readable from a plugin.
 * A saved grant therefore reads as "ask" here; the host applies it during
 * assert and allows without showing anything.
 */
export function decidePermission(
  request: V2PermissionRequest,
  rules: readonly V2PermissionRule[],
): V2PermissionDecision {
  for (const resource of request.patterns) {
    const rule = matchingRule(request.permission, resource, rules);
    if (rule.effect === "deny") return { effect: "deny", resource, rule };
  }
  for (const resource of request.patterns) {
    const rule = matchingRule(request.permission, resource, rules);
    if (rule.effect === "ask") return { effect: "ask", resource, rule };
  }
  return { effect: "allow" };
}

function detail(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

async function runHostEffect<A>(effect: Effect.Effect<A, unknown>): Promise<A> {
  return await Effect.runPromise(effect as Effect.Effect<A, never>);
}

async function sessionRecord(
  host: V2PermissionHostContext,
  sessionID: string | undefined,
): Promise<V2SessionRecord | undefined> {
  if (!sessionID) return undefined;
  try {
    return await runHostEffect(host.session.get({ sessionID }));
  } catch (error) {
    throw new PermissionRulesUnavailableError(
      `reading session ${JSON.stringify(sessionID)} failed: ${detail(error)}`,
    );
  }
}

async function agentRules(
  host: V2PermissionHostContext,
  agentID: string,
): Promise<readonly V2PermissionRule[]> {
  let record: V2AgentRecord;
  try {
    record = (await runHostEffect(host.agent.get({ agentID }))).data;
  } catch (error) {
    throw new PermissionRulesUnavailableError(
      `reading agent ${JSON.stringify(agentID)} failed: ${detail(error)}`,
    );
  }
  if (!record.permissions) {
    throw new PermissionRulesUnavailableError(
      `agent ${JSON.stringify(agentID)} reported no permission rules`,
    );
  }
  return record.permissions;
}

/**
 * Read the ordered ruleset OpenCode would evaluate for this tool call.
 *
 * The agent's rules come first and the session's own overrides are appended
 * after them, which is the order the host uses and the reason a session
 * override wins: evaluation takes the last matching rule.
 */
async function configuredRules(
  host: V2PermissionHostContext,
  context: V2ExecutionContext,
): Promise<readonly V2PermissionRule[]> {
  const session = await sessionRecord(host, context.sessionID);
  const agentID = context.agent ?? session?.agent;
  if (!agentID) {
    throw new PermissionRulesUnavailableError(
      "the tool call carried no agent identifier and its session named none",
    );
  }
  return [...(await agentRules(host, agentID)), ...(session?.permissions ?? [])];
}

function describeRule(rule: V2PermissionRule | undefined): string {
  if (!rule) return "no matching rule";
  return `${JSON.stringify(rule.action)} on ${JSON.stringify(rule.resource)} is ${JSON.stringify(rule.effect)}`;
}

function deniedMessage(request: V2PermissionRequest, decision: V2PermissionDecision): string {
  return (
    `Permission denied: this session's permission rules deny ${JSON.stringify(request.permission)}` +
    `${decision.resource === undefined ? "" : ` for ${JSON.stringify(decision.resource)}`}` +
    ` (${describeRule(decision.rule)}).`
  );
}

function requiredContextID(
  context: V2ExecutionContext,
  key: "sessionID" | "messageID" | "id",
): string {
  const value = context[key];
  if (typeof value !== "string" || value.length === 0) {
    throw new Error(`V2 permission request requires context.${key}`);
  }
  return value;
}

/**
 * Answer a V2 tool's permission request.
 *
 * The host's configured rules are evaluated first, so anything the user has
 * already permitted — reading a file inside the project under OpenCode's
 * default allow-everything ruleset, for instance — runs with no round trip,
 * and a denial fails naming the rule that produced it. Only a rule that asks
 * costs a call to the host, which raises the real prompt.
 */
export async function requestPermission(
  host: V2PermissionHostContext,
  request: V2PermissionRequest,
  context: V2ExecutionContext,
  signal?: AbortSignal,
): Promise<void> {
  let rules: readonly V2PermissionRule[];
  try {
    rules = await configuredRules(host, context);
  } catch (error) {
    if (!(error instanceof PermissionRulesUnavailableError)) throw error;
    throw new PermissionRulesUnavailableError(
      `The ${JSON.stringify(request.permission)} operation was refused because AFT could not ` +
        `read this session's OpenCode permission rules (${error.message}), so it cannot tell ` +
        "whether the operation is permitted.",
    );
  }

  const decision = decidePermission(request, rules);
  if (decision.effect === "allow") return;
  if (decision.effect === "deny") throw new PermissionDeniedError(deniedMessage(request, decision));

  const result = await Effect.runPromiseExit(
    host.permission.assert({
      sessionID: requiredContextID(context, "sessionID") as AssertInput["sessionID"],
      ...(context.agent ? { agent: context.agent as AssertInput["agent"] } : {}),
      action: request.permission,
      resources: [...request.patterns],
      save: [...request.always],
      metadata: { ...request.metadata },
      source: {
        type: "tool",
        messageID: requiredContextID(context, "messageID"),
        id: requiredContextID(context, "id"),
      },
    }),
    { signal },
  );
  if (result._tag === "Failure") {
    const error = Cause.squash(result.cause);
    if (
      typeof error === "object" &&
      error !== null &&
      "_tag" in error &&
      error._tag === "Tool.Error"
    ) {
      throw error;
    }
    throw new PermissionStepAbort(error);
  }
}
