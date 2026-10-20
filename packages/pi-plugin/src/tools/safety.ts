/**
 * aft_safety — operation undo, per-file history, named checkpoints, restore, list.
 */

import { coerceStringArray } from "@cortexkit/aft-bridge";
import { StringEnum } from "@earendil-works/pi-ai";
import type { AgentToolResult, ExtensionAPI, Theme } from "@earendil-works/pi-coding-agent";
import { type Static, Type } from "typebox";
import type { PluginContext } from "../types.js";
import {
  bridgeFor,
  callBridge,
  callToolCall,
  textResult,
  withPathAliasPreparation,
} from "./_shared.js";
import { assertExternalDirectoryPermission, resolvePathArg } from "./hoisted.js";
import {
  accentPath,
  asNumber,
  asRecord,
  asRecordOrEmpty,
  asRecords,
  asString,
  collapsibleResult,
  extractStructuredPayload,
  formatTimestamp,
  type RenderContextLike,
  type RenderResultOptionsLike,
  renderErrorResult,
  renderSections,
  renderToolCall,
  shortenPath,
} from "./render-helpers.js";

function responsePaths(response: Record<string, unknown>): string[] {
  return Array.isArray(response.paths)
    ? response.paths.filter((path): path is string => typeof path === "string" && path.length > 0)
    : [];
}

const SafetyParams = Type.Object({
  op: StringEnum(["undo", "history", "checkpoint", "restore", "list", "purge"] as const, {
    description: "Safety operation",
  }),
  path: Type.Optional(
    Type.String({
      description:
        "File path (required for history, optional for undo, optional for checkpoint — snapshots only that file when files is omitted, optional for restore — restores only that file, required for purge — a file or directory). Absolute or relative to project root.",
    }),
  ),
  name: Type.Optional(
    Type.String({ description: "Checkpoint name (required for checkpoint, restore)" }),
  ),
  files: Type.Optional(
    Type.Array(Type.String(), {
      description:
        "Specific files for checkpoint, or to restore from a checkpoint (optional; restore without path/files restores the whole checkpoint; explicit checkpoint files may be untracked or gitignored)",
    }),
  ),
});

/** Exported for renderer unit tests. */
export function buildSafetySections(args: unknown, payload: unknown, theme: Theme): string[] {
  const safeArgs = asRecordOrEmpty(args);
  const op = asString(safeArgs.op);
  const path = asString(safeArgs.path);
  const name = asString(safeArgs.name);
  const response = asRecord(payload);
  if (!response) return [theme.fg("muted", "No safety result.")];

  if (op === "undo") {
    if (response.operation === true) {
      return [
        `${theme.fg("success", "restored operation")} ${theme.fg("accent", asString(response.op_id) ?? "(operation)")}`,
        `${theme.fg("muted", "files")} ${asNumber(response.restored_count) ?? asRecords(response.restored).length}`,
      ];
    }
    return [
      `${theme.fg("success", "restored")} ${theme.fg("accent", shortenPath(asString(response.path) ?? path ?? "(file)"))}`,
      `${theme.fg("muted", "backup")} ${asString(response.backup_id) ?? "—"}`,
    ];
  }

  if (op === "history") {
    const entries = asRecords(response.entries);
    const sections = [theme.fg("accent", shortenPath(asString(response.file) ?? path ?? "(file)"))];
    if (entries.length === 0) {
      sections.push(theme.fg("muted", "No history entries."));
      return sections;
    }
    sections.push(
      entries
        .map((entry, index) => {
          const backupId = asString(entry.backup_id) ?? `entry-${index + 1}`;
          const timestamp = formatTimestamp(entry.timestamp) ?? "unknown time";
          const description = asString(entry.description) ?? "";
          return `${index + 1}. ${backupId} ${theme.fg("muted", timestamp)}${description ? `\n   ${description}` : ""}`;
        })
        .join("\n"),
    );
    return sections;
  }

  if (op === "checkpoint") {
    const skipped = asRecords(response.skipped);
    return [
      `${theme.fg("success", "checkpoint created")} ${theme.fg("accent", asString(response.name) ?? name ?? "(checkpoint)")}`,
      `${theme.fg("muted", "files")} ${asNumber(response.file_count) ?? 0}`,
      skipped.length > 0
        ? `${theme.fg("warning", "skipped")}\n${skipped.map((entry) => `  ↳ ${shortenPath(asString(entry.file) ?? "(file)")}: ${asString(entry.error) ?? "unknown error"}`).join("\n")}`
        : theme.fg("muted", "No skipped files."),
      asString(response.durability),
    ].filter((section): section is string => Boolean(section));
  }

  if (op === "restore") {
    return [
      `${theme.fg("success", "checkpoint restored")} ${theme.fg("accent", asString(response.name) ?? name ?? "(checkpoint)")}`,
      `${theme.fg("muted", "files")} ${asNumber(response.file_count) ?? 0}`,
      asString(response.durability),
    ].filter((section): section is string => Boolean(section));
  }

  if (op === "list") {
    const checkpoints = asRecords(response.checkpoints);
    const sections = [
      theme.fg("accent", `${checkpoints.length} checkpoint${checkpoints.length === 1 ? "" : "s"}`),
    ];
    if (checkpoints.length === 0) {
      sections.push(theme.fg("muted", "No checkpoints saved."));
      const durability = asString(response.durability);
      if (durability) sections.push(theme.fg("muted", durability));
      return sections;
    }
    sections.push(
      checkpoints
        .map((checkpoint, index) => {
          const name = asString(checkpoint.name) ?? `checkpoint-${index + 1}`;
          const count = asNumber(checkpoint.file_count) ?? 0;
          const created = formatTimestamp(checkpoint.created_at) ?? "unknown time";
          return `${index + 1}. ${name} ${theme.fg("muted", `${count} file${count === 1 ? "" : "s"} · ${created}`)}`;
        })
        .join("\n"),
    );
    const durability = asString(response.durability);
    if (durability) sections.push(theme.fg("muted", durability));
    return sections;
  }

  if (op === "purge") {
    const pruned = asRecords(response.checkpoints_pruned)
      .map((entry) => asString(entry.name))
      .filter((name): name is string => Boolean(name));
    const dropped = Array.isArray(response.checkpoints_dropped)
      ? response.checkpoints_dropped.filter((name): name is string => typeof name === "string")
      : [];
    const failures = asRecords(response.failures);
    return [
      `${theme.fg("success", "purged")} ${theme.fg("accent", shortenPath(asString(response.path) ?? path ?? "(path)"))}`,
      `${theme.fg("muted", "undo backups removed")} ${asNumber(response.undo_entries_removed) ?? 0}`,
      pruned.length > 0 ? `${theme.fg("muted", "checkpoints pruned")} ${pruned.join(", ")}` : "",
      dropped.length > 0 ? `${theme.fg("muted", "checkpoints dropped")} ${dropped.join(", ")}` : "",
      failures.length > 0
        ? `${theme.fg("warning", "not removed")}\n${failures.map((entry) => `  ↳ ${asString(entry.target) ?? "(unknown)"}: ${asString(entry.error) ?? "unknown error"}`).join("\n")}`
        : "",
      theme.fg("warning", "This cannot be undone."),
    ].filter((section) => section.length > 0);
  }

  const knownDetails = [path, name].filter((value): value is string => Boolean(value));
  return [theme.fg("muted", "Safety result available."), ...knownDetails];
}

/** Exported for renderer unit tests. */
export function renderSafetyCall(args: unknown, theme: Theme, context: RenderContextLike) {
  const safeArgs = asRecordOrEmpty(args);
  const op = asString(safeArgs.op);
  const target = asString(safeArgs.path) ?? asString(safeArgs.name);
  const summary = [
    op ? theme.fg("accent", op) : undefined,
    target ? accentPath(theme, target) : undefined,
  ]
    .filter(Boolean)
    .join(" ");
  return renderToolCall("safety", summary, theme, context);
}

/** Exported for renderer unit tests. */
export function renderSafetyResult(
  result: AgentToolResult<unknown>,
  args: unknown,
  theme: Theme,
  context: RenderContextLike,
  options: RenderResultOptionsLike = { expanded: true },
) {
  if (context.isError) return renderErrorResult(result, "safety failed", theme, context);
  const sections = buildSafetySections(args, extractStructuredPayload(result), theme);
  const op = asString(asRecordOrEmpty(args).op) ?? "safety";
  return collapsibleResult({
    summary: `${op}: ${sections[0] ?? "completed"}`,
    full: renderSections(sections, context),
    expanded: options.expanded,
    context,
  });
}

export function registerSafetyTool(pi: ExtensionAPI, ctx: PluginContext): void {
  pi.registerTool(
    withPathAliasPreparation({
      name: "aft_safety",
      label: "safety",
      description:
        "`undo` reverts a tool call (omit path) or latest file snapshot (irreversible); `history` lists snapshots; `checkpoint` saves named snapshots on disk across restarts, expiring after 14 days (contents may include secrets); `restore` restores a checkpoint, optionally scoped by path/files; `list` lists checkpoints; `purge` permanently deletes this session's undo backups and checkpoint copies of a file or directory `path` (cannot be undone). Undo keeps 20 snapshots/file (oldest evicted).",
      parameters: SafetyParams,
      async execute(
        _toolCallId: string,
        params: Static<typeof SafetyParams>,
        _signal,
        _onUpdate,
        extCtx,
      ) {
        if ((params.op === "history" || params.op === "purge") && !params.path) {
          throw new Error(`op='${params.op}' requires 'path'`);
        }
        if ((params.op === "checkpoint" || params.op === "restore") && !params.name) {
          throw new Error(`op='${params.op}' requires 'name'`);
        }

        const filePath = params.path ? await resolvePathArg(extCtx.cwd, params.path) : undefined;
        // Coerce at the boundary: a bare-string/JSON-stringified `files` would
        // otherwise crash the unchecked `.map` below before validation.
        const fileInputs = coerceStringArray(params.files);
        const files =
          fileInputs.length > 0
            ? await Promise.all(fileInputs.map((file) => resolvePathArg(extCtx.cwd, file)))
            : undefined;
        const bridge = bridgeFor(ctx, extCtx.cwd);
        const restrictToProjectRoot = ctx.config.restrict_to_project_root ?? false;

        if (params.op === "undo") {
          const previewReq: Record<string, unknown> = {};
          if (filePath) previewReq.file = filePath;
          const preview = await callBridge(bridge, "undo_preview", previewReq, extCtx);
          for (const file of new Set(responsePaths(preview))) {
            await assertExternalDirectoryPermission(extCtx, file, {
              restrictToProjectRoot,
            });
          }
        }
        if (params.op === "checkpoint") {
          const checkpointFiles = files ?? (filePath ? [filePath] : undefined);
          if (Array.isArray(checkpointFiles)) {
            const checked = new Set<string>();
            for (const file of checkpointFiles) {
              if (checked.has(file)) continue;
              checked.add(file);
              await assertExternalDirectoryPermission(extCtx, file, {
                restrictToProjectRoot,
              });
            }
          }
        }
        if (params.op === "restore" && params.name) {
          const preview = await callBridge(
            bridge,
            "checkpoint_paths",
            { name: params.name },
            extCtx,
          );
          for (const file of new Set(responsePaths(preview))) {
            await assertExternalDirectoryPermission(extCtx, file, {
              restrictToProjectRoot,
            });
          }
        }

        if (params.op === "purge" && params.path) {
          // Pi has no edit prompt; the purge path gets the same project-root
          // check every other write in this plugin gets. A purge reaches every
          // saved copy under the path, whether or not the path still exists,
          // so, as for restore, each saved path the engine says it would
          // remove is checked too before anything is removed.
          const purgeTarget = await resolvePathArg(extCtx.cwd, params.path);
          await assertExternalDirectoryPermission(extCtx, purgeTarget, { restrictToProjectRoot });
          const preview = await callBridge(
            bridge,
            "safety_purge_paths",
            { file: purgeTarget },
            extCtx,
          );
          for (const file of new Set(responsePaths(preview))) {
            await assertExternalDirectoryPermission(extCtx, file, {
              restrictToProjectRoot,
            });
          }
        }

        const rawArgs: Record<string, unknown> = { op: params.op };
        if (filePath) rawArgs.filePath = filePath;
        if (params.name) rawArgs.name = params.name;
        if (files) rawArgs.files = files;
        const response = await callToolCall(bridge, "safety", rawArgs, extCtx);
        if (response.success === false) {
          throw new Error(response.text || response.message || `${params.op} failed`);
        }
        return textResult(response.text, response);
      },
      renderCall(args, theme, context) {
        return renderSafetyCall(args, theme, context);
      },
      renderResult(result, options = { expanded: false, isPartial: false }, theme, context) {
        return renderSafetyResult(result, context.args, theme, context, options);
      },
    }),
  );
}
