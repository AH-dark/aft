/**
 * The names AFT registers its command and patch tools under on OpenCode 2.
 *
 * OpenCode 2's timeline gives its rich rows (title, command and output for a
 * command; file count and diff for a patch) only to tools it recognises by
 * name, and those names are `shell` and `patch`. OpenCode 2 also files shell
 * permission rules under `shell`. So on OpenCode 2 AFT's `bash` is registered
 * as `shell` and its `apply_patch` as `patch`, replacing the host's built-ins
 * under the same names. OpenCode 1 and Pi keep `bash` and `apply_patch`.
 *
 * Only the name the host and the model see changes. Inside the plugin the
 * tool map, `disabled_tools`, the config keys and every call into AFT's engine
 * keep the AFT names; this module translates at the host boundary in both
 * directions. The engine gets the same map on every request (the
 * `host_tool_names` field beside the session), so the text it renders names
 * the tools the model can actually call.
 */

/** AFT tool name -> the name it is registered under on OpenCode 2. */
export const V2_HOST_TOOL_NAMES: Readonly<Record<"bash" | "apply_patch", string>> = Object.freeze({
  bash: "shell",
  apply_patch: "patch",
});

const AFT_TOOL_FOR_V2_HOST: Readonly<Record<string, string>> = Object.freeze(
  Object.fromEntries(Object.entries(V2_HOST_TOOL_NAMES).map(([aft, host]) => [host, aft])),
);

/** The name an AFT tool is registered under on OpenCode 2. */
export function v2HostToolName(aftName: string): string {
  return (V2_HOST_TOOL_NAMES as Readonly<Record<string, string>>)[aftName] ?? aftName;
}

/**
 * The AFT tool a host-side tool name refers to, for hook events the host keys
 * by the registered name. A name AFT never renamed comes back unchanged.
 */
export function aftToolNameForV2Host(hostName: string): string {
  return AFT_TOOL_FOR_V2_HOST[hostName] ?? hostName;
}

/**
 * Rewrite the references to `bash` and `apply_patch` as tools in a description
 * or hint written for OpenCode 1, so it names the tools an OpenCode 2 model
 * can call.
 *
 * The shared descriptions are built once for both hosts, and on OpenCode 1
 * they must stay byte-identical (a changed byte rebuilds every session's
 * prompt cache), so they are rewritten here instead of being parameterised at
 * every call site. Only a tool reference changes:
 * - `bash(` and `` `bash` `` and a bare word `bash` become `shell`;
 * - `` `apply_patch` `` and `apply_patch(` become `patch`.
 * These stay: config keys (`bash.background`), companion tools (`bash_status`),
 * task ids (`bash-6b45…`), and `bash` as a program or a syntax ("python/node/
 * bash REPLs", "bash itself", "bash syntax").
 */
export function renameV2ToolReferences(text: string): string {
  return text
    .replace(/(?<![\w./-])bash(?![\w-]|\.\w|\s+(?:itself|REPLs|syntax)\b)/g, "shell")
    .replace(/(?<![\w./-])apply_patch(?![\w-]|\.\w)/g, "patch");
}
