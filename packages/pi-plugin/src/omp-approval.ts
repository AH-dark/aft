/**
 * Approval tiers for AFT tools on OMP (oh-my-pi).
 *
 * OMP gates every tool call on its approval mode (`tools.approvalMode`:
 * `always-ask` | `write` | `yolo`, forced to `yolo` by `--auto-approve`). A
 * tool declares its capability tier through an `approval` field; a tool that
 * declares none is treated as `exec`, so in `write` mode OMP would prompt for
 * every AFT call, reads included (oh-my-pi
 * `packages/coding-agent/src/tools/approval.ts`, `normalizeDecision` and
 * `resolveApproval`).
 *
 * OMP does not tell an extension tool which approval mode the session runs in,
 * so AFT never raises its own outside-access prompt on OMP. Instead the tier is
 * a function of the call's arguments: a call naming a path outside the
 * session's project reports `exec`, so OMP's own mode decides whether it is
 * asked about (prompted in `write` and `always-ask`, never in `yolo`). The
 * outside check is lexical and advisory only: it decides whether OMP prompts,
 * while the engine still enforces its floors and `restrict_to_project_root`.
 *
 * Upstream Pi has no approval gate and gets no `approval` field.
 */

import { homedir } from "node:os";
import { isAbsolute, relative, resolve, sep } from "node:path";
import { decodeFileUrl, relativePathEscapesRoot } from "@cortexkit/aft-bridge";

/** OMP's capability tiers, least to most privileged. */
export type OmpToolTier = "read" | "write" | "exec";

/**
 * The tier of each AFT tool when every path it names is inside the project.
 * A tool missing from this table declares nothing, and OMP treats it as `exec`.
 */
export const OMP_TOOL_TIERS: Readonly<Record<string, OmpToolTier>> = {
  // Read-only: they read files, indexes or background-task state.
  read: "read",
  grep: "read",
  glob: "read",
  aft_search: "read",
  aft_outline: "read",
  aft_zoom: "read",
  aft_callgraph: "read",
  aft_inspect: "read",
  // Lists merge-conflict regions; it changes nothing.
  aft_conflicts: "read",
  // Searches only; the replacing twin is `ast_grep_replace` below.
  ast_grep_search: "read",
  bash_status: "read",
  bash_watch: "read",
  // Change files: edits, moves and deletes, imports, and undo/restore, which
  // write files back from their backups.
  write: "write",
  edit: "write",
  apply_patch: "write",
  aft_delete: "write",
  aft_move: "write",
  aft_import: "write",
  aft_safety: "write",
  ast_grep_replace: "write",
  // Run commands, or act on running ones.
  bash: "exec",
  powershell: "exec",
  bash_write: "exec",
  bash_kill: "exec",
};

const TIER_RANK: Readonly<Record<OmpToolTier, number>> = { read: 0, write: 1, exec: 2 };

/** Argument keys whose string values (or arrays of strings) name filesystem paths. */
const PATH_KEYS = new Set([
  "path",
  "paths",
  "file",
  "files",
  "file_path",
  "filePath",
  "target",
  "scope",
  "workdir",
  "destination",
  "toPath",
]);

/** A non-`file:` URL in `scheme://` form (an `https://` page, an `issue://N` reference) names no filesystem path. */
const NON_FILE_URL = /^[a-zA-Z][a-zA-Z0-9+.-]*:\/\//;

function expandPath(value: string): string {
  const decoded = decodeFileUrl(value);
  if (decoded === "~") return homedir();
  if (decoded.startsWith("~/") || decoded.startsWith(`~${sep}`))
    return resolve(homedir(), decoded.slice(2));
  return decoded;
}

/** Whether `value`, resolved against `root`, lies outside it. Purely lexical. */
export function namesOutsidePath(root: string, value: string): boolean {
  if (value.length === 0) return false;
  if (!value.startsWith("file:") && NON_FILE_URL.test(value)) return false;
  const expanded = expandPath(value);
  if (expanded.startsWith("file:")) return false;
  const absolute = isAbsolute(expanded) ? expanded : resolve(root, expanded);
  return relativePathEscapesRoot(relative(root, absolute));
}

function collectPaths(value: unknown, key: string | undefined, out: string[], depth: number): void {
  if (depth > 6 || value === null || value === undefined) return;
  if (typeof value === "string") {
    if (key !== undefined && PATH_KEYS.has(key)) out.push(value);
    return;
  }
  if (Array.isArray(value)) {
    for (const item of value) collectPaths(item, key, out, depth + 1);
    return;
  }
  if (typeof value === "object") {
    for (const [child, item] of Object.entries(value as Record<string, unknown>))
      collectPaths(item, child, out, depth + 1);
  }
}

/** The path-like argument values of one call, nested batches included (to six levels deep). */
export function pathArguments(args: unknown): string[] {
  const out: string[] = [];
  collectPaths(args, undefined, out, 0);
  return out;
}

/**
 * The tier OMP should see for one call of `tool`: `exec` when any path it names
 * is outside `root`, else the tool's own tier. Never throws: OMP calls this
 * synchronously inside its approval gate, where a throw would fail the call.
 */
export function ompCallTier(
  tool: string,
  args: unknown,
  root: string | undefined,
  prepare?: (args: unknown) => unknown,
): OmpToolTier {
  const natural = OMP_TOOL_TIERS[tool] ?? "exec";
  if (natural === "exec" || !root) return natural;
  try {
    const prepared = prepare ? prepare(args) : args;
    const outside = pathArguments(prepared).some((path) => namesOutsidePath(root, path));
    return outside && TIER_RANK.exec > TIER_RANK[natural] ? "exec" : natural;
  } catch {
    // Arguments OMP will reject anyway; the tool's own tier is enough here.
    return natural;
  }
}

/**
 * The `approval` value to declare for `tool` on OMP, or undefined for a tool
 * AFT does not classify. It never carries a `policy`: an explicit `prompt`
 * would make OMP ask even in `yolo`, and only the mode should decide that.
 */
export function ompApprovalFor(
  tool: string,
  projectRoot: () => string | undefined,
  prepare?: (args: unknown) => unknown,
): ((args: unknown) => OmpToolTier) | undefined {
  if (!(tool in OMP_TOOL_TIERS)) return undefined;
  return (args: unknown) => ompCallTier(tool, args, projectRoot(), prepare);
}
