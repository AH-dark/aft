/**
 * Live reload of the plugin's own copy of the AFT config (`ctx.config`) when
 * `~/.config/cortexkit/aft.jsonc` or `<project>/.cortexkit/aft.jsonc` changes.
 * Shared by the OpenCode 1 plugin and the OpenCode 2 server entry. The engine
 * applies its own keys from the same files separately.
 */

import {
  aftLiveConfigKeys,
  aftLiveSecurityKeys,
  formatConfigParseErrorMessage,
  type LiveConfigLoad,
  type LiveConfigReload,
  resolveCortexKitConfigPaths,
  startLiveConfigReload,
} from "@cortexkit/aft-bridge";

import {
  type AftConfig,
  getConfigLoadErrors,
  getConfigLoadSources,
  getConfigLoadTexts,
  getConfigValidationErrors,
  loadAftConfig,
  resolveBashConfig,
} from "./config.js";
import { error, log } from "./logger.js";

/** The live keys, read through this host's bash resolution. */
export const OPENCODE_LIVE_CONFIG_KEYS = aftLiveConfigKeys<AftConfig>(
  (config) =>
    resolveBashConfig(config) as ReturnType<typeof resolveBashConfig> & Record<string, unknown>,
);

/**
 * Load both config files for a live reload. A parse failure, a setting whose
 * value does not validate, or a rejected key is an error here, not a default:
 * the reload then keeps the last valid config. (Loading at host startup keeps
 * the valid part of such a file instead.)
 */
function projectTextOf(directory: string): string | null {
  const { projectConfigPath } = resolveCortexKitConfigPaths(directory);
  return getConfigLoadTexts().get(projectConfigPath) ?? null;
}

export function loadOpenCodeConfigForLiveReload(directory: string): LiveConfigLoad<AftConfig> {
  try {
    const config = loadAftConfig(directory);
    const [failure] = getConfigLoadErrors();
    if (failure) {
      return { ok: false, message: formatConfigParseErrorMessage(failure.path, failure.message) };
    }
    const [invalid] = getConfigValidationErrors();
    if (invalid) {
      return {
        ok: false,
        message: `AFT config at ${invalid.path} has an invalid setting: ${invalid.message}`,
      };
    }
    return {
      ok: true,
      config,
      sources: [...getConfigLoadSources()],
      projectText: projectTextOf(directory),
      texts: Object.fromEntries(getConfigLoadTexts()),
    };
  } catch (err) {
    return { ok: false, message: err instanceof Error ? err.message : String(err) };
  }
}

export interface OpenCodeLiveConfigReloadOptions {
  /** The directory whose project config applies. */
  directory: string;
  /** The config files the startup load read (the bootstrap result's `sources`). */
  initialSources: readonly string[];
  /** Their texts (the bootstrap result's `sourceTexts`). */
  initialSourceTexts?: Readonly<Record<string, string>>;
  getConfig(): AftConfig;
  setConfig(config: AftConfig): void;
  /** Show a config error to the user. */
  notify(message: string): void;
  /** Tests drive `reload()` directly instead of watching. */
  watch?: boolean;
}

/** Keep `ctx.config` current for the live keys while the host runs. */
export function startOpenCodeLiveConfigReload(
  options: OpenCodeLiveConfigReloadOptions,
): LiveConfigReload {
  const { userConfigPath, projectConfigPath } = resolveCortexKitConfigPaths(options.directory);
  return startLiveConfigReload<AftConfig>({
    paths: [userConfigPath, projectConfigPath],
    projectPath: projectConfigPath,
    load: () => loadOpenCodeConfigForLiveReload(options.directory),
    initialSources: options.initialSources,
    initialProjectText: options.initialSourceTexts?.[projectConfigPath] ?? null,
    initialTexts: options.initialSourceTexts,
    securityKeys: aftLiveSecurityKeys(OPENCODE_LIVE_CONFIG_KEYS),
    keys: OPENCODE_LIVE_CONFIG_KEYS,
    getConfig: options.getConfig,
    setConfig: options.setConfig,
    log: (message) => log(`${message} (${options.directory})`),
    reportError: (message) => {
      error(message);
      options.notify(message);
    },
    watch: options.watch,
  });
}
