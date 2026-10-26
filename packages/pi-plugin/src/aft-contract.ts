/**
 * AFT interoperability contract v1 (AFT side). See README.md "Interop
 * binding (OMP)" for the protocol this module implements; this file
 * documents only its own invariants.
 *
 * Answers `aft:binding-request { v: 1, sessionId, requestId }` on the
 * shared synchronous extension event bus with
 * `{ v: 1, sessionId, requestId, tools, project }`, mirroring the existing
 * `adr:binding-request` pattern. `tools` is read live from the host's own
 * registry, never a static list. `project` computes a call's real
 * filesystem targets; it never mutates a file or runs a model/shell
 * command. It may call the AFT bridge for a read-only preflight
 * (`hashline_preflight`, `undo_preview`, `checkpoint_paths`) when the real
 * tool already has one; starting the bridge process if one is not already
 * running for this `cwd` is allowed, and that call itself reads only.
 *
 * OMP-only: upstream Pi has no consumer for this event, and only OMP's
 * loader populates `sourceInfo.path` the way tool-inventory filtering here
 * relies on.
 */

import { stat } from "node:fs/promises";
import { isAbsolute, resolve } from "node:path";
import {
  coerceBoolean,
  coerceStringArray,
  prepareCanonicalEditArguments,
  prepareCanonicalPathArguments,
} from "@cortexkit/aft-bridge";
import type { ExtensionAPI, ExtensionContext } from "@earendil-works/pi-coding-agent";
import { TOOL_SOURCE_PATH } from "./aft-source-path.js";
import { detectPiHarness } from "./harness.js";
import { internalTarget } from "./omp-internal-urls.js";
import {
  bridgeFor,
  callBridge,
  callToolCallForSession,
  resolveSessionId,
} from "./tools/_shared.js";
import { resolvePathArg, splitSearchPathArg } from "./tools/hoisted.js";
import { asRecord, asString } from "./tools/render-helpers.js";
import type { PluginContext } from "./types.js";

export { TOOL_SOURCE_PATH } from "./aft-source-path.js";

export type AftCapability = "observation" | "mutation" | "execution" | "native-url";
export type AftProjectionKind = "observation" | "mutation" | "execution" | "native" | "unknown";

export interface AftProjection {
  kind: AftProjectionKind;
  paths: string[];
  broad: boolean;
}

export interface AftBindingTool {
  name: string;
  sourcePath: string;
  capabilities: AftCapability[];
}

export type AftProjectFn = (
  toolName: string,
  input: Record<string, unknown>,
  cwd: string,
) => Promise<AftProjection>;

export interface AftBindingResponse {
  v: 1;
  sessionId: string;
  requestId: string;
  tools: AftBindingTool[];
  project: AftProjectFn;
}

/**
 * Static per-tool capability sets. A tool's capability set never depends on
 * the specific call (e.g. `aft_safety` always carries both `observation` and
 * `mutation` because `list`/`history` are read-only while `undo`/`checkpoint`/
 * `restore`/`purge` mutate); which one applies to one call is what `project`
 * resolves. Only `read`, `write`, `edit`, `grep` and `bash` also carry
 * `native-url`: `withOmpInternalUrls` (`omp-internal-urls.ts`) wraps exactly
 * those five and nothing else.
 */
const TOOL_CAPABILITIES: Readonly<Record<string, readonly AftCapability[]>> = {
  read: ["observation", "native-url"],
  write: ["mutation", "native-url"],
  edit: ["mutation", "native-url"],
  grep: ["observation", "native-url"],
  bash: ["execution", "native-url"],
  powershell: ["execution"],
  aft_outline: ["observation"],
  aft_zoom: ["observation"],
  aft_search: ["observation"],
  aft_inspect: ["observation"],
  aft_callgraph: ["observation"],
  aft_conflicts: ["observation"],
  aft_import: ["mutation"],
  aft_safety: ["observation", "mutation"],
  aft_delete: ["mutation"],
  aft_move: ["mutation"],
  ast_grep_search: ["observation"],
  ast_grep_replace: ["observation", "mutation"],
  bash_status: ["observation"],
  bash_watch: ["observation"],
  bash_write: ["execution"],
  bash_kill: ["execution"],
};

/** Tools whose registration wraps `prepareArguments` with `withPathAliasPreparation`
 * (`tools/_shared.ts`), i.e. accept legacy/alternate path argument names that
 * must be normalized the same way before `project` reads them. `edit` is
 * handled separately (`prepareCanonicalEditArguments`) because it also
 * accepts hashline/native edit shapes. */
const PATH_ALIAS_TOOLS = new Set([
  "read",
  "write",
  "grep",
  "aft_search",
  "aft_outline",
  "aft_zoom",
  "aft_callgraph",
  "aft_conflicts",
  "aft_import",
  "aft_safety",
  "aft_delete",
  "aft_move",
]);

/** Five tools `withOmpInternalUrls` wraps; only these can resolve to a native target. */
const NATIVE_URL_CANDIDATE_TOOLS = new Set(["read", "write", "edit", "grep", "bash"]);

function isGithubResourcePath(value: unknown): boolean {
  return typeof value === "string" && (value.startsWith("issue://") || value.startsWith("pr://"));
}

function isUrlLike(value: unknown): boolean {
  return (
    typeof value === "string" &&
    (value.startsWith("http://") ||
      value.startsWith("https://") ||
      value.startsWith("issue://") ||
      value.startsWith("pr://"))
  );
}

/**
 * Resolve one path argument to an absolute path, always — unlike
 * `resolvePathArg` alone, which falls back to a possibly-relative expansion
 * when the target does not exist yet (the common case for a `write` target,
 * a move destination, or a file an edit would create). The contract requires
 * exact mutation targets resolved against the supplied `cwd`, so a
 * projection must never hand back a path a caller would have to re-resolve.
 */
async function resolveOne(cwd: string, target: string): Promise<string> {
  const resolved = await resolvePathArg(cwd, target);
  return isAbsolute(resolved) ? resolved : resolve(cwd, resolved);
}

async function resolveMany(cwd: string, targets: string[]): Promise<string[]> {
  return Promise.all(targets.map((target) => resolveOne(cwd, target)));
}

async function isDirectoryPath(path: string): Promise<boolean> {
  try {
    return (await stat(path)).isDirectory();
  } catch {
    return false;
  }
}

/** Dynamic/uninspectable effect with no enumerable targets: never presented as an
 * empty-but-safe mutation or observation (see the contract's "Projection" section). */
const UNKNOWN: AftProjection = { kind: "unknown", paths: [], broad: true };

function observation(paths: string[], broad: boolean): AftProjection {
  return { kind: "observation", paths, broad };
}
function mutation(paths: string[], broad: boolean): AftProjection {
  return { kind: "mutation", paths, broad };
}
function execution(paths: string[] = [], broad = true): AftProjection {
  return { kind: "execution", paths, broad };
}
function native(): AftProjection {
  return { kind: "native", paths: [], broad: false };
}

function responsePaths(response: Record<string, unknown>): string[] {
  return Array.isArray(response.paths)
    ? response.paths.filter((path): path is string => typeof path === "string" && path.length > 0)
    : [];
}

async function projectRead(cwd: string, input: Record<string, unknown>): Promise<AftProjection> {
  const pathArg = asString(input.path);
  if (pathArg === undefined) return UNKNOWN;
  if (isGithubResourcePath(pathArg) || isUrlLike(pathArg)) return observation([], false);
  const resolved = await resolveOne(cwd, pathArg);
  return observation([resolved], await isDirectoryPath(resolved));
}

async function projectWrite(cwd: string, input: Record<string, unknown>): Promise<AftProjection> {
  const pathArg = asString(input.path);
  if (pathArg === undefined) return UNKNOWN;
  // A GitHub comment target is a real, exactly-known mutation target, but not
  // a filesystem path: no `paths` entry exists for consumers to protect
  // because there is no project-tree location this call can touch.
  if (isGithubResourcePath(pathArg)) return mutation([], false);
  const resolved = await resolveOne(cwd, pathArg);
  return mutation([resolved], false);
}

async function projectEdit(
  ctx: PluginContext,
  cwd: string,
  input: Record<string, unknown>,
  extCtx: ExtensionContext | undefined,
): Promise<AftProjection> {
  if (ctx.hashlineEffective === true && typeof input.patch === "string") {
    try {
      const bridge = bridgeFor(ctx, cwd);
      const preflight = await callToolCallForSession(
        bridge,
        "hashline_preflight",
        { patch: input.patch },
        extCtx ? resolveSessionId(extCtx) : undefined,
        extCtx,
      );
      const affected = [
        ...(Array.isArray(preflight.affected_paths) ? preflight.affected_paths : []),
        ...(Array.isArray(preflight.mv_destinations) ? preflight.mv_destinations : []),
      ].filter((value): value is string => typeof value === "string" && value.length > 0);
      if (affected.length === 0) return UNKNOWN;
      return mutation(await resolveMany(cwd, affected), false);
    } catch {
      // A patch the engine cannot parse/preflight yet has no enumerable
      // targets from here; never guess, never claim empty-but-safe.
      return UNKNOWN;
    }
  }
  const pathArg = asString(input.path);
  if (pathArg === undefined) return UNKNOWN;
  if (isGithubResourcePath(pathArg)) return mutation([], false);
  const resolved = await resolveOne(cwd, pathArg);
  return mutation([resolved], false);
}

async function projectGrep(cwd: string, input: Record<string, unknown>): Promise<AftProjection> {
  const pathArg = asString(input.path);
  if (pathArg === undefined) return observation([cwd], true);
  const fragments = /\s/.test(pathArg) ? pathArg.trim().split(/\s+/).filter(Boolean) : [pathArg];
  const resolved = await resolveMany(cwd, fragments);
  if (resolved.length > 1) return observation(resolved, true);
  return observation(resolved, await isDirectoryPath(resolved[0]));
}

async function projectOutline(cwd: string, input: Record<string, unknown>): Promise<AftProjection> {
  const target = input.target;
  const filesMode = coerceBoolean(input.files) === true;
  const targets = Array.isArray(target)
    ? target.filter((t): t is string => typeof t === "string")
    : typeof target === "string"
      ? [target]
      : [];
  if (targets.length === 0) return UNKNOWN;
  const real = targets.filter((t) => !isUrlLike(t));
  if (real.length === 0) return observation([], false);
  const resolved = await resolveMany(cwd, real);
  const dirFlags = await Promise.all(resolved.map(isDirectoryPath));
  const broad = filesMode || resolved.length > 1 || dirFlags.some(Boolean);
  return observation(resolved, broad);
}

async function projectZoom(cwd: string, input: Record<string, unknown>): Promise<AftProjection> {
  const batch = Array.isArray(input.batch) ? input.batch : [];
  const targets = batch
    .map((entry) => asRecord(entry)?.target)
    .filter((t): t is string => typeof t === "string" && !isUrlLike(t) && !isGithubResourcePath(t));
  if (targets.length === 0) return observation([], false);
  const resolved = await resolveMany(cwd, targets);
  return observation(resolved, resolved.length > 1);
}

async function projectSearch(cwd: string, input: Record<string, unknown>): Promise<AftProjection> {
  // `path` selects a different project/folder root; it is never a
  // subdirectory filter (see reading.ts/semantic.ts docs), so the scope is
  // always the whole selected project — inherently broad.
  const pathArg = asString(input.path);
  const root = pathArg && pathArg.trim().length > 0 ? await resolveOne(cwd, pathArg) : cwd;
  return observation([root], true);
}

async function projectInspect(cwd: string, input: Record<string, unknown>): Promise<AftProjection> {
  const scope = input.scope;
  const values =
    scope === undefined
      ? []
      : (Array.isArray(scope) ? scope : [scope]).filter(
          (v): v is string => typeof v === "string" && v.length > 0,
        );
  if (values.length === 0) return observation([cwd], true);
  const resolved = await resolveMany(cwd, values);
  // scope entries are a file or a directory; treat every entry as an
  // ancestor scope since a directory scope reaches its descendants.
  return observation(resolved, true);
}

async function projectCallgraph(
  cwd: string,
  input: Record<string, unknown>,
): Promise<AftProjection> {
  const pathArg = asString(input.path);
  if (pathArg === undefined) return UNKNOWN;
  const toPathArg = asString(input.toPath);
  const targets = toPathArg ? [pathArg, toPathArg] : [pathArg];
  return observation(await resolveMany(cwd, targets), false);
}

async function projectConflicts(
  cwd: string,
  input: Record<string, unknown>,
): Promise<AftProjection> {
  const pathArg = asString(input.path);
  const resolved = pathArg && pathArg.trim().length > 0 ? await resolveOne(cwd, pathArg) : cwd;
  // Conflicts are always discovered from the repository's top level
  // regardless of `path` (see conflicts.ts docs): always whole-repo scope.
  return observation([resolved], true);
}

async function projectImport(cwd: string, input: Record<string, unknown>): Promise<AftProjection> {
  const pathArg = asString(input.path);
  if (pathArg === undefined) return UNKNOWN;
  return mutation([await resolveOne(cwd, pathArg)], false);
}

async function projectDelete(cwd: string, input: Record<string, unknown>): Promise<AftProjection> {
  const files = coerceStringArray(input.files);
  if (files.length === 0) return UNKNOWN;
  const recursive = coerceBoolean(input.recursive) === true;
  return mutation(await resolveMany(cwd, files), recursive);
}

async function projectMove(cwd: string, input: Record<string, unknown>): Promise<AftProjection> {
  const pathArg = asString(input.path);
  const destinationArg = asString(input.destination);
  if (pathArg === undefined || destinationArg === undefined) return UNKNOWN;
  const [source, destination] = await Promise.all([
    resolveOne(cwd, pathArg),
    resolveOne(cwd, destinationArg),
  ]);
  // A directory source relocates its whole subtree; a file source does not.
  return mutation([source, destination], await isDirectoryPath(source));
}

async function projectSafety(
  ctx: PluginContext,
  cwd: string,
  input: Record<string, unknown>,
  extCtx: ExtensionContext | undefined,
): Promise<AftProjection> {
  const op = asString(input.op);
  const pathArg = asString(input.path);
  const filesArg = coerceStringArray(input.files);
  const resolvedPath = pathArg ? await resolveOne(cwd, pathArg) : undefined;
  const resolvedFiles = filesArg.length > 0 ? await resolveMany(cwd, filesArg) : undefined;

  if (op === "list") return observation([], false);
  if (op === "history") return observation(resolvedPath ? [resolvedPath] : [], false);

  if (op === "undo") {
    // undo_preview is the authoritative source; never guess a partial
    // scope from the call's own arguments when the preview fails.
    try {
      const bridge = bridgeFor(ctx, cwd);
      const previewReq: Record<string, unknown> = {};
      if (resolvedPath) previewReq.file = resolvedPath;
      const preview = await callBridge(bridge, "undo_preview", previewReq, extCtx);
      return mutation(await resolveMany(cwd, responsePaths(preview)), false);
    } catch {
      return UNKNOWN;
    }
  }

  if (op === "checkpoint") {
    // Writes a durable snapshot record: a mutation of AFT's own store,
    // never an observation, however explicit its target files are. An
    // implicit whole-project snapshot has no enumerable scope from here.
    const files = resolvedFiles ?? (resolvedPath ? [resolvedPath] : undefined);
    return files ? mutation(files, false) : UNKNOWN;
  }

  if (op === "restore") {
    const name = asString(input.name);
    if (!name) return UNKNOWN;
    // checkpoint_paths is the authoritative source; never guess a partial
    // scope from the call's own path/files when the preview fails (`path`
    // can itself be a directory, so the raw argument alone proves nothing).
    try {
      const bridge = bridgeFor(ctx, cwd);
      const previewReq: Record<string, unknown> = { name };
      if (resolvedPath) previewReq.file = resolvedPath;
      if (resolvedFiles) previewReq.files = resolvedFiles;
      const preview = await callBridge(bridge, "checkpoint_paths", previewReq, extCtx);
      return mutation(await resolveMany(cwd, responsePaths(preview)), false);
    } catch {
      return UNKNOWN;
    }
  }

  if (op === "purge") {
    // Permanently removes undo/checkpoint copies of `path`; it can be a
    // directory, reaching every saved copy under it.
    return resolvedPath ? mutation([resolvedPath], true) : UNKNOWN;
  }

  return UNKNOWN;
}

async function astScope(
  cwd: string,
  input: Record<string, unknown>,
): Promise<{ paths: string[]; broad: boolean } | undefined> {
  const rawPaths = coerceStringArray(input.paths);
  if (rawPaths.length === 0) return { paths: [cwd], broad: true };
  const paths: string[] = [];
  let broad = coerceStringArray(input.globs).length > 0;
  for (const raw of rawPaths) {
    const split = await splitSearchPathArg(cwd, raw);
    if (split.missing.length > 0) return undefined;
    for (const target of split.paths) {
      const path = await resolveOne(cwd, target);
      try {
        // The engine requires every expanded root to exist; never admit a partial or fictitious scope.
        const directory = (await stat(path)).isDirectory();
        broad = broad || directory;
      } catch {
        return undefined;
      }
      paths.push(path);
    }
  }
  return { paths, broad };
}

async function projectAstSearch(
  cwd: string,
  input: Record<string, unknown>,
): Promise<AftProjection> {
  const scope = await astScope(cwd, input);
  return scope ? observation(scope.paths, scope.broad) : UNKNOWN;
}

async function projectAstReplace(
  cwd: string,
  input: Record<string, unknown>,
): Promise<AftProjection> {
  const scope = await astScope(cwd, input);
  if (!scope) return UNKNOWN;
  return coerceBoolean(input.dryRun) === true
    ? observation(scope.paths, scope.broad)
    : mutation(scope.paths, scope.broad);
}

/**
 * Project one AFT tool call's real filesystem targets without mutating
 * anything or running a model/shell command (calling the AFT bridge for a
 * read-only preflight, starting it if needed, is allowed). Reuses the same normalizers
 * (`prepareCanonicalPathArguments`/`prepareCanonicalEditArguments`) the real
 * registrations apply via `withPathAliasPreparation`, the same OMP
 * internal-URL detection the real registrations apply via
 * `withOmpInternalUrls` (`internalTarget`), and, where the real tool already
 * has a non-mutating preview RPC (`hashline_preflight`, `undo_preview`,
 * `checkpoint_paths`), that same preview — so a projected target always
 * agrees with what the real call would actually touch.
 */
export async function projectAftCall(
  ctx: PluginContext,
  toolName: string,
  rawInput: unknown,
  cwd: string,
  /**
   * The session's own ExtensionContext, when the caller has one bound,
   * passed through to read-only preflight RPCs (`hashline_preflight`,
   * `undo_preview`, `checkpoint_paths`) so a preview inspects the exact same
   * session-scoped undo/checkpoint stack the real mutation would use.
   * `registerAftBinding` always supplies the matching captured context;
   * direct callers of `projectAftCall` may omit it for a best-effort,
   * unscoped preview.
   */
  extCtx?: ExtensionContext,
): Promise<AftProjection> {
  const input = asRecord(rawInput) ?? {};

  if (ctx.ompRouter && NATIVE_URL_CANDIDATE_TOOLS.has(toolName)) {
    if (internalTarget(toolName, input, ctx.ompRouter) !== undefined) return native();
  }

  // The real `edit` tool registers one of two disjoint schemas depending on
  // `hashlineEffective` (`tool-registration.ts`'s hashline branch takes a bare
  // `{ patch }` and never runs it through `prepareCanonicalEditArguments`;
  // only the non-hashline branch does, via `withPathAliasPreparation`).
  // Normalizing a hashline patch through the non-hashline grammar would
  // reject every real hashline call as an unrecognized shape.
  let normalized: Record<string, unknown>;
  try {
    normalized =
      toolName === "edit"
        ? ctx.hashlineEffective === true
          ? input
          : ((prepareCanonicalEditArguments("edit", input) as Record<string, unknown>) ?? input)
        : PATH_ALIAS_TOOLS.has(toolName)
          ? ((prepareCanonicalPathArguments(toolName, input) as Record<string, unknown>) ?? input)
          : input;
  } catch {
    // The normalizer rejects this call's own shape (e.g. a required path
    // missing, an unrecognized key): no real tool call could execute it
    // either, so there is nothing to project but "unknown, refuse".
    return UNKNOWN;
  }

  switch (toolName) {
    case "read":
      return projectRead(cwd, normalized);
    case "write":
      return projectWrite(cwd, normalized);
    case "edit":
      return projectEdit(ctx, cwd, normalized, extCtx);
    case "grep":
      return projectGrep(cwd, normalized);
    case "bash":
    case "powershell":
      return execution();
    case "aft_outline":
      return projectOutline(cwd, normalized);
    case "aft_zoom":
      return projectZoom(cwd, normalized);
    case "aft_search":
      return projectSearch(cwd, normalized);
    case "aft_inspect":
      return projectInspect(cwd, normalized);
    case "aft_callgraph":
      return projectCallgraph(cwd, normalized);
    case "aft_conflicts":
      return projectConflicts(cwd, normalized);
    case "aft_import":
      return projectImport(cwd, normalized);
    case "aft_safety":
      return projectSafety(ctx, cwd, normalized, extCtx);
    case "aft_delete":
      return projectDelete(cwd, normalized);
    case "aft_move":
      return projectMove(cwd, normalized);
    case "ast_grep_search":
      return projectAstSearch(cwd, normalized);
    case "ast_grep_replace":
      return projectAstReplace(cwd, normalized);
    case "bash_status":
    case "bash_watch":
      return observation([], false);
    case "bash_write":
      return execution();
    case "bash_kill":
      return execution([], false);
    default:
      return UNKNOWN;
  }
}

interface HostToolEntry {
  name?: unknown;
  sourceInfo?: { source?: unknown; path?: unknown };
}

/**
 * The live set of tools this extension has actually registered for this host
 * process, read straight from the host's own registry (`getAllTools`), not a
 * static surface computation: a tool only appears here when the host
 * recorded its `sourceInfo.path` as this module's own entry path (so a
 * same-named tool another extension later overwrote drops out) and, when the
 * host also exposes `getActiveTools`, only while still active. Both reads
 * are best-effort: an older host without either method yields no tools,
 * which the binding responder treats as "cannot answer yet" rather than
 * publishing a stale or guessed list.
 */
function liveRegisteredTools(pi: ExtensionAPI): AftBindingTool[] {
  const api = pi as unknown as {
    getAllTools?: () => HostToolEntry[];
    getActiveTools?: () => Array<string | { name: string }>;
  };
  if (typeof api.getAllTools !== "function") return [];
  let allTools: HostToolEntry[];
  try {
    allTools = api.getAllTools();
  } catch {
    return [];
  }
  if (!Array.isArray(allTools)) return [];

  let activeNames: Set<string> | undefined;
  if (typeof api.getActiveTools === "function") {
    try {
      const active = api.getActiveTools();
      if (Array.isArray(active)) {
        activeNames = new Set(
          active.map((entry) => (typeof entry === "string" ? entry : entry.name)),
        );
      }
    } catch {
      activeNames = undefined;
    }
  }

  const tools: AftBindingTool[] = [];
  for (const entry of allTools) {
    const name = typeof entry.name === "string" ? entry.name : undefined;
    if (!name) continue;
    const capabilities = TOOL_CAPABILITIES[name];
    if (!capabilities) continue;
    if (entry.sourceInfo?.path !== TOOL_SOURCE_PATH) continue;
    if (activeNames && !activeNames.has(name)) continue;
    tools.push({ name, sourcePath: TOOL_SOURCE_PATH, capabilities: [...capabilities] });
  }
  return tools;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

/**
 * Install the `aft:binding-request` responder. Call during factory setup
 * (before any session event) so load order relative to consumer extensions
 * never matters; the responder itself answers lazily, from the host's live
 * registry, at request time — never from a snapshot taken at install time —
 * so late registration (e.g. the deferred `powershell` slot) stays current.
 *
 * A no-op outside the OMP harness: `sourceInfo.path`-keyed admission and the
 * only known consumers (ADR/Roadmap/omo-prometheus) are both OMP-only.
 *
 * Returns an unsubscribe function; also unsubscribes on `session_shutdown`.
 */
export function registerAftBinding(pi: ExtensionAPI, ctx: PluginContext): () => void {
  if (detectPiHarness(pi) !== "omp") return () => {};

  let latestContext: ExtensionContext | undefined;
  const capture = (_event: unknown, extCtx?: ExtensionContext): void => {
    if (extCtx) latestContext = extCtx;
  };
  // Requesters ask lazily, so any of these events capturing a session's
  // ExtensionContext before the first `aft:binding-request` is enough.
  // Deliberately NOT `tool_call`: registering any handler for it disables
  // OMP's speculative batch reads/tasks for the whole session.
  pi.on("session_start", capture);
  pi.on("session_tree", capture);
  pi.on("before_agent_start", capture);
  pi.on("agent_start", capture);
  pi.on("turn_start", capture);
  pi.on("input", capture);

  const subscription = pi.events.on("aft:binding-request", (payload: unknown) => {
    const extCtx = latestContext;
    if (!extCtx || !isRecord(payload) || payload.v !== 1) return;
    const sessionId = resolveSessionId(extCtx);
    if (
      typeof payload.sessionId !== "string" ||
      !sessionId ||
      payload.sessionId !== sessionId ||
      typeof payload.requestId !== "string" ||
      !payload.requestId
    ) {
      return;
    }
    const tools = liveRegisteredTools(pi);
    // No answer, not an empty-tools answer: the host's registry is not
    // readable yet (too early, or an older host), so staying silent lets the
    // requester retry rather than caching "AFT has nothing registered".
    if (tools.length === 0) return;
    const response: AftBindingResponse = {
      v: 1,
      sessionId: payload.sessionId,
      requestId: payload.requestId,
      tools,
      project: (toolName, input, cwd) => projectAftCall(ctx, toolName, input, cwd, extCtx),
    };
    pi.events.emit("aft:binding", response);
  });

  const unsubscribe = (): void => {
    latestContext = undefined;
    subscription();
  };
  pi.on("session_shutdown", unsubscribe);
  return unsubscribe;
}
