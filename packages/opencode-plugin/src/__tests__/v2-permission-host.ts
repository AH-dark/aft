import { Error as ToolError } from "@opencode/plugin/promise/tool";
import { Effect } from "effect";
import type { V2PermissionHostContext } from "../permissions/v2.js";

export interface V2PermissionRule {
  action: string;
  resource: string;
  effect: "allow" | "deny" | "ask";
}

function matches(value: string, pattern: string): boolean {
  let expression = pattern
    .replaceAll("\\", "/")
    .replace(/[.+^${}()|[\]\\]/g, "\\$&")
    .replaceAll("*", ".*")
    .replaceAll("?", ".");
  if (expression.endsWith(" .*")) expression = `${expression.slice(0, -3)}( .*)?`;
  return new RegExp(`^${expression}$`, process.platform === "win32" ? "si" : "s").test(
    value.replaceAll("\\", "/"),
  );
}

/** A test-only ruleset model; production decisions belong exclusively to the host. */
export function decidePermission(
  request: { permission: string; patterns: readonly string[] },
  rules: readonly V2PermissionRule[],
): { effect: V2PermissionRule["effect"] } {
  const effects = request.patterns.map((resource) => {
    for (let index = rules.length - 1; index >= 0; index--) {
      const rule = rules[index]!;
      if (matches(request.permission, rule.action) && matches(resource, rule.resource))
        return rule.effect;
    }
    return "ask";
  });
  return { effect: effects.includes("deny") ? "deny" : effects.includes("ask") ? "ask" : "allow" };
}

type AssertInput = Parameters<V2PermissionHostContext["permission"]["assert"]>[0];
export function permissionHost(
  options: {
    agentRules?: readonly V2PermissionRule[];
    sessionRules?: readonly V2PermissionRule[];
    rejection?: Error;
    evaluate?: (
      input: AssertInput,
      effect: V2PermissionRule["effect"],
    ) => V2PermissionRule["effect"];
  } = {},
) {
  const calls: AssertInput[] = [];
  const prompts: AssertInput[] = [];
  const rules = options.agentRules ?? [{ action: "*", resource: "*", effect: "ask" as const }];
  // Perform no evaluation until the returned Effect runs. Then check configured
  // denies before hooks, return on allow, and prompt only for ask, matching
  // @opencode/core 2.0.31, dist/chunks/location-0k8emadn.js:169-186 and 234-236.
  const host = {
    agent: { get: () => Effect.succeed({ data: { permissions: rules } }) },
    session: {
      get: () => Effect.succeed({ agent: "build", permissions: options.sessionRules ?? [] }),
    },
    permission: {
      assert: (input: AssertInput) =>
        Effect.suspend(() => {
          calls.push(input);
          const initial = decidePermission(
            { permission: input.action, patterns: input.resources },
            [...rules, ...(options.sessionRules ?? [])],
          ).effect;
          const effect =
            initial === "deny" ? initial : (options.evaluate?.(input, initial) ?? initial);
          if (effect === "deny")
            return Effect.fail(
              new ToolError({
                message: `Permission denied: ${input.action}`,
                error: {
                  _tag: "Permission.BlockedError",
                  rules: [...rules, ...(options.sessionRules ?? [])],
                  permission: input.action,
                  resources: input.resources,
                },
              }),
            );
          if (effect === "allow") return Effect.void;
          prompts.push(input);
          return options.rejection instanceof ToolError
            ? Effect.fail(options.rejection)
            : options.rejection
              ? Effect.die(options.rejection)
              : Effect.void;
        }),
    },
  };
  return { host, calls, prompts };
}
