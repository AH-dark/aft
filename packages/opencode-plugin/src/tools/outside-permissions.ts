import type { ToolContext } from "@opencode-ai/plugin";
import { classifyPermissionError, permissionRuleDenial, runAsk } from "./permissions.js";

type Runtime = Partial<Pick<ToolContext, "ask">> & {
  effectAbort?: AbortSignal;
  hostVersion?: 2;
  sessionID?: string;
  messageID?: string;
  id?: string;
  callID?: string;
};
type Continuation = { key: string; tokens: string[] };
const continuations = new WeakMap<object, Continuation>();
const hostGrants = new WeakMap<object, readonly string[]>();

export function permissionOperation(tool: string, args: Record<string, unknown>): string {
  if (["hashline_preflight", "hashline_edit", "edit", "edit_symbol"].includes(tool)) return "edit";
  if (["undo_preview", "undo"].includes(tool)) return "safety:undo";
  if (["checkpoint_paths", "restore_checkpoint"].includes(tool)) return "safety:restore";
  if (tool === "checkpoint") return "safety:checkpoint";
  if (tool === "safety") return `safety:${String(args.op ?? "")}`;
  return (
    ({ read_file: "read", write_file: "write", delete_file: "delete" } as Record<string, string>)[
      tool
    ] ?? tool
  );
}

export function inheritHostPermissionGrants(source: object, target: object): void {
  const grants = hostGrants.get(source);
  if (grants) hostGrants.set(target, grants);
}

/** Shell approvals come from the plugin's permission loop, never agent fields. */
export function withHostPermissionGrants(args: Record<string, unknown>, grants: readonly string[]) {
  const { permissions_granted: _untrusted, ...clean } = args;
  hostGrants.set(clean, [...grants]);
  return clean;
}

/** Present AFT's outside-path requests through the host callback: OpenCode 2
 * binds it to permission.assert, while OpenCode 1 binds it to context.ask. */
export async function withOutsidePermissions<T extends Record<string, unknown>>(
  runtime: Runtime,
  args: Record<string, unknown>,
  send: (args: Record<string, unknown>) => Promise<T>,
  preview = false,
  operation = "",
): Promise<T> {
  const { permissions_granted: _untrusted, ...clean } = args;
  const inherited = hostGrants.get(args) ?? [];
  // The continuation is stored on this call's runtime object, so it already
  // belongs to one tool call. The key adds the operation family and the
  // session, message and call identifiers the host provides, so a
  // continuation never moves to a different operation. It must not include
  // the arguments: a preview and its apply send different arguments
  // (undo_preview's `file`, then safety's `op` and its own path argument), and OpenCode 1
  // gives no call ID, so an argument-based key would never match and the user
  // would be asked twice.
  const key = JSON.stringify([
    permissionOperation(operation, clean),
    runtime.sessionID ?? null,
    runtime.messageID ?? null,
    runtime.id ?? runtime.callID ?? null,
  ]);
  const prior = continuations.get(runtime);
  let granted = prior?.key === key ? [...prior.tokens] : [];
  continuations.delete(runtime);
  for (let round = 0; round < 32; round++) {
    if (round > 0) runtime.effectAbort?.throwIfAborted();
    const approvals = [...inherited, ...granted];
    const response = await send(
      approvals.length ? { ...clean, permissions_granted: approvals } : clean,
    );
    granted = Array.isArray(response.permissions_continuation)
      ? response.permissions_continuation.filter(
          (id): id is string => typeof id === "string" && id.startsWith("outside:"),
        )
      : [];
    delete response.permissions_continuation;
    if (response.success !== false || response.code !== "permission_required") {
      if (preview && response.success !== false && granted.length)
        continuations.set(runtime, { key, tokens: granted });
      return response;
    }
    const asks = response.asks as
      | Array<{
          kind: string;
          grant_id?: string;
          patterns?: string[];
          always?: string[];
          path?: string;
          parentDir?: string;
        }>
      | undefined;
    if (
      !asks?.length ||
      !asks.every(
        (ask) => ask.kind === "external_directory" && ask.grant_id?.startsWith("outside:"),
      )
    ) {
      if (granted.length) continuations.set(runtime, { key, tokens: granted });
      return response;
    }
    if (typeof runtime.ask !== "function")
      throw new Error(
        "Permission denied: outside access requires a permission-capable OpenCode host.",
      );
    for (const ask of asks) {
      try {
        await runAsk(
          runtime.ask({
            permission: "external_directory",
            patterns: ask.patterns ?? [],
            always: ask.always ?? [],
            metadata: { filepath: ask.path, parentDir: ask.parentDir },
          }),
        );
      } catch (error) {
        // The OC2 adapter preserves host denial and step-abort objects.
        if (runtime.hostVersion === 2) throw error;
        const failure = classifyPermissionError(error, "Permission denied (external directory).");
        throw new Error(
          failure.kind === "rule_denied"
            ? permissionRuleDenial(
                "external_directory",
                ask.parentDir ?? ask.path ?? ask.patterns?.[0] ?? "outside path",
              )
            : failure.message,
        );
      }
      runtime.effectAbort?.throwIfAborted();
      if (ask.grant_id && !granted.includes(ask.grant_id)) granted.push(ask.grant_id);
    }
  }
  throw new Error("outside-access permission retry failed: too many rounds");
}
