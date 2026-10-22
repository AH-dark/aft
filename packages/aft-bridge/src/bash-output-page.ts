/**
 * Shared handling of raw bash output pages. `bash_status` returns task output
 * in byte pages of at most 64 KiB, with `output_has_more` / `stderr_has_more`
 * saying whether captured bytes remain past the page.
 */

/**
 * Watch result text when the watch limit ends while an exited task still has
 * captured output left to scan. The wording matches the engine's own watch
 * (`commands/bash_watch.rs`), so every host reports this case the same way.
 */
export const TERMINAL_SCAN_INCOMPLETE_TEXT =
  "watch limit reached while scanning captured output. The task already exited; the pattern scan is incomplete. Use a longer watch or paged raw output to inspect the remaining bytes.";

/**
 * Added to an exited task's no-match watch result when a regex scan had to
 * drop older output from its 64 KiB window, so the non-match is not a
 * complete answer. Literal patterns never get it: the tail they keep is
 * exact. The text is the engine's `REGEX_WINDOW_SCAN_INCOMPLETE` in
 * `commands/bash_watch.rs`; a bridge test keeps the two identical.
 */
export const REGEX_WINDOW_SCAN_INCOMPLETE_TEXT =
  "No match found, but the scan is incomplete: a regex match longer than 64 KiB may have been missed. Literal patterns are always scanned in full.";

/**
 * Decodes one stream's raw pages without turning a UTF-8 character split
 * across a page edge into replacement characters. The incomplete tail of a
 * page (at most three bytes) is held back and decoded with the next page;
 * `end` flushes it when the stream really ends. `baseOffset` is the byte
 * offset in the stream where the returned text starts.
 */
export class BashOutputPageDecoder {
  private pending: Buffer = Buffer.alloc(0);
  private pendingOffset = 0;

  decode(bytes: Buffer, offset: number, end: boolean): { text: string; baseOffset: number } {
    const contiguous =
      this.pending.length > 0 && offset === this.pendingOffset + this.pending.length;
    const baseOffset = contiguous ? this.pendingOffset : offset;
    const combined = contiguous ? Buffer.concat([this.pending, bytes]) : bytes;
    const cut = end ? combined.length : completePrefixLength(combined);
    // Copy the at-most-three-byte carry instead of retaining a whole page's backing buffer.
    this.pending = Buffer.from(combined.subarray(cut));
    this.pendingOffset = baseOffset + cut;
    return { text: combined.subarray(0, cut).toString("utf8"), baseOffset };
  }
}

/**
 * Length of the longest prefix of `bytes` that does not end inside a
 * character that the next bytes could still complete. A trailing sequence
 * that can never become valid UTF-8 is kept, so it decodes as replacement
 * characters now rather than being held back.
 */
function completePrefixLength(bytes: Buffer): number {
  if (bytes.length === 0) return 0;
  let lead = bytes.length - 1;
  for (let count = 0; count < 3 && lead > 0 && (bytes.readUInt8(lead) & 0xc0) === 0x80; count++) {
    lead--;
  }
  const first = bytes.readUInt8(lead);
  const width =
    first >= 0xc2 && first <= 0xdf
      ? 2
      : first >= 0xe0 && first <= 0xef
        ? 3
        : first >= 0xf0 && first <= 0xf4
          ? 4
          : 0;
  const available = bytes.length - lead;
  if (width === 0 || available >= width) return bytes.length;
  for (let index = lead + 1; index < bytes.length; index++) {
    if ((bytes.readUInt8(index) & 0xc0) !== 0x80) return bytes.length;
  }
  const second = bytes[lead + 1];
  if (
    second !== undefined &&
    ((first === 0xe0 && second < 0xa0) ||
      (first === 0xed && second >= 0xa0) ||
      (first === 0xf0 && second < 0x90) ||
      (first === 0xf4 && second >= 0x90))
  )
    return bytes.length;
  return lead;
}
