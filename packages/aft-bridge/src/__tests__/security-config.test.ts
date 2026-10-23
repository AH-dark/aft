import { describe, expect, test } from "bun:test";
import { malformedSecurityConfigErrors } from "../feature-config.js";

describe("security configuration diagnostics", () => {
  test("security schema failures name every guarded root section", () => {
    for (const key of [
      "restrict_to_project_root",
      "url_fetch_allow_private",
      "sandbox",
      "indexes",
      "index",
      "remote_exec",
      "disabled_tools",
      "bash",
      "lsp",
      "semantic",
      "search",
      "subc",
      "opencode",
      "github",
      "gh_shim",
      "auto_update",
    ]) {
      expect(malformedSecurityConfigErrors({ [key]: null }, "opencode", [{ path: [key] }])).toEqual(
        [`invalid_security_config:${key}`],
      );
    }
  });

  test("active harness failures name the section and ignore inactive issues", () => {
    const raw = { harnesses: { opencode: { sandbox: null }, pi: { sandbox: null } } };
    expect(
      malformedSecurityConfigErrors(raw, "opencode", [
        { path: ["harnesses", "pi", "sandbox"] },
        { path: ["harnesses", "opencode", "sandbox", "profile"] },
      ]),
    ).toEqual(["invalid_security_config:harnesses.opencode.sandbox"]);
  });

  test("mixed invalid overrides reject instead of discarding valid hardening", () => {
    expect(
      malformedSecurityConfigErrors(
        { harnesses: { pi: { sandbox: { profile: { worker: "strict" } }, inspect: false } } },
        "pi",
        [{ path: ["harnesses", "pi", "inspect"] }],
      ),
    ).toEqual(["invalid_security_config:harnesses.pi"]);
  });

  test("malformed harness containers and active blocks reject", () => {
    expect(
      malformedSecurityConfigErrors({ harnesses: null }, "pi", [{ path: ["harnesses"] }]),
    ).toEqual(["invalid_security_config:harnesses"]);
    expect(
      malformedSecurityConfigErrors({ harnesses: { pi: null } }, "pi", [
        { path: ["harnesses", "pi"] },
      ]),
    ).toEqual(["invalid_security_config:harnesses.pi"]);
  });

  test("security diagnostics are sorted unique and prefer precise section paths", () => {
    expect(
      malformedSecurityConfigErrors(
        { harnesses: { opencode: { sandbox: null, inspect: false } } },
        "opencode",
        [
          { path: ["remote_exec"] },
          { path: ["harnesses", "opencode", "inspect"] },
          { path: ["harnesses", "opencode", "sandbox"] },
          { path: ["remote_exec"] },
          { path: ["bash"] },
        ],
      ),
    ).toEqual([
      "invalid_security_config:bash",
      "invalid_security_config:harnesses.opencode.sandbox",
      "invalid_security_config:remote_exec",
    ]);
  });

  test("ordinary malformed sections remain eligible for partial parsing", () => {
    expect(
      malformedSecurityConfigErrors({ inspect: false }, "pi", [{ path: ["inspect"] }]),
    ).toEqual([]);
    expect(
      malformedSecurityConfigErrors({ harnesses: { pi: { inspect: false } } }, "pi", [
        { path: ["harnesses", "pi", "inspect"] },
      ]),
    ).toEqual([]);
  });
});
