import * as fs from "node:fs";
import * as path from "node:path";
import { coerceStringArray } from "@cortexkit/aft-bridge";
import type { ToolContext, ToolDefinition } from "@opencode-ai/plugin";
import { tool } from "@opencode-ai/plugin";
import { prepareToolMap } from "../normalize-schemas.js";
import type { PluginContext } from "../types.js";
import {
  callBridge,
  callToolCall,
  expandTilde,
  resolvePathFromProjectRoot,
  resolveProjectRoot,
} from "./_shared.js";
import {
  askEditPermission,
  assertExternalDirectoryPermission,
  permissionDeniedResponse,
  resolveAbsolutePath,
  resolveRelativePatternFromAbsolute,
} from "./permissions.js";

const z = tool.schema;

function responsePaths(response: Record<string, unknown>): string[] {
  return Array.isArray(response.paths)
    ? response.paths.filter((path): path is string => typeof path === "string" && path.length > 0)
    : [];
}

function bridgeErrorMessage(response: Record<string, unknown>, fallback: string): string {
  return typeof response.message === "string" && response.message.length > 0
    ? response.message
    : fallback;
}

function relativePatternsFromPaths(context: ToolContext, paths: string[]): string[] {
  const seen = new Set<string>();
  const patterns: string[] = [];

  for (const filePath of paths) {
    const absolutePath = resolveAbsolutePath(context, filePath);
    const pattern = resolveRelativePatternFromAbsolute(context, absolutePath);
    if (seen.has(pattern)) continue;
    seen.add(pattern);
    patterns.push(pattern);
  }

  return patterns;
}

/**
 * Tool definitions for safety & recovery commands: undo, edit_history,
 * checkpoint, restore_checkpoint, list_checkpoints, purge.
 */
export function safetyTools(ctx: PluginContext): Record<string, ToolDefinition> {
  return prepareToolMap({
    aft_safety: {
      description:
        "File safety and recovery operations.\n\n" +
        "Per-file undo stack is capped at 20 entries (oldest evicted).\n\n" +
        "Ops:\n" +
        "- 'undo': Undo the entire last tool call when 'path' is omitted (typical), or undo the last edit to one file when 'path' is provided. Note: pops from the undo stack (irreversible, no redo). Use 'history' to inspect per-file history before undoing.\n" +
        "- 'history': List all edit snapshots for a file. Requires 'path'.\n" +
        "- 'checkpoint': Save a named snapshot (on disk across restarts; expires in 14 days; may contain secrets). Explicit files may be untracked/gitignored; omit for backup-tracked files. Requires 'name'.\n" +
        "- 'restore': Restore files to a previously saved checkpoint. Optional 'path'/'files' scopes the restore; omitted scope restores the whole checkpoint. Requires 'name'.\n" +
        "- 'list': List all available named checkpoints. No extra params needed.\n" +
        "- 'purge': Permanently delete this session's undo backups and checkpoint copies of 'path' (a file, or everything under a directory); cannot be undone.\n\n" +
        "Each op requires specific parameters — see parameter descriptions for requirements.\n\n" +
        "Use checkpoint before risky multi-file changes. Use undo for quick single-file rollback.",
      // Parameters are Zod-optional because different ops need different subsets.
      // Runtime guards below validate per-op requirements and give clear errors.
      args: {
        op: z
          .enum(["undo", "history", "checkpoint", "restore", "list", "purge"])
          .describe("Safety operation"),
        path: z
          .string()
          .optional()
          .describe(
            "File path (required for history, optional for undo, optional for checkpoint — snapshots only that file when files is omitted, optional for restore — restores only that file, required for purge — a file or directory). Absolute or relative to project root",
          ),
        name: z.string().optional().describe("Checkpoint name (required for checkpoint, restore)"),
        files: z
          .array(z.string())
          .optional()
          .describe(
            "Specific files to include in checkpoint, or to restore from a checkpoint (optional; restore without path/files restores the whole checkpoint; explicit checkpoint files may be untracked or gitignored)",
          ),
      },
      execute: async (args, context): Promise<string> => {
        const op = args.op as string;

        if ((op === "history" || op === "purge") && typeof args.path !== "string") {
          throw new Error(`'path' is required for '${op}' op`);
        }
        if ((op === "checkpoint" || op === "restore") && typeof args.name !== "string") {
          throw new Error(`'name' is required for '${op}' op`);
        }

        if (op === "undo") {
          const previewParams: Record<string, unknown> = {};
          if (typeof args.path === "string") previewParams.file = args.path;
          const preview = await callBridge(ctx, context, "undo_preview", previewParams);
          if (preview.success === false) {
            throw new Error(bridgeErrorMessage(preview, "undo preview failed"));
          }

          const previewPaths = Array.from(new Set(responsePaths(preview)));
          for (const filePath of previewPaths) {
            const denial = await assertExternalDirectoryPermission(ctx, context, filePath);
            if (denial) return permissionDeniedResponse(denial, context);
          }

          const filePath =
            typeof args.path === "string"
              ? resolveAbsolutePath(context, args.path as string)
              : undefined;
          const permissionError = await askEditPermission(
            context,
            relativePatternsFromPaths(context, previewPaths),
            filePath ? { filepath: filePath } : { operation: "undo", paths: previewPaths },
          );
          if (permissionError) return permissionDeniedResponse(permissionError, context);
        }

        if (op === "checkpoint") {
          const coercedFiles = coerceStringArray(args.files);
          const checkpointFiles =
            coercedFiles.length > 0
              ? coercedFiles
              : typeof args.path === "string"
                ? [args.path]
                : undefined;
          if (Array.isArray(checkpointFiles)) {
            const projectRoot = await resolveProjectRoot(ctx, context);
            const uniqueParents = new Set<string>();
            for (const rawFile of checkpointFiles) {
              if (typeof rawFile !== "string") continue;
              // Expand ~ so the permission check resolves the real target (and
              // matches what Rust receives below); a relative path is left for
              // path.resolve against the project root.
              const file = expandTilde(rawFile);
              const abs = path.isAbsolute(file) ? file : path.resolve(projectRoot, file);
              const parent = path.dirname(abs);
              if (uniqueParents.has(parent)) continue;
              uniqueParents.add(parent);
              const denial = await assertExternalDirectoryPermission(ctx, context, abs, {
                kind: "file",
              });
              if (denial) return permissionDeniedResponse(denial, context);
            }
          }
        }

        if (op === "restore") {
          const projectRoot = await resolveProjectRoot(ctx, context);
          const previewParams: Record<string, unknown> = { name: args.name };
          if (typeof args.path === "string") {
            previewParams.file = resolvePathFromProjectRoot(projectRoot, args.path);
          }
          const previewFiles = coerceStringArray(args.files).map((file) =>
            resolvePathFromProjectRoot(projectRoot, file),
          );
          if (previewFiles.length > 0) previewParams.files = previewFiles;
          const preview = await callBridge(ctx, context, "checkpoint_paths", previewParams);
          if (preview.success === false) {
            throw new Error(bridgeErrorMessage(preview, "checkpoint path preview failed"));
          }

          for (const filePath of new Set(responsePaths(preview))) {
            const denial = await assertExternalDirectoryPermission(ctx, context, filePath);
            if (denial) return permissionDeniedResponse(denial, context);
          }

          const permissionError = await askEditPermission(
            context,
            relativePatternsFromPaths(context, Array.from(new Set(responsePaths(preview)))),
            { checkpoint: args.name },
          );
          if (permissionError) return permissionDeniedResponse(permissionError, context);
        }

        if (op === "purge") {
          // Purging destroys copies of the path's content, so it asks the same
          // edit permission a write to the path would. A directory is asked
          // for as itself and as everything under it, so a rule like
          // `secrets/*` also governs purging `secrets`. Whether the path still
          // exists cannot decide the ask: a deleted directory's saved copies
          // are still purged. So, as restore does, the engine first lists
          // every saved path the purge would remove, and each one is asked
          // for too; any denial refuses the purge before anything is removed.
          const absolutePath = resolveAbsolutePath(context, args.path as string);
          let isDirectory = false;
          try {
            isDirectory = fs.statSync(absolutePath).isDirectory();
          } catch {
            // A deleted path is asked for as itself plus its saved copies below.
          }
          const denial = await assertExternalDirectoryPermission(ctx, context, absolutePath, {
            kind: isDirectory ? "directory" : "file",
          });
          if (denial) return permissionDeniedResponse(denial, context);
          // Sent as the purge itself sends it, so the engine resolves both alike.
          const preview = await callBridge(ctx, context, "safety_purge_paths", {
            file: expandTilde(args.path as string),
          });
          if (preview.success === false) {
            throw new Error(bridgeErrorMessage(preview, "purge path preview failed"));
          }
          const previewPaths = Array.from(new Set(responsePaths(preview)));
          for (const savedPath of previewPaths) {
            const previewDenial = await assertExternalDirectoryPermission(ctx, context, savedPath);
            if (previewDenial) return permissionDeniedResponse(previewDenial, context);
          }
          const pattern = resolveRelativePatternFromAbsolute(context, absolutePath);
          const patterns = isDirectory ? [pattern, path.join(pattern, "*")] : [pattern];
          for (const previewPattern of relativePatternsFromPaths(context, previewPaths)) {
            if (!patterns.includes(previewPattern)) patterns.push(previewPattern);
          }
          const permissionError = await askEditPermission(context, patterns, {
            filepath: absolutePath,
            operation: "purge",
          });
          if (permissionError) return permissionDeniedResponse(permissionError, context);
        }

        const rawArgs: Record<string, unknown> = { op };
        if (args.name !== undefined) rawArgs.name = args.name;
        // Expand ~ on every path so Rust (which treats ~ literally) gets the real
        // target instead of creating/looking up a literal `~` path. Relative
        // paths are left for Rust to resolve against the project root.
        const payloadFiles = coerceStringArray(args.files).map(expandTilde);
        const filePathArg =
          typeof args.path === "string" ? expandTilde(args.path as string) : undefined;
        if (filePathArg !== undefined) rawArgs.filePath = filePathArg;
        if (payloadFiles.length > 0) rawArgs.files = payloadFiles;

        const response = await callToolCall(ctx, context, "safety", rawArgs);
        if (response.success === false) {
          throw new Error((response.message as string) || `${op} failed`);
        }
        return response.text;
      },
    },
  });
}
