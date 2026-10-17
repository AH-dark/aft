/**
 * AFT reading tools: aft_outline + aft_zoom.
 * Structural overview and symbol/section inspection.
 */

import {
  coerceBoolean,
  coerceJsonCollectionParam,
  coerceTargetParam,
  isBlankParam,
  usableZoomTargets,
} from "@cortexkit/aft-bridge";
import type {
  AgentToolResult,
  ExtensionAPI,
  ExtensionContext,
  Theme,
} from "@earendil-works/pi-coding-agent";
import { type Static, Type } from "typebox";
import { resolveGithubConfig, toolEnabled } from "../config.js";
import type { PluginContext } from "../types.js";
import {
  bridgeFor,
  callToolCall,
  coerceOptionalInt,
  isEmptyParam,
  optionalInt,
  textResult,
  withPathAliasPreparation,
} from "./_shared.js";
import { assertExternalReadPermission, resolvePathArg, whenGhReadEnabled } from "./hoisted.js";
import {
  accentPath,
  asRecord,
  asRecordOrEmpty,
  asRecords,
  asString,
  collapsibleResult,
  collectTextContent,
  extractStructuredPayload,
  type RenderContextLike,
  type RenderResultOptionsLike,
  renderErrorResult,
  renderSections,
  renderToolCall,
  shortenPath,
} from "./render-helpers.js";

/** GitHub discussion targets are fetched by the server, not read from disk. */
function isGithubPathArg(value: unknown): boolean {
  return typeof value === "string" && (value.startsWith("issue://") || value.startsWith("pr://"));
}

const OutlineParams = Type.Object({
  target: Type.Union([Type.String(), Type.Array(Type.String())], {
    description:
      "What to outline: a file path, directory path, URL (http:// or https://), or array of file paths. The mode is auto-detected: URLs by `http://`/`https://` prefix, directories by stat, arrays as multi-file.",
  }),
  files: Type.Optional(
    Type.Boolean({
      description:
        "Directory-only mode: when true, target must be a directory or array of directories and the result is a breadth-first file tree with directory rollups plus language, symbol, and line counts.",
    }),
  ),
  includeTests: Type.Optional(
    Type.Boolean({
      description:
        "Directory outline only: include test files. Defaults to false; tests are hidden.",
    }),
  ),
});

const ZoomTarget = Type.Object({
  path: Type.String({ description: "Path to file (absolute or project-relative)" }),
  symbol: Type.String({ description: "Symbol name in that file" }),
});

const ZoomParams = Type.Object({
  path: Type.Optional(Type.String({ description: "Path to file (absolute or project-relative)" })),
  url: Type.Optional(
    Type.String({
      description: "HTTP/HTTPS URL of an HTML or Markdown document to fetch and zoom into",
    }),
  ),
  symbols: Type.Optional(
    Type.Union([Type.String(), Type.Array(Type.String())], {
      description:
        "Symbol name for code, or heading text for Markdown/HTML. Pass a string or array for batched lookups in the same file/URL.",
    }),
  ),
  targets: Type.Optional(
    Type.Union([ZoomTarget, Type.Array(ZoomTarget)], {
      description:
        "Cross-file batch: `{ path, symbol }` or an array of them. May be combined with path/url + symbols; all lookups are answered in one batch.",
    }),
  ),
  contextLines: optionalInt(
    1,
    Number.MAX_SAFE_INTEGER,
    "Lines of context before/after (default: 3)",
  ),
  callgraph: Type.Optional(
    Type.Boolean({
      description:
        "Include call-graph annotations (calls-out / called-by within the same file). Default false; off keeps zoom output minimal.",
    }),
  ),
});

function isUrl(s: string): boolean {
  return (
    s.startsWith("http://") ||
    s.startsWith("https://") ||
    s.startsWith("issue://") ||
    s.startsWith("pr://")
  );
}

async function assertReadPathPermissions(
  extCtx: ExtensionContext,
  ctx: PluginContext,
  paths: string | string[],
): Promise<void> {
  const targets = Array.isArray(paths) ? paths : [paths];
  const checked = new Set<string>();
  for (const target of targets) {
    if (!target || checked.has(target)) continue;
    checked.add(target);
    await assertExternalReadPermission(ctx, extCtx, target);
  }
}

/** Best-effort label for renderers when zoom is called with `path` OR `url`. */
function zoomTargetLabel(args: unknown): string {
  const safeArgs = asRecordOrEmpty(args);
  return asString(safeArgs.path) ?? asString(safeArgs.url) ?? "(no target)";
}

export interface ReadingSurface {
  outline: boolean;
  zoom: boolean;
}

function zoomExtraCallSites(call: Record<string, unknown>): string {
  const extraCount = call.extra_count;
  return typeof extraCount === "number" && Number.isInteger(extraCount) && extraCount > 0
    ? ` +${extraCount}`
    : "";
}

/** Exported for renderer unit tests. */
export function buildOutlineSections(text: string, theme: Theme): string[] {
  const trimmed = text.trim();
  if (!trimmed) return [theme.fg("muted", "No outline available.")];

  const lines = trimmed.split("\n");
  if (lines.length === 1) return [theme.fg("accent", lines[0])];
  return [theme.fg("accent", lines[0]), lines.slice(1).join("\n")];
}

function renderZoomRecord(
  record: Record<string, unknown>,
  targetLabel: string,
  theme: Theme,
): string {
  const name = asString(record.name) ?? "(unknown symbol)";
  const kind = asString(record.kind) ?? "symbol";
  const range = asRecord(record.range);
  const startLine = range && typeof range.start_line === "number" ? range.start_line : undefined;
  const endLine = range && typeof range.end_line === "number" ? range.end_line : undefined;
  const location =
    startLine !== undefined
      ? `${shortenPath(targetLabel)}:${startLine}${endLine && endLine !== startLine ? `-${endLine}` : ""}`
      : shortenPath(targetLabel);
  const lines = [`${theme.fg("accent", name)} ${theme.fg("muted", `[${kind}] ${location}`)}`];

  const content = asString(record.content);
  if (content) {
    lines.push(
      content
        .split("\n")
        .map((line) => `  ${line}`)
        .join("\n"),
    );
  }

  const annotations = asRecord(record.annotations);
  const callsOut = annotations ? asRecords(annotations.calls_out) : [];
  const calledBy = annotations ? asRecords(annotations.called_by) : [];
  if (callsOut.length > 0) {
    lines.push(
      `${theme.fg("muted", "calls out")}`,
      callsOut
        .map(
          (call) =>
            `  ↳ ${asString(call.name) ?? "(unknown)"}${typeof call.line === "number" ? `:${call.line}` : ""}${zoomExtraCallSites(call)}`,
        )
        .join("\n"),
    );
  }
  if (calledBy.length > 0) {
    lines.push(
      `${theme.fg("muted", "called by")}`,
      calledBy
        .map(
          (call) =>
            `  ↳ ${asString(call.name) ?? "(unknown)"}${typeof call.line === "number" ? `:${call.line}` : ""}${zoomExtraCallSites(call)}`,
        )
        .join("\n"),
    );
  }

  return lines.join("\n");
}

/** Exported for renderer unit tests. */
export function buildZoomSections(args: unknown, payload: unknown, theme: Theme): string[] {
  const safeArgs = asRecordOrEmpty(args);
  const batch = asRecord(payload);
  const batchItems = Array.isArray(batch?.targets)
    ? (batch.targets as unknown[])
    : Array.isArray(batch?.symbols)
      ? (batch.symbols as unknown[])
      : Array.isArray(batch?.entries)
        ? (batch.entries as unknown[])
        : null;
  if (batchItems) {
    const header =
      batch?.complete === false ? [theme.fg("warning", "Incomplete zoom results")] : [];
    return [
      ...header,
      ...batchItems.map((item) => {
        const record = asRecord(item);
        if (!record) return theme.fg("muted", "No zoom result available.");
        const response = asRecord(record.response) ?? record;
        const name = asString(record.name) ?? asString(response.name) ?? "(unknown symbol)";
        const itemTargetLabel = asString(record.targetLabel) ?? zoomTargetLabel(safeArgs);
        if (response.success === false || record.success === false) {
          const location = record.targetLabel ? ` in ${shortenPath(itemTargetLabel)}` : "";
          return theme.fg(
            "error",
            `Symbol "${name}" not found${location}: ${asString(response.message) ?? asString(record.error) ?? "zoom failed"}`,
          );
        }
        if (record.response) {
          return renderZoomRecord(
            { ...response, name: asString(response.name) ?? name },
            itemTargetLabel,
            theme,
          );
        }
        const content = asString(record.content);
        if (content) {
          return [
            `${theme.fg("accent", name)} ${theme.fg("muted", shortenPath(itemTargetLabel))}`,
            content,
          ]
            .filter(Boolean)
            .join("\n");
        }
        return renderZoomRecord(record, itemTargetLabel, theme);
      }),
    ];
  }

  const items = Array.isArray(payload) ? payload : payload ? [payload] : [];
  if (items.length === 0) return [theme.fg("muted", "No zoom result available.")];

  return items
    .map((item) => {
      const record = asRecord(item);
      if (!record) return theme.fg("muted", "No zoom result available.");
      return renderZoomRecord(record, zoomTargetLabel(safeArgs), theme);
    })
    .filter(Boolean);
}

/** Exported for renderer unit tests. */
export function renderOutlineCall(args: unknown, theme: Theme, context: RenderContextLike) {
  const safeArgs = asRecordOrEmpty(args);
  const target = safeArgs.target;
  const files = safeArgs.files === true;
  const summary = Array.isArray(target)
    ? theme.fg("accent", `${target.length} ${files ? "directories" : "files"}`)
    : typeof target === "string"
      ? `${accentPath(theme, target)}${files ? " files" : ""}`
      : undefined;
  return renderToolCall("outline", summary, theme, context);
}

/** Exported for renderer unit tests. */
export function renderOutlineResult(
  result: AgentToolResult<unknown>,
  theme: Theme,
  context: RenderContextLike,
  options: RenderResultOptionsLike = { expanded: true },
) {
  if (context.isError) return renderErrorResult(result, "outline failed", theme, context);
  const sections = buildOutlineSections(collectTextContent(result), theme);
  return collapsibleResult({
    summary: sections[0] ?? "outline completed",
    full: renderSections(sections, context),
    expanded: options.expanded,
    context,
  });
}

/** Exported for renderer unit tests. */
export function renderZoomCall(args: unknown, theme: Theme, context: RenderContextLike) {
  const safeArgs = asRecordOrEmpty(args);
  const hasExplicitTarget = typeof safeArgs.path === "string" || typeof safeArgs.url === "string";
  // `symbols` accepts string OR string[]; renderer adapts to both shapes.
  const symbols = safeArgs.symbols;
  const targets = safeArgs.targets;
  let summary: string;
  if (typeof symbols === "string") {
    summary = theme.fg("toolOutput", symbols);
  } else if (Array.isArray(symbols) && symbols.length > 0) {
    summary = theme.fg("toolOutput", `${symbols.length} symbols`);
  } else if (Array.isArray(targets) && targets.length > 0) {
    summary = theme.fg("toolOutput", `${targets.length} targets`);
  } else if (targets && typeof targets === "object" && !Array.isArray(targets)) {
    summary = theme.fg("toolOutput", asString(asRecord(targets)?.symbol) ?? "1 target");
  } else if (hasExplicitTarget) {
    summary = theme.fg("toolOutput", "lines");
  } else {
    summary = "";
  }
  const hasKnownTarget =
    hasExplicitTarget ||
    typeof symbols === "string" ||
    Array.isArray(symbols) ||
    Array.isArray(targets) ||
    asRecord(targets) !== undefined;
  const display = [
    hasKnownTarget ? accentPath(theme, zoomTargetLabel(safeArgs)) : undefined,
    summary || undefined,
  ]
    .filter(Boolean)
    .join(" ");
  return renderToolCall("zoom", display, theme, context);
}

/** Exported for renderer unit tests. */
export function renderZoomResult(
  result: AgentToolResult<unknown>,
  args: Static<typeof ZoomParams>,
  theme: Theme,
  context: RenderContextLike,
  options: RenderResultOptionsLike = { expanded: true },
) {
  if (context.isError) return renderErrorResult(result, "zoom failed", theme, context);
  const sections = buildZoomSections(args, extractStructuredPayload(result), theme);
  return collapsibleResult({
    summary: sections[0] ?? "zoom completed",
    full: renderSections(sections, context),
    expanded: options.expanded,
    context,
  });
}

/**
 * The outline description's steer toward focused reading. It names
 * `aft_search`, `aft_zoom` and `aft_callgraph` only when each is registered,
 * falling back to `read` for symbol reading. Kept identical to the OpenCode copy.
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

export function registerReadingTools(
  pi: ExtensionAPI,
  ctx: PluginContext,
  surface: ReadingSurface,
): void {
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
  if (surface.outline) {
    const outlineTool = withPathAliasPreparation({
      name: "aft_outline",
      label: "outline",
      description: `Structural outline of source code, documentation files, or remote URLs. For code, returns symbols (functions, classes, types) with line ranges. For Markdown and HTML, returns heading hierarchy. Use this to explore structure before reading specific sections with ${zoomEnabled ? "aft_zoom" : "read"}. With \`files: true\`, the outline is breadth-first with directory rollups; drill in by outlining a subdirectory. Rows show language, symbol count, and line count.\n\n${outlineFocusSteer(ctx.config)}\n\nPass a single \`target\`:\n  • file path → outline that file (with signatures)\n  • directory path → outline source files under it\n  • URL (http:// or https://) → fetch and outline a remote HTML/Markdown document\n  • array of paths → outline multiple files in one call; with files:true, every path must be a directory${githubOutlineDescription ? `\n\n${githubOutlineDescription}` : ""}\n\nWhen a list is cut, the reply ends with \`shown N of M <unit> (<reason>) · narrow: <knobs>\`; absence of that line means the list is complete.`,
      parameters: OutlineParams,
      async execute(
        _toolCallId: string,
        params: Static<typeof OutlineParams>,
        _signal,
        _onUpdate,
        extCtx,
      ) {
        const bridge = bridgeFor(ctx, extCtx.cwd);
        // Coerce at the boundary: a host may deliver the string|array `target` as
        // a JSON-stringified array, which would otherwise be treated as one
        // literal path (coerceTargetParam). And a stringified "true" must enable
        // files mode (coerceBoolean).
        const target = coerceTargetParam(params.target);
        const filesMode = coerceBoolean(params.files);
        const hasIncludeTests = !isEmptyParam(params.includeTests);
        const includeTests = coerceBoolean(params.includeTests);
        const rawArgs: Record<string, unknown> = {
          target,
          ...(filesMode ? { files: true } : {}),
          ...(hasIncludeTests ? { includeTests } : {}),
        };

        if (Array.isArray(target)) {
          if (target.length === 0) {
            throw new Error("'target' must be a non-empty string or array of strings");
          }
          const resolvedTargets = await Promise.all(
            (target as string[]).map((entry) => resolvePathArg(extCtx.cwd, entry)),
          );
          await assertReadPathPermissions(extCtx, ctx, resolvedTargets);
        } else {
          if (typeof target !== "string" || target.length === 0) {
            throw new Error("'target' must be a non-empty string or array of strings");
          }
          if (!(!filesMode && isUrl(target))) {
            const resolvedTarget = await resolvePathArg(extCtx.cwd, target);
            await assertReadPathPermissions(extCtx, ctx, resolvedTarget);
          }
        }

        const response = await callToolCall(bridge, "outline", rawArgs, extCtx);
        if (response.success === false) {
          throw new Error(response.text || response.message || "outline failed");
        }
        let text = typeof response.text === "string" ? response.text : "";
        const trimmed = text.trim();
        if (trimmed.startsWith("{") && trimmed.endsWith("}")) {
          try {
            const parsed = JSON.parse(text);
            if (typeof parsed.text === "string") {
              text = parsed.text;
            }
          } catch {
            // keep original text
          }
        }
        return textResult(text, response);
      },
      renderCall(args, theme, context) {
        return renderOutlineCall(args, theme, context);
      },
      renderResult(result, options = { expanded: false, isPartial: false }, theme, context) {
        return renderOutlineResult(result, theme, context, options);
      },
    });
    pi.registerTool(outlineTool);
  }

  if (surface.zoom) {
    pi.registerTool(
      withPathAliasPreparation({
        name: "aft_zoom",
        label: "zoom",
        description:
          "Inspect code symbols or documentation sections. For code, returns the full source of a symbol. Pass `callgraph: true` to also include call-graph annotations (calls-out / called-by within the same file). For Markdown and HTML, returns the section content under the given heading.\n\nUse `{ path, symbols }` or `{ url, symbols }` for one file/URL. `symbols` can be a string or array (one or many lookups in the same file/URL). Use `targets` for cross-file batches: `{ path, symbol }` or an array of them. Sending both merges them into one batch; a lookup that fails reports its own error line." +
          (githubZoomDescription ? `\n\n${githubZoomDescription}` : ""),
        parameters: ZoomParams,
        async execute(
          _toolCallId: string,
          params: Static<typeof ZoomParams>,
          _signal,
          _onUpdate,
          extCtx,
        ) {
          const bridge = bridgeFor(ctx, extCtx.cwd);
          // Some models fill every declared property on every call, sending
          // empty strings / arrays / objects (or whitespace) for the ones they
          // do not mean. Blank values count as absent, and `targets` entries
          // whose path and symbol are both blank are dropped as placeholders.
          const targetsInput = coerceJsonCollectionParam(params.targets, "targets");
          const targetEntries = usableZoomTargets(targetsInput);
          const hasPath = !isBlankParam(params.path);
          const hasUrl = !isBlankParam(params.url);
          const hasTargets = targetEntries.length > 0;
          const hasSymbols = !isBlankParam(params.symbols);
          // Coerce stringified booleans and numbers here so they reach the server
          // in the same form the main client sends.
          const wantCallgraph = coerceBoolean(params.callgraph);
          const contextLines = coerceOptionalInt(
            params.contextLines,
            "contextLines",
            1,
            Number.MAX_SAFE_INTEGER,
          );

          // Cross-file batch. `path`/`url` + `symbols` sent alongside
          // `targets` join the same batch instead of being refused: the server
          // answers every lookup and reports each one that fails on its own
          // line.
          if (hasTargets) {
            if (hasPath && hasUrl) {
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
            if (hasPath && !isGithubPathArg(params.path)) localPaths.push(params.path as string);
            const resolvedTargets = await Promise.all(
              localPaths.map((path) => resolvePathArg(extCtx.cwd, path)),
            );
            await assertReadPathPermissions(extCtx, ctx, resolvedTargets);

            const rawArgs: Record<string, unknown> = {
              targets: targets.map((target) => ({ filePath: target.path, symbol: target.symbol })),
            };
            if (hasPath) rawArgs.filePath = params.path;
            else if (hasUrl) rawArgs.url = params.url;
            if (hasSymbols) rawArgs.symbols = params.symbols;
            if (contextLines !== undefined) rawArgs.contextLines = contextLines;
            if (wantCallgraph) rawArgs.callgraph = true;

            const response = await callToolCall(bridge, "zoom", rawArgs, extCtx);
            if (response.success === false) {
              throw new Error(response.text || response.message || "zoom failed");
            }
            return textResult(response.text, response);
          }

          if (!hasPath && !hasUrl) {
            throw new Error("Provide exactly one of 'path', 'url', or 'targets'");
          }
          if (hasPath && hasUrl) {
            throw new Error("Provide exactly ONE of 'path' or 'url' — not both");
          }

          // URL mode passes through to Rust; Rust fetches, validates, and caches.
          // File mode still resolves locally before dispatch so external-directory
          // permission checks approve the same path the server will read.
          const githubPath =
            (hasPath && isGithubPathArg(params.path)) || (hasUrl && isGithubPathArg(params.url));
          if (!hasUrl && !githubPath) {
            const file = await resolvePathArg(extCtx.cwd, params.path as string);
            await assertReadPathPermissions(extCtx, ctx, file);
          }

          const rawArgs: Record<string, unknown> = hasUrl
            ? { url: params.url }
            : { filePath: params.path };
          if (hasSymbols) rawArgs.symbols = params.symbols;
          if (contextLines !== undefined) rawArgs.contextLines = contextLines;
          if (wantCallgraph) rawArgs.callgraph = true;

          const response = await callToolCall(bridge, "zoom", rawArgs, extCtx);
          if (response.success === false) {
            throw new Error(response.text || response.message || "zoom failed");
          }
          return textResult(response.text, response);
        },
        renderCall(args, theme, context) {
          return renderZoomCall(args, theme, context);
        },
        renderResult(result, options = { expanded: false, isPartial: false }, theme, context) {
          return renderZoomResult(result, context.args, theme, context, options);
        },
      }),
    );
  }
}
