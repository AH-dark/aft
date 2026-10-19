/**
 * Shared wording and helpers for reading a file at a git revision: the
 * optional `ref` argument of `read`, `aft_outline` and `aft_zoom`.
 *
 * The argument exists only while the `revision_reads` config key is true.
 * While it is off, the OpenCode and Pi tools neither advertise it nor send
 * it; a call that still carries it is forwarded so the engine refuses it by
 * name (`revision_reads_disabled`) rather than reading the working tree.
 */

/** Description of the `ref` argument itself; paid in every prompt offering it. */
export const REVISION_REF_DESCRIPTION =
  "Git revision to read the file at instead of the working tree: a branch, tag, full or short commit SHA, or a form like HEAD~2. Content comes from git's object store; the working tree, index and HEAD are not touched.";

/** Sentence appended to the tool descriptions when `ref` is offered. */
export const REVISION_REF_TOOL_NOTE =
  "Pass `ref` to read the file as it is at a git revision (branch, tag, SHA, HEAD~2); the reply's first line names the commit it resolved to.";

/** Whether this configuration offers the `ref` argument. */
export function revisionReadsOffered(config: { revision_reads?: boolean }): boolean {
  return config.revision_reads === true;
}

/**
 * The `ref` value to forward, or undefined when the caller sent none. A blank
 * string counts as absent, like the placeholders some models put in every
 * optional field; any other value is forwarded unchanged for the engine to
 * validate.
 */
export function revisionRefArg(value: unknown): unknown {
  if (value === undefined || value === null) return undefined;
  if (typeof value === "string" && value.trim().length === 0) return undefined;
  return value;
}
