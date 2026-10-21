import { expect, test } from "bun:test";
import { timeoutForCommand } from "../command-timeouts.js";

test("tree-wide commands share the 60 second transport budget", () => {
  for (const command of ["ast_search", "ast_replace", "outline", "zoom", "import", "safety"]) {
    expect(timeoutForCommand(command)).toBe(60_000);
  }
  expect(timeoutForCommand("read")).toBeUndefined();
});
