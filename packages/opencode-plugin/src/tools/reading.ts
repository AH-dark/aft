import {
  coerceBoolean,
  coerceJsonCollectionParam,
  coerceTargetParam,
  formatZoomText,
  isBlankParam,
  usableZoomTargets,
} from "@cortexkit/aft-bridge";
import type { ToolContext, ToolDefinition, ToolResult } from "@opencode-ai/plugin";
import { tool } from "@opencode-ai/plugin";
import { resolveGithubConfig, toolEnabled } from "../config.js";
import { prepareToolMap } from "../normalize-schemas.js";
import type { PluginContext } from "../types.js";
import {
  callToolCall,
  coerceOptionalInt,
  isEmptyParam,
  optionalInt,
  resolvePathArg,
  type ToolRuntime,
} from "./_shared.js";
import { whenGhReadEnabled } from "./hoisted.js";
import {
  assertExternalDirectoryPermission,
  assertV2ReadPermission,
  permissionDeniedResponse,
  permissionRuleDenial,
} from "./permissions.js";

const z = tool.schema;

/** GitHub discussion targets are fetched by the server, not read from disk. */
function isGithubPathArg(value: unknown): boolean {
  return typeof value === "string" && (value.startsWith("issue://") || value.startsWith("pr://"));
}

function buildZoomTitle(args: {
  path?: string;
  url?: string;
  symbols?: string | string[];
  targets?: { path: string; symbol: string } | Array<{ path: string; symbol: string }>;
}): string {
  const targets = args.targets;
  if (!isEmptyParam(targets)) {
    if (Array.isArray(targets)) {
      if (targets.length === 1) {
        return `${targets[0].path}#${targets[0].symbol}`;
      }
      return `${targets.length} targets across files`;
    }
    if (targets && typeof targets === "object") {
      return `${targets.path}#${targets.symbol}`;
    }
  }

  const path = args.path ?? args.url ?? "";
  if (typeof args.symbols === "string") return path ? `${path}#${args.symbols}` : args.symbols;
  if (Array.isArray(args.symbols) && args.symbols.length > 0) {
    if (args.symbols.length === 1) return path ? `${path}#${args.symbols[0]}` : args.symbols[0];
    return path ? `${path} (${args.symbols.length} symbols)` : `${args.symbols.length} symbols`;
  }
  return path || "(no target)";
}

interface ZoomBatchSymbolResult {
  name: string;
  success: boolean;
  content?: string;
  error?: string;
}

interface ZoomBatchResult {
  complete: boolean;
  symbols: ZoomBatchSymbolResult[];
  text: string;
}

/**
 * Tool definitions for code reading commands: outline + zoom.
 */
/**
 * The outline description's steer toward focused reading. It names
 * `aft_search`, `aft_zoom` and `aft_callgraph` only when each is registered,
 * falling back to `read` for symbol reading.
 */
function outlineFocusSteer(config: PluginContext["config"]): string {
  const zoomEnabled = toolEnabled(config, "aft_zoom");
  const reader = zoomEnabled ? "aft_zoom" : "read";
  const locate = toolEnabled(config, "aft_search") ? `aft_search + ${reader}` : reader;
  const callgraph = zoomEnabled
    ? toolEnabled(config, "aft_callgraph")
      ? " aft_zoom with `callgraph:true` gives one-level forward calls-out; use aft_callgraph only for reverse callers or multi-level traces."
      : " aft_zoom with `callgraph:true` gives one-level forward calls-out."
    : "";
  return `For understanding a specific feature, prefer ${locate} on named symbols; use aft_outline on a whole directory only for high-level structure mapping.${callgraph}`;
}

export function readingTools(ctx: PluginContext): Record<string, ToolDefinition> {
  const zoomEnabled = toolEnabled(ctx.config, "aft_zoom");
  const ghReadEnabled = resolveGithubConfig(ctx.config).read;
  const githubOutlineDescription = whenGhReadEnabled(
    ghReadEnabled,
    "GitHub issues and pull requests can be outlined with `issue://NUMBER` or `pr://NUMBER` (including `OWNER/REPO` forms).",
  );
  const githubZoomDescription = whenGhReadEnabled(
    ghReadEnabled,
    "GitHub issue and pull-request discussion ordinals can be zoomed with `path: issue://…` or `path: pr://…` and `symbols`.",
  );
  const tools = prepareToolMap({
    aft_outline: {
      description:
        "Structural outline of source code, documentation files, or remote URLs. For code, returns symbols (functions, classes, types) with line ranges. For Markdown and HTML, returns heading hierarchy. Use this to explore structure before reading specific sections with " +
        (zoomEnabled ? "aft_zoom" : "read") +
        ". With `files: true`, the outline is breadth-first with directory rollups; drill in by outlining a subdirectory. Rows show language, symbol count, and line count.\n\n" +
        outlineFocusSteer(ctx.config) +
        "\n\n" +
        "Pass a single `target`:\n" +
        "  • file path → outline that file (with signatures)\n" +
        "  • directory path → outline source files under it\n" +
        "  • URL (http:// or https://) → fetch and outline a remote HTML/Markdown document\n" +
        "  • array of paths → outline multiple files in one call; with files:true, every path must be a directory" +
        (githubOutlineDescription ? `\n\n${githubOutlineDescription}` : "") +
        "\n\nWhen a list is cut, the reply ends with `shown N of M <unit> (<reason>) · narrow: <knobs>`; absence of that line means the list is complete.",
      args: {
        target: z
          .union([z.string(), z.array(z.string())])
          .describe(
            "What to outline: a file path, directory path, URL, or array of paths. The mode is auto-detected: URLs by `http://`/`https://` prefix, directories by stat, arrays as multi-file.",
          ),
        files: z
          .boolean()
          .optional()
          .describe(
            "Directory-only mode: when true, target must be a directory or array of directories and the result is a breadth-first file tree with directory rollups plus language, symbol, and line counts.",
          ),
        includeTests: z
          .boolean()
          .optional()
          .describe(
            "Directory outline only: include test files. Defaults to false; tests are hidden.",
          ),
      },
      execute: async (args, context): Promise<string> => {
        // Coerce at the boundary: a host may deliver the string|array `target` as
        // a JSON-stringified array, which would otherwise be treated as one
        // literal path (coerceTargetParam). And a stringified "true" must enable
        // files mode (coerceBoolean).
        const target = coerceTargetParam(args.target);
        const filesMode = coerceBoolean(args.files);
        const hasIncludeTests = !isEmptyParam(args.includeTests);
        const includeTests = coerceBoolean(args.includeTests);
        const rawArgs: Record<string, unknown> = {
          target,
          ...(filesMode ? { files: true } : {}),
          ...(hasIncludeTests ? { includeTests } : {}),
        };

        const checkedReads = new Map<string, string | undefined>();
        let directoryOutline = filesMode;
        if (Array.isArray(target)) {
          if (target.length === 0) {
            throw new Error("'target' must be a non-empty string or array of strings");
          }
          const resolvedTargets = await Promise.all(
            target.map((entry) => resolvePathArg(ctx, context, entry)),
          );
          directoryOutline ||= (
            await Promise.all(resolvedTargets.map(permissionKindForPath))
          ).includes("directory");
          const permissionDenied = await assertPathReadPermissions(
            ctx,
            context,
            resolvedTargets,
            filesMode ? "directory" : "file",
            checkedReads,
          );
          if (permissionDenied) return permissionDeniedResponse(permissionDenied);
        } else {
          if (typeof target !== "string" || target.length === 0) {
            throw new Error("'target' must be a non-empty string or array of strings");
          }

          const hasUrl =
            !filesMode &&
            (target.startsWith("http://") ||
              target.startsWith("https://") ||
              target.startsWith("issue://") ||
              target.startsWith("pr://"));
          if (!hasUrl) {
            const resolvedTarget = await resolvePathArg(ctx, context, target);
            const kind = await permissionKindForPath(resolvedTarget);
            directoryOutline ||= kind === "directory";
            const permissionDenied = await assertPathReadPermissions(
              ctx,
              context,
              resolvedTarget,
              kind,
              checkedReads,
            );
            if (permissionDenied) return permissionDeniedResponse(permissionDenied);
          }
        }

        const excluded = new Set<string>();
        for (;;) {
          const response = await callToolCall(ctx, context, "outline", {
            ...rawArgs,
            ...(excluded.size ? { _exclude_read_paths: [...excluded] } : {}),
          });
          if (response.success === false)
            throw new Error((response.message as string) || "outline failed");
          if ((context as ToolRuntime).hostVersion !== 2 || !directoryOutline) return response.text;
          // Directory results are held until every contributing file is authorized.
          // Exclusions can reveal more rows under the output budget, so check each render.
          const contentFiles = response.content_files;
          if (!Array.isArray(contentFiles)) {
            if (!directoryOutline) return response.text;
            throw new Error(
              "OpenCode 2 outline requires server content_files for read authorization",
            );
          }
          const distinctFiles = new Set(contentFiles as string[]);
          let denied = 0;
          for (const file of distinctFiles) {
            if (!checkedReads.has(file))
              checkedReads.set(file, await assertV2ReadPermission(context, file));
            const denial = checkedReads.get(file);
            if (!denial) continue;
            if (denial !== permissionRuleDenial("read")) return permissionDeniedResponse(denial);
            if (excluded.has(file))
              throw new Error("Outline server did not exclude a read-denied file");
            excluded.add(file);
            denied++;
          }
          if (denied) {
            if (distinctFiles.size === denied)
              return permissionDeniedResponse(permissionRuleDenial("read"));
            continue;
          }
          return (
            response.text +
            (excluded.size ? `\n${excluded.size} files left out by read rules.` : "")
          );
        }
      },
    },

    aft_zoom: {
      description:
        "Inspect code symbols or documentation sections. For code, returns the full source of a symbol. Pass `callgraph: true` to also include call-graph annotations (calls-out / called-by within the same file). For Markdown and HTML, returns the section content under the given heading.\n\nUse `{ path, symbols }` or `{ url, symbols }` for one file/URL. `symbols` can be a string or array (one or many lookups in the same file/URL). Use `targets` for cross-file batches: `{ path, symbol }` or an array of them. Sending both merges them into one batch; a lookup that fails reports its own error line." +
        (githubZoomDescription ? `\n\n${githubZoomDescription}` : ""),
      args: {
        path: z.string().optional().describe("Path to file (absolute or relative to project root)"),
        url: z
          .string()
          .optional()
          .describe("HTTP/HTTPS URL of an HTML or Markdown document to fetch and zoom into"),
        symbols: z
          .union([z.string(), z.array(z.string())])
          .optional()
          .describe(
            "Symbol name for code, or heading text for Markdown/HTML. Pass a string for one lookup or an array for batched lookups in the same file/URL.",
          ),
        targets: z
          .union([
            z.object({
              path: z.string().describe("Path to file (absolute or relative to project root)"),
              symbol: z.string().describe("Symbol name in that file"),
            }),
            z.array(
              z.object({
                path: z.string().describe("Path to file (absolute or relative to project root)"),
                symbol: z.string().describe("Symbol name in that file"),
              }),
            ),
          ])
          .optional()
          .describe(
            "Cross-file batch: `{ path, symbol }` or an array of them. May be combined with path/url + symbols; all lookups are answered in one batch.",
          ),
        contextLines: optionalInt(1, Number.MAX_SAFE_INTEGER).describe(
          "Lines of context before/after the symbol (default: 3)",
        ),
        callgraph: z
          .boolean()
          .optional()
          .describe(
            "Include call-graph annotations (calls-out / called-by within the same file). Default false; off keeps zoom output minimal.",
          ),
      },
      execute: async (args, context): Promise<ToolResult> => {
        // Some models fill every declared property on every call, sending
        // empty strings / arrays / objects (or whitespace) for the ones they
        // do not mean. Blank values count as absent, and `targets` entries
        // whose path and symbol are both blank are dropped as placeholders.
        const targetsInput = coerceJsonCollectionParam(args.targets, "targets");
        const targetEntries = usableZoomTargets(targetsInput);
        const hasFilePath = !isBlankParam(args.path);
        const hasUrl = !isBlankParam(args.url);
        const hasTargets = targetEntries.length > 0;
        const hasSymbols = !isBlankParam(args.symbols);
        // Coerce at the boundary: stringified "true" must request callgraph (coerceBoolean).
        const wantCallgraph = coerceBoolean(args.callgraph);
        const contextLines = coerceOptionalInt(
          args.contextLines,
          "contextLines",
          1,
          Number.MAX_SAFE_INTEGER,
        );

        // TUI title + scalar metadata for the tool-call header. OpenCode's UI
        // only auto-renders SCALAR args (strings, numbers, booleans) — arrays
        // and objects are dropped from the `[key=value, ...]` line. Stringify
        // collection-shaped args here so `targets`/`symbols` stay visible.
        // Attached to every successful return via `withMeta`. (Error paths
        // can't carry a title: OpenCode skips `tool.execute.after` when execute
        // throws, and the plugin `context.metadata()` callback is unbridged, so
        // the return value is the only channel that survives.)
        const zoomTitle = buildZoomTitle({
          ...args,
          targets: (hasTargets ? targetEntries : undefined) as Parameters<
            typeof buildZoomTitle
          >[0]["targets"],
        });
        const zoomDisplay: Record<string, unknown> = { title: zoomTitle };
        if (hasFilePath) zoomDisplay.path = args.path;
        if (hasUrl) zoomDisplay.url = args.url;
        if (hasSymbols) {
          zoomDisplay.symbols =
            typeof args.symbols === "string" ? args.symbols : JSON.stringify(args.symbols);
        }
        if (hasTargets) zoomDisplay.targets = JSON.stringify(targetEntries);
        if (contextLines !== undefined) zoomDisplay.contextLines = contextLines;
        if (wantCallgraph) zoomDisplay.callgraph = true;
        const withMeta = (output: string): ToolResult => ({
          output,
          title: zoomTitle,
          metadata: zoomDisplay,
        });

        // Cross-file batch. `path`/`url` + `symbols` sent alongside `targets`
        // join the same batch instead of being refused: the server answers
        // every lookup and reports each one that fails on its own line.
        if (hasTargets) {
          if (hasFilePath && hasUrl) {
            throw new Error("Provide exactly ONE of 'path' or 'url' — not both");
          }
          const targets = targetEntries.map((entry, i) => {
            if (!entry || typeof entry !== "object" || Array.isArray(entry)) {
              throw new Error(`targets[${i}].path must be a non-empty string`);
            }
            const record = entry as Record<string, unknown>;
            const targetPath = isBlankParam(record.path) ? record.filePath : record.path;
            return {
              path: typeof targetPath === "string" ? targetPath : "",
              symbol: typeof record.symbol === "string" ? record.symbol : "",
            };
          });
          const localPaths = targets
            .map((target) => target.path)
            .filter((path) => path.trim().length > 0 && !isGithubPathArg(path));
          if (hasFilePath && !isGithubPathArg(args.path)) localPaths.push(args.path as string);
          const resolvedTargets = await Promise.all(
            localPaths.map((path) => resolvePathArg(ctx, context, path)),
          );
          const deniedPaths = new Map<string, string>();
          const resolvedByInput = new Map(
            localPaths.map((input, index) => [input, resolvedTargets[index]]),
          );
          const isDenied = (input: string) => {
            const resolved = resolvedByInput.get(input);
            return resolved !== undefined && deniedPaths.has(resolved);
          };
          for (const file of new Set(resolvedTargets)) {
            const denial = await assertPathReadPermissions(ctx, context, file);
            if (denial) {
              if ((context as ToolRuntime).hostVersion !== 2)
                return permissionDeniedResponse(denial);
              deniedPaths.set(file, denial);
            }
          }

          const rawArgs: Record<string, unknown> = {
            targets: targets
              .filter((target) => !isDenied(target.path))
              .map((target) => ({
                filePath: target.path,
                symbol: target.symbol,
              })),
          };
          const deniedPrimary = hasFilePath && isDenied(args.path as string);
          if (!deniedPrimary) {
            if (hasFilePath) rawArgs.filePath = args.path;
            else if (hasUrl) rawArgs.url = args.url;
          }
          if (hasSymbols && !deniedPrimary) rawArgs.symbols = args.symbols;
          if (contextLines !== undefined) rawArgs.contextLines = contextLines;
          if (wantCallgraph) rawArgs.callgraph = true;

          const deniedText = [...deniedPaths]
            .map(([file, denial]) => `${file}: ${denial}`)
            .join("\n");
          if (!(rawArgs.targets as unknown[]).length && !(hasFilePath && !deniedPrimary) && !hasUrl)
            return withMeta(deniedText);
          const response = await callToolCall(ctx, context, "zoom", rawArgs);
          if (response.success === false) {
            throw new Error(response.text || response.message || "zoom failed");
          }
          return withMeta([response.text, deniedText].filter(Boolean).join("\n\n"));
        }

        if (!hasFilePath && !hasUrl) {
          throw new Error("Provide exactly one of 'path', 'url', or 'targets'");
        }
        if (hasFilePath && hasUrl) {
          throw new Error("Provide exactly ONE of 'path' or 'url' — not both");
        }

        // URL mode passes through to Rust; Rust fetches, validates, and caches.
        // File mode still resolves locally before dispatch so external-directory
        // permission checks approve the same path the server will read.
        const githubPath =
          (hasFilePath && isGithubPathArg(args.path)) || (hasUrl && isGithubPathArg(args.url));
        if (!hasUrl && !githubPath) {
          const file = await resolvePathArg(ctx, context, args.path as string);
          const permissionDenied = await assertPathReadPermissions(ctx, context, file);
          if (permissionDenied) return permissionDeniedResponse(permissionDenied);
        }

        const rawArgs: Record<string, unknown> = hasUrl
          ? { url: args.url }
          : { filePath: args.path };
        if (hasSymbols) rawArgs.symbols = args.symbols;
        if (contextLines !== undefined) rawArgs.contextLines = contextLines;
        if (wantCallgraph) rawArgs.callgraph = true;

        const response = await callToolCall(ctx, context, "zoom", rawArgs);
        if (response.success === false) {
          throw new Error(response.text || response.message || "zoom failed");
        }
        return withMeta(response.text);
      },
    },
  });
  return tools;
}

/**
 * Format multi-symbol zoom results as plain text. Successful entries use
 * `formatZoomText` (line-numbered, no JSON escapes); failures render as
 * `Symbol "name" not found: <reason>`. Sections are blank-line separated.
 *
 * Exported for regression tests.
 */
export function formatZoomBatchResult(
  targetLabel: string,
  symbols: string[],
  responses: Record<string, unknown>[],
): ZoomBatchResult {
  const entries = symbols.map((name, index): ZoomBatchSymbolResult => {
    const response = responses[index] ?? { success: false, message: "missing zoom response" };
    if (response.success === false) {
      const message =
        typeof response.message === "string" && response.message.length > 0
          ? response.message
          : "zoom failed";
      return { name, success: false, error: message };
    }
    return { name, success: true, content: formatZoomText(targetLabel, response) };
  });
  const complete = entries.every((entry) => entry.success);
  const sections: string[] = [];
  if (!complete) {
    sections.push("Incomplete zoom results: one or more symbols failed.");
  }
  for (const entry of entries) {
    if (entry.success) {
      sections.push(entry.content ?? "");
    } else {
      sections.push(`Symbol "${entry.name}" not found: ${entry.error ?? "zoom failed"}`);
    }
  }
  return { complete, symbols: entries, text: sections.join("\n\n") };
}

async function permissionKindForPath(resolvedPath: string): Promise<"file" | "directory"> {
  try {
    const { stat } = await import("node:fs/promises");
    const st = await stat(resolvedPath);
    return st.isDirectory() ? "directory" : "file";
  } catch {
    // If stat fails, keep the tool call moving so the server can report the
    // real path error. Use a file label because a missing path cannot be a
    // directory that the permission prompt could grant.
    return "file";
  }
}

async function assertPathReadPermissions(
  ctx: PluginContext,
  context: ToolContext,
  target: string | string[],
  kind: "file" | "directory" = "file",
  checkedReads = new Map<string, string | undefined>(),
): Promise<string | undefined> {
  const targets = Array.isArray(target) ? target : [target];
  const checked = new Set<string>();

  for (const resolvedPath of targets) {
    if (typeof resolvedPath !== "string" || resolvedPath.length === 0) continue;
    const key = `${kind}:${resolvedPath}`;
    if (checked.has(key)) continue;
    checked.add(key);

    const denial = await assertExternalDirectoryPermission(ctx, context, resolvedPath, {
      kind,
      serverValidatedRead: true,
    });
    if (denial) return denial;
    if (kind === "file" && (await permissionKindForPath(resolvedPath)) === "file") {
      if (!checkedReads.has(resolvedPath))
        checkedReads.set(resolvedPath, await assertV2ReadPermission(context, resolvedPath));
      const readDenial = checkedReads.get(resolvedPath);
      if (readDenial) return readDenial;
    }
  }

  return undefined;
}
