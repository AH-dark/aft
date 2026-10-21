import * as path from "node:path";

/** A scripted engine reply, not a plugin-side containment implementation. */
export function engineOutsideAsk(target: string, args: Record<string, unknown>, directory = false) {
  const id = `outside:fixture:${target}`;
  if (Array.isArray(args.permissions_granted) && args.permissions_granted.includes(id)) return;
  const parentDir = directory ? target : path.dirname(target);
  const pattern = path.join(parentDir, "*").replaceAll("\\", "/");
  return {
    success: false,
    code: "permission_required",
    text: "outside access needs permission",
    permissions_continuation: Array.isArray(args.permissions_granted)
      ? args.permissions_granted
      : [],
    asks: [
      {
        kind: "external_directory",
        grant_id: id,
        path: target,
        parentDir,
        operation: "read",
        patterns: [pattern],
        always: [pattern],
      },
    ],
  };
}
