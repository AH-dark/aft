import { requestPermission, type V2PermissionHostContext } from "../../permissions/v2.js";
import type { V2ToolConsumers } from "../definitions/v2.js";

export const V2_BUILTIN_REPLACEMENTS = ["read", "edit", "write", "apply_patch"] as const;
export const V2_AFT_FILESYSTEM_TOOLS = ["aft_delete", "aft_move"] as const;

export const V2_PERMISSION_ASK_INVENTORY = [
  "read",
  "edit",
  "write",
  "apply_patch",
  "aft_delete",
  "aft_move",
  "bash:withPermissionLoop",
  "bash:host-fallback",
] as const;

function domainMethod(host: object, domain: string, method: string): boolean {
  const value = (host as Record<string, unknown>)[domain];
  if (!value || typeof value !== "object") return false;
  return typeof (value as Record<string, unknown>)[method] === "function";
}

/**
 * Bind projected tools to the host's in-process permission engine.
 * Missing capabilities refuse startup instead of leaving gated tools disabled.
 */
export function hoistedV2ToolConsumers(host: V2PermissionHostContext | object): V2ToolConsumers {
  if (!domainMethod(host, "permission", "assert")) {
    throw new Error(
      "AFT requires OpenCode 2.0.29 or newer: this V2 host has no permission.assert. Upgrade OpenCode before loading AFT.",
    );
  }
  if (!domainMethod(host, "agent", "get") || !domainMethod(host, "session", "get")) {
    throw new Error(
      "AFT requires OpenCode 2.0.29 or newer with agent.get and session.get permission rules.",
    );
  }
  return {
    requestPermission: (request, context, signal) =>
      requestPermission(host as V2PermissionHostContext, request, context, signal),
  };
}
