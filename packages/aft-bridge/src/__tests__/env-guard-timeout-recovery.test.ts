import { describe, expect, test } from "bun:test";
import { acquireEnv, withEnv } from "./test-utils/env-guard.js";

const proveTimedOutHolder = process.env.AFT_TEST_ENV_GUARD_TIMEOUT_PROOF === "1";
const proveFinishedHolder = process.env.AFT_TEST_ENV_GUARD_FINISH_PROOF === "1";

// The timeout case is enabled only for a dedicated proof run; normal suites do
// not deliberately time out a test or leave an environment fixture pending.
describe.serial("environment fixture timeout recovery", () => {
  if (proveTimedOutHolder) {
    test("intentional environment fixture holder times out", async () => {
      await withEnv({ AFT_ENV_GUARD_TIMEOUT_PROOF: "held" }, () => new Promise<void>(() => {}));
    });
  } else if (proveFinishedHolder) {
    test("an environment fixture can outlive its finished test", async () => {
      let markStarted!: () => void;
      const started = new Promise<void>((resolve) => {
        markStarted = resolve;
      });
      void withEnv({ AFT_ENV_GUARD_TIMEOUT_PROOF: "held" }, () => {
        markStarted();
        return new Promise<void>(() => {});
      });
      await started;
    });
  } else {
    test.skip("intentional environment fixture holder times out", () => {});
  }

  if (proveTimedOutHolder || proveFinishedHolder) {
    test("a later environment fixture acquires the lock", async () => {
      const release = await acquireEnv({ AFT_ENV_GUARD_TIMEOUT_PROOF: "next" });
      try {
        expect(process.env.AFT_ENV_GUARD_TIMEOUT_PROOF).toBe("next");
      } finally {
        release();
      }
    });
  } else {
    test.skip("a later environment fixture acquires the lock", () => {});
  }
});
