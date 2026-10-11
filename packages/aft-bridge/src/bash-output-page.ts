/** Decode raw pages without turning a split UTF-8 character into replacements. */
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
