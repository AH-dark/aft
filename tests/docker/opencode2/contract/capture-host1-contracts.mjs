#!/usr/bin/env node
import { createHash } from "node:crypto";
import { spawn } from "node:child_process";
import {
  chmod,
  mkdir,
  mkdtemp,
  readFile,
  realpath,
  rm,
  writeFile,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const scriptDir = dirname(fileURLToPath(import.meta.url));
const repoRoot = resolve(scriptDir, "../../../../..");
const contractDir = scriptDir;
const probeDir = join(contractDir, "probe");
const version = process.argv[2]?.trim() || (await readFile(join(repoRoot, ".github/opencode-version.txt"), "utf8")).trim();
if (!/^\d+\.\d+\.\d+(?:[-+][0-9A-Za-z.-]+)?$/.test(version)) {
  throw new Error(`invalid OpenCode version: ${version || "<empty>"}`);
}
if (process.platform !== "linux" || process.arch !== "x64") {
  throw new Error("capture host1 contracts on Linux x64; use the Docker command in tests/docker/opencode2/harness/README.md");
}

const root = await mkdtemp(join(tmpdir(), "aft-opencode1-contract-"));
const runId = `oc1-${version}-${new Date().toISOString().replace(/[-:.]/g, "")}-t7-parity`;
const sourceSha = (process.env.AFT_CAPTURE_SOURCE_SHA ?? (await exec("git", ["rev-parse", "HEAD"], { cwd: repoRoot })).stdout).trim();
if (!/^[0-9a-f]{40}$/.test(sourceSha)) {
  throw new Error("set AFT_CAPTURE_SOURCE_SHA to the full repository checkout SHA when running in a container");
}
const hostPrefix = join(root, "host");
const aimockPrefix = join(root, "aimock");
const executable = join(hostPrefix, "node_modules", ".bin", "opencode");
const oldProvider = JSON.parse(await readFile(join(contractDir, "host1-provider-config.json"), "utf8"));
const oldGate = JSON.parse(await readFile(join(contractDir, "host1-edit-family-gate.json"), "utf8"));
const suffix = version;
const providerTranscript = `probe/host1-provider-config-${suffix}.txt`;
const gateTranscript = `probe/host1-edit-family-gate-${suffix}.txt`;
let mockProcess;

try {
  await mkdir(probeDir, { recursive: true });
  await exec("npm", ["install", "--prefix", hostPrefix, "--no-audit", "--no-fund", `opencode-ai@${version}`], {
    cwd: repoRoot,
    env: await isolatedEnvironment(join(root, "install")),
    timeoutMs: 300_000,
  });
  await exec("npm", ["install", "--prefix", aimockPrefix, "--no-audit", "--no-fund", "@copilotkit/aimock@1.24.0"], {
    cwd: repoRoot,
    env: await isolatedEnvironment(join(root, "aimock-install")),
    timeoutMs: 300_000,
  });

  const installedVersion = (await exec(executable, ["--version"], {
    cwd: root,
    env: await isolatedEnvironment(join(root, "version")),
  })).stdout.trim();
  if (!installedVersion.includes(version)) {
    throw new Error(`installed host reports ${JSON.stringify(installedVersion)}, expected ${version}`);
  }

  const bundlePath = await realpath(executable);
  const bundle = await readFile(bundlePath, "latin1");
  const sha256 = createHash("sha256").update(await readFile(bundlePath)).digest("hex");
  const requiredGateFragments = [
    oldGate.registry_function,
    oldGate.selector_source,
    ...oldGate.tool_selectors.map((selector) => selector.source),
  ];
  const missingFragments = requiredGateFragments.filter((fragment) => !bundle.includes(fragment));
  if (missingFragments.length > 0) {
    throw new Error(
      `the ${version} bundle changed the captured edit-family selector; inspect ${bundlePath} and update host1-edit-family-gate.json before publishing this pin (missing: ${missingFragments.join(", ")})`,
    );
  }

  const mock = await startAimock(aimockPrefix, root);
  mockProcess = mock.process;
  const candidates = [
    {
      label: "V2 providers/package/settings shape",
      configKey: "providers",
      model: "openai/mock-model",
      config: {
        providers: {
          openai: {
            name: "Deterministic aimock",
            package: "@opencode/ai/providers/openai-compatible",
            settings: { baseURL: `${mock.url}/v1`, apiKey: "{env:OPENAI_API_KEY}" },
            models: { "mock-model": { name: "Mock Model" } },
          },
        },
      },
    },
    {
      label: "V1 provider key with OpenAI API alias",
      configKey: "provider",
      model: "openai/mock-model",
      config: {
        provider: {
          openai: {
            name: "Deterministic aimock",
            api: "openai",
            options: { baseURL: `${mock.url}/v1`, apiKey: "{env:OPENAI_API_KEY}" },
            models: { "mock-model": { name: "Mock Model" } },
          },
        },
      },
    },
    {
      label: "V1 npm OpenAI-compatible provider",
      configKey: "provider",
      model: "mock/mock-model",
      config: {
        provider: {
          mock: {
            name: "Deterministic aimock",
            npm: "@ai-sdk/openai-compatible",
            options: { baseURL: `${mock.url}/v1`, apiKey: "{env:OPENAI_API_KEY}" },
            models: { "mock-model": { name: "Mock Model" } },
          },
        },
      },
    },
  ];
  const observations = [];
  for (const [index, candidate] of candidates.entries()) {
    const before = await readMockRequests(mock.requestsPath);
    const result = await probeCandidate(candidate, index, root, executable);
    const after = await readMockRequests(mock.requestsPath);
    observations.push({
      ...result,
      mock_requests: after.length - before.length,
      observed_wire_paths: after.slice(before.length).map((request) => request.path).filter(Boolean),
    });
  }

  const accepted = observations[2];
  const acceptedPath = accepted.observed_wire_paths[0];
  if (
    accepted.exit_code !== 0 ||
    !acceptedPath?.endsWith("/v1/chat/completions") ||
    !accepted.observed_session.session_id ||
    accepted.observed_session.model?.providerID !== "mock" ||
    accepted.observed_session.model?.id !== "mock-model"
  ) {
    throw new Error(
      `the pinned V1 provider did not produce the expected chat session: exit=${accepted.exit_code}, paths=${JSON.stringify(accepted.observed_wire_paths)}, session=${JSON.stringify(accepted.observed_session)}\n${accepted.output.slice(-4000)}`,
    );
  }
  if (
    observations[0].exit_code !== 1 ||
    observations[0].mock_requests !== 0 ||
    !observations[0].diagnostic?.includes('kind=unsupported')
  ) {
    throw new Error("the V2 providers candidate changed on OpenCode 1; review the new outcome before updating the V1 contract");
  }
  if (
    observations[1].exit_code !== 0 ||
    observations[1].observed_wire_paths.length === 0 ||
    observations[1].observed_wire_paths.some((path) => !path.endsWith("/v1/responses"))
  ) {
    throw new Error("the V1 API-alias candidate changed its Responses behavior; review the new outcome before updating the V1 contract");
  }

  const providerRecord = {
    ...oldProvider,
    run_id: runId,
    observed_run_id: runId,
    capture_source_sha: sourceSha,
    host_version: version,
    transcript: providerTranscript,
    provider_config: {
      mock: {
        name: "Deterministic aimock",
        npm: "@ai-sdk/openai-compatible",
        options: { baseURL: "{{AIMOCK_BASE_URL}}/v1", apiKey: "{env:OPENAI_API_KEY}" },
        models: { "mock-model": { name: "Mock Model" } },
      },
    },
    opencode_json: candidates[2].config,
    run_command: ["opencode", "run", "--print-logs", "--format", "json", "--model", "mock/mock-model", "${message}"],
    observed_session: accepted.observed_session,
    observed_wire_path: acceptedPath,
    rejected_shapes: observations.slice(0, 2).map((observation, index) => ({
      config_key: candidates[index].configKey,
      shape: candidates[index].label,
      model: candidates[index].model,
      host_diagnostic: observation.diagnostic,
      host_error: observation.host_error,
      exit_code: observation.exit_code,
      mock_requests: observation.mock_requests,
      observed_wire_paths: observation.observed_wire_paths,
      rejected_because: index === 0
        ? "the OpenCode 1 host does not consume the V2 providers/package/settings key"
        : "the Responses API does not carry the chat messages the parity harness reads",
    })),
  };
  delete providerRecord.observed_image_sha;

  const gateRecord = {
    ...oldGate,
    run_id: runId,
    observed_run_id: runId,
    capture_source_sha: sourceSha,
    host_version: version,
    transcript: gateTranscript,
    executable: "opencode-ai/bin/opencode.exe",
    executable_sha256: sha256,
  };
  const providerText = renderProviderTranscript({ version, runId, sourceSha, mockUrl: mock.url, observations, candidates, root });
  const gateText = renderGateTranscript({ version, runId, sourceSha, sha256, gateRecord, root });

  await writeFile(join(contractDir, "host1-provider-config.json"), `${JSON.stringify(providerRecord, null, 2)}\n`);
  await writeFile(join(contractDir, "host1-edit-family-gate.json"), `${JSON.stringify(gateRecord, null, 2)}\n`);
  await writeFile(join(contractDir, providerTranscript), providerText);
  await writeFile(join(contractDir, gateTranscript), gateText);
  for (const oldPath of [oldProvider.transcript, oldGate.transcript]) {
    if (oldPath && oldPath !== providerTranscript && oldPath !== gateTranscript) {
      await rm(join(contractDir, oldPath), { force: true });
    }
  }
  console.log(`Captured OpenCode 1 ${version} provider config and edit-family gate: ${runId}`);
  console.log(`Provider wire path: ${acceptedPath}; rejected-shape outcomes are recorded in ${providerTranscript}`);
  console.log(`Edit-family fragments remain present in ${sha256.slice(0, 12)}…`);
} finally {
  if (mockProcess && mockProcess.exitCode === null) {
    mockProcess.kill("SIGTERM");
    await new Promise((resolveExit) => mockProcess.once("exit", resolveExit));
  }
  await rm(root, { recursive: true, force: true });
}

function isolatedEnvironment(base) {
  const dirs = {
    HOME: join(base, "home"),
    TMPDIR: join(base, "tmp"),
    XDG_CONFIG_HOME: join(base, "xdg-config"),
    XDG_DATA_HOME: join(base, "xdg-data"),
    XDG_STATE_HOME: join(base, "xdg-state"),
    XDG_CACHE_HOME: join(base, "xdg-cache"),
    XDG_RUNTIME_DIR: join(base, "xdg-runtime"),
  };
  const env = { ...process.env };
  for (const key of Object.keys(env)) {
    if (
      [
        "XDG_",
        "OPENCODE_",
        "ANTHROPIC_",
        "OPENAI_",
        "AZURE_",
        "AWS_",
        "GOOGLE_",
        "GEMINI_",
        "COPILOT_",
      ].some((prefix) => key.startsWith(prefix))
    ) {
      delete env[key];
    }
  }
  Object.assign(env, dirs, {
    OPENCODE_DISABLE_DEFAULT_PLUGINS: "true",
    OPENCODE_DB: "opencode2.db",
    OPENAI_API_KEY: "sk-opencode2-harness",
    npm_config_cache: join(dirs.XDG_CACHE_HOME, "npm"),
    NO_PROXY: "127.0.0.1,localhost",
    no_proxy: "127.0.0.1,localhost",
  });
  return mkdirDirs(dirs).then(() => chmod(dirs.XDG_RUNTIME_DIR, 0o700)).then(() => env);
}

async function mkdirDirs(dirs) {
  await Promise.all(Object.values(dirs).map((directory) => mkdir(directory, { recursive: true })));
}

async function probeCandidate(candidate, index, base, host) {
  const profile = join(base, `probe-${index}`);
  const env = await isolatedEnvironment(profile);
  const configDirectory = join(env.XDG_CONFIG_HOME, "opencode");
  const project = join(profile, "project");
  await mkdir(configDirectory, { recursive: true });
  await mkdir(project, { recursive: true });
  await writeFile(join(configDirectory, "opencode.json"), `${JSON.stringify(candidate.config, null, 2)}\n`);
  const result = await exec(
    host,
    ["run", "--print-logs", "--format", "json", "--model", candidate.model, "say hello"],
    { cwd: project, env: { ...env, PWD: project }, timeoutMs: 90_000 },
  );

  const output = `${result.stdout}\n${result.stderr}`;
  const stream = output.match(/stream providerID=([^\s]+) modelID=([^\s]+)/);
  const sessionId = output.match(/(?:created id=|sessionID[=:])([^\s,]+)/)?.[1];
  return {
    label: candidate.label,
    config_key: candidate.configKey,
    model: candidate.model,
    exit_code: result.exit_code,
    timed_out: result.timed_out,
    diagnostic: providerDiagnostic(output),
    host_error: output.match(/ProviderModelNotFoundError: Model not found: [^"]+/)?.[0],
    observed_session: {
      session_id: sessionId,
      model: stream ? { id: stream[2], providerID: stream[1] } : undefined,
      outcome: result.exit_code === 0 ? "succeeded" : "failed",
      run_exit_status: result.exit_code,
    },
    output: cleanOutput(output, base),
  };
}

async function startAimock(prefix, base) {
  const script = join(base, "aimock-server.mjs");
  const readyPath = join(base, "aimock-url.txt");
  const requestsPath = join(base, "aimock-requests.json");
  await writeFile(script, `
    import { createRequire } from "node:module";
    import { writeFile } from "node:fs/promises";
    const require = createRequire(import.meta.url);
    const { LLMock } = require(process.env.AIMOCK_PACKAGE);
    const mock = new LLMock({ port: 0 });
    mock.on({ predicate: (request) => JSON.stringify(request).includes("You are a title generator") }, { content: "Scenario title" });
    mock.on({ predicate: () => true }, { content: "Scenario complete" });
    await mock.start();
    await writeFile(process.env.AIMOCK_READY, mock.url);
    const save = () => writeFile(process.env.AIMOCK_REQUESTS, JSON.stringify(mock.getRequests()));
    const timer = setInterval(() => { void save(); }, 100);
    const stop = async () => {
      clearInterval(timer);
      await save();
      await mock.stop();
      process.exit(0);
    };
    process.on("SIGTERM", () => { void stop(); });
    process.on("SIGINT", () => { void stop(); });
  `);
  const env = await isolatedEnvironment(join(base, "aimock-runtime"));
  Object.assign(env, {
    AIMOCK_PACKAGE: join(prefix, "node_modules", "@copilotkit", "aimock"),
    AIMOCK_READY: readyPath,
    AIMOCK_REQUESTS: requestsPath,
  });
  const child = spawn(process.execPath, [script], {
    cwd: base,
    env,
    stdio: ["ignore", "pipe", "pipe"],
    detached: process.platform !== "win32",
  });
  let output = "";
  child.stdout.setEncoding("utf8").on("data", (chunk) => { output += chunk; });
  child.stderr.setEncoding("utf8").on("data", (chunk) => { output += chunk; });
  for (let attempt = 0; attempt < 200; attempt += 1) {
    try {
      const url = (await readFile(readyPath, "utf8")).trim();
      return { process: child, url, requestsPath };
    } catch {
      if (child.exitCode !== null) throw new Error(`aimock exited before listening: ${output}`);
      await new Promise((resolveDelay) => setTimeout(resolveDelay, 50));
    }
  }
  child.kill("SIGTERM");
  throw new Error(`aimock did not listen within 10 seconds: ${output}`);
}

async function readMockRequests(path) {
  try {
    const values = JSON.parse(await readFile(path, "utf8"));
    if (!Array.isArray(values)) return [];
    return values.map((entry) => {
      const request = entry?.request ?? entry;
      const rawPath = request?.path ?? request?.url ?? request?.pathname ?? entry?.url ?? entry?.path;
      let requestPath;
      if (typeof rawPath === "string") {
        try {
          requestPath = new URL(rawPath, "http://127.0.0.1").pathname;
        } catch {
          requestPath = rawPath;
        }
      }
      return { path: requestPath, entry };
    });
  } catch {
    return [];
  }
}

function cleanOutput(output, base) {
  return output
    .replaceAll(base, "<capture-root>")
    .replaceAll(/sk-[A-Za-z0-9_-]+/g, "sk-opencode2-harness")
    .trim();
}

function providerDiagnostic(output) {
  const line = output.match(/configuration compatibility diagnostic[^\r\n]*/)?.[0];
  if (!line) return undefined;
  const encodedPath = line.match(/path="((?:\\.|[^"])*)"/)?.[1];
  const kind = line.match(/kind=([^\s]+)/)?.[1];
  const action = line.match(/action="([^"]*)"/)?.[1];
  const fields = [
    "configuration compatibility diagnostic",
    encodedPath ? `path=${encodedPath.replaceAll('\\"', '"')}` : undefined,
    kind ? `kind=${kind}` : undefined,
    action ? `action="${action}"` : undefined,
  ].filter(Boolean);
  return fields.join(" ");
}

function renderProviderTranscript({ version, runId, sourceSha, mockUrl, observations, candidates, root }) {
  const blocks = observations.map((observation, index) => {
    const normalized = {
      ...observation,
      output: observation.output.replaceAll(mockUrl, "http://127.0.0.1:<mock>")
        .replaceAll(root, "<capture-root>"),
    };
    return [
      `${index + 1}. ${candidates[index].label}`,
      `config key: ${candidates[index].configKey}; model: ${candidates[index].model}`,
      `config:\n${JSON.stringify(candidates[index].config, null, 2).replaceAll(mockUrl, "http://127.0.0.1:<mock>")}`,
      `exit: ${normalized.exit_code}; mock requests: ${normalized.mock_requests}; paths: ${JSON.stringify(normalized.observed_wire_paths)}`,
      normalized.diagnostic ? `diagnostic: ${normalized.diagnostic}` : "diagnostic: none",
      normalized.host_error ? `host error: ${normalized.host_error}` : "host error: none",
      normalized.output ? `host output:\n${normalized.output}` : "host output: <empty>",
    ].join("\n");
  });
  return [
    "OpenCode 1 provider configuration probe for the T7 parity leg",
    `run_id: ${runId}`,
    `source_sha: ${sourceSha}`,
    `binary: opencode-ai@${version} (${process.platform}-${process.arch})`,
    "isolation: throwaway HOME, TMPDIR, XDG_CONFIG_HOME, XDG_DATA_HOME, XDG_STATE_HOME, XDG_CACHE_HOME, XDG_RUNTIME_DIR; OPENCODE_DB=opencode2.db; OPENCODE_DISABLE_DEFAULT_PLUGINS=true",
    "service: foreground opencode run; private XDG state and process cleanup prevent a managed background service from using user state",
    "provider mock: @copilotkit/aimock@1.24.0 on loopback; fake OpenAI key only",
    "",
    ...blocks,
    "",
    `Accepted provider config: ${JSON.stringify(candidates[2].config, null, 2).replaceAll(mockUrl, "http://127.0.0.1:<mock>")}`,
    "The contract uses {{AIMOCK_BASE_URL}} so each private harness scenario can materialize its own local endpoint.",
    "",
  ].join("\n");
}

function renderGateTranscript({ version, runId, sourceSha, sha256, gateRecord, root }) {
  return [
    "OpenCode 1 edit-family selector capture",
    `run_id: ${runId}`,
    `source_sha: ${sourceSha}`,
    `binary: opencode-ai@${version} (${process.platform}-${process.arch})`,
    `binary_sha256: ${sha256}`,
    "isolation: throwaway HOME and all XDG roots; no host service started for the bundle inspection",
    `bundle: opencode-ai/bin/opencode.exe (temporary prefix ${root.replaceAll(root, "<capture-root>")})`,
    "",
    `registry_function: ${gateRecord.registry_function}`,
    `selector_source: ${gateRecord.selector_source}`,
    ...gateRecord.tool_selectors.map(({ tool, source }) => `tool selector ${tool}: ${source}`),
    "",
    "Every fragment above was found byte-for-byte in the installed host bundle. The model-id selector still enables apply_patch for eligible GPT names and edit/write otherwise.",
    "",
  ].join("\n");
}

async function exec(command, args, { cwd, env = process.env, timeoutMs = 60_000 } = {}) {
  return new Promise((resolveResult, rejectResult) => {
    const child = spawn(command, args, {
      cwd,
      env,
      stdio: ["ignore", "pipe", "pipe"],
      detached: process.platform !== "win32",
    });
    let stdout = "";
    let stderr = "";
    let timedOut = false;
    const timer = setTimeout(() => {
      timedOut = true;
      try {
        if (child.pid && process.platform !== "win32") process.kill(-child.pid, "SIGTERM");
        else child.kill("SIGTERM");
      } catch {}
      setTimeout(() => {
        try {
          if (child.pid && process.platform !== "win32") process.kill(-child.pid, "SIGKILL");
          else child.kill("SIGKILL");
        } catch {}
      }, 2_000).unref();
    }, timeoutMs);
    child.stdout.setEncoding("utf8").on("data", (chunk) => { stdout += chunk; });
    child.stderr.setEncoding("utf8").on("data", (chunk) => { stderr += chunk; });
    child.once("error", (error) => {
      clearTimeout(timer);
      rejectResult(error);
    });
    child.once("close", (code) => {
      clearTimeout(timer);
      if (child.pid && process.platform !== "win32") {
        try {
          process.kill(-child.pid, "SIGTERM");
        } catch {}
      }
      const result = { stdout, stderr, exit_code: code ?? 1, timed_out: timedOut };
      if (timedOut) rejectResult(new Error(`${command} timed out after ${timeoutMs} ms\n${stderr}`));
      else resolveResult(result);
    });
  });
}
