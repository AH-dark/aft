import { describe, expect, test } from "bun:test";
import { e2eBinaryRequired } from "./e2e/binary-requirement.js";

describe("e2e binary requirement", () => {
  test("every CI spelling requires the binary", () => {
    for (const env of [
      { CI: "true" },
      { CI: "TRUE" },
      { CI: "1" },
      { GITHUB_ACTIONS: "true" },
      { AFT_REQUIRE_E2E_BINARY: "1" },
    ]) {
      expect(e2eBinaryRequired(env)).toBe(true);
    }
  });

  test("a local run without a CI flag may skip", () => {
    expect(e2eBinaryRequired({})).toBe(false);
    expect(e2eBinaryRequired({ CI: "false" })).toBe(false);
    expect(e2eBinaryRequired({ CI: "" })).toBe(false);
  });
});
