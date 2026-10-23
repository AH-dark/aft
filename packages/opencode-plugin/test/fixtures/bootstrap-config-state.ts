import { ConfigRejectedError, getConfigLoadErrors, loadAftConfig } from "../../src/config.js";

const directory = process.env.AFT_BOOTSTRAP_FIXTURE_PROJECT;
if (!directory) throw new Error("Missing isolated bootstrap fixture project");
try {
  loadAftConfig(directory);
} catch (error) {
  // Prime the rejected parser state without aborting the fake boot's isolation checks.
  if (!(error instanceof ConfigRejectedError)) throw error;
}
const expectedErrors = process.env.AFT_BOOTSTRAP_FIXTURE_INVALID === "1";
if (getConfigLoadErrors().length > 0 !== expectedErrors) {
  throw new Error("Bootstrap fixture did not establish the expected parser state");
}
