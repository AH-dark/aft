import type { PermissionDomain } from "@opencode/plugin/effect/permission";
import { Cause, Effect } from "effect";

import type { V2ExecutionContext, V2PermissionRequest } from "../tools/definitions/v2.js";

type AssertInput = Parameters<PermissionDomain["assert"]>[0];
/** Rejecting a prompt without feedback ends the model's current step, not just the tool call. */
export class PermissionStepAbort extends Error {
  constructor(readonly original: unknown) {
    super("OpenCode ended the current step");
  }
}

/** The host evaluates rules and hooks before prompting only when approval is required. */
export interface V2PermissionHostContext {
  readonly permission: Pick<PermissionDomain, "assert">;
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

/** Delegate every decision to OpenCode so permission hooks also see allow and deny. */
export async function requestPermission(
  host: V2PermissionHostContext,
  request: V2PermissionRequest,
  context: V2ExecutionContext,
  signal?: AbortSignal,
): Promise<void> {
  const result = await Effect.runPromiseExit(
    host.permission.assert({
      sessionID: requiredContextID(context, "sessionID") as AssertInput["sessionID"],
      ...(context.agent ? { agent: context.agent as AssertInput["agent"] } : {}),
      action: request.permission,
      resources:
        request.permission === "read"
          ? request.patterns.map((path) => path.replaceAll("\\", "/"))
          : [...request.patterns],
      save: [...request.always],
      ...(Object.keys(request.metadata).length ? { metadata: { ...request.metadata } } : {}),
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
