import { access, mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { type BashCompletedPayload, BinaryBridge } from "../../bridge.js";
import { inlineUserConfigTier } from "../../config-tiers.js";

/** Obtain a real Rust completion so host tests cannot pass over a canned, already-redacted frame. */
export async function printedCredentialCompletion(): Promise<BashCompletedPayload> {
  const binary =
    process.env.AFT_TEST_BINARY ?? resolve(import.meta.dir, "../../../../../target/debug/aft");
  await access(binary);
  const directory = await mkdtemp(join(tmpdir(), "aft-completion-redaction-"));
  const token = `ghp_${"a".repeat(36)}`;
  let resolveCompletion: (value: BashCompletedPayload) => void = () => {};
  const completion = new Promise<BashCompletedPayload>((resolve) => {
    resolveCompletion = resolve;
  });
  const bridge = new BinaryBridge(binary, directory, {
    maxRestarts: 0,
    childEnv: {
      AFT_CACHE_DIR: join(directory, "cache"),
      RUSTC_WRAPPER: "",
      CARGO_BUILD_RUSTC_WRAPPER: "",
    },
    onBashCompletion: resolveCompletion,
  });
  let timer: ReturnType<typeof setTimeout> | undefined;
  try {
    const configured = await bridge.send("configure", {
      project_root: directory,
      storage_dir: join(directory, "storage"),
      harness: "opencode",
      bash_permissions: false,
      config: inlineUserConfigTier({
        bash: { background: true, compress: false },
        sandbox: { profile: { head: "off", worker: "off" } },
      }),
    });
    if (!configured.success) throw new Error(`configure failed: ${JSON.stringify(configured)}`);
    const started = await bridge.send("bash", {
      command:
        process.platform === "win32" ? `Write-Output '${token}'` : `printf '%s\\n' '${token}'`,
      background: true,
      compressed: false,
    });
    if (!started.success) throw new Error(`bash failed: ${JSON.stringify(started)}`);
    return await Promise.race([
      completion,
      new Promise<never>((_, reject) => {
        timer = setTimeout(() => reject(new Error("completion frame missing")), 15_000);
      }),
    ]);
  } finally {
    clearTimeout(timer);
    await bridge.shutdown();
    await rm(directory, { recursive: true, force: true });
  }
}
