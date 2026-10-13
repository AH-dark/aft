import { onTestFinished } from "bun:test";

type EnvOverrides = Record<string, string | undefined>;
type EnvSnapshot = Map<string, string | undefined>;

let envLockTail = Promise.resolve();

async function acquireEnvMutex(): Promise<() => void> {
  let releaseNext!: () => void;
  const waitForPrevious = envLockTail;
  envLockTail = new Promise<void>((resolve) => {
    releaseNext = resolve;
  });

  await waitForPrevious;

  let released = false;
  return () => {
    if (released) return;
    released = true;
    releaseNext();
  };
}

function snapshotEnv(overrides: EnvOverrides): EnvSnapshot {
  const snapshot: EnvSnapshot = new Map();
  for (const key of Object.keys(overrides)) {
    snapshot.set(key, process.env[key]);
  }
  return snapshot;
}

function applyEnv(values: EnvOverrides | EnvSnapshot): void {
  for (const [key, value] of values instanceof Map ? values : Object.entries(values)) {
    if (value === undefined) {
      delete process.env[key];
    } else {
      process.env[key] = value;
    }
  }
}

export async function withEnv<T>(overrides: EnvOverrides, fn: () => T | Promise<T>): Promise<T> {
  const release = await acquireEnv(overrides);
  try {
    return await fn();
  } finally {
    release();
  }
}

export async function acquireEnv(overrides: EnvOverrides): Promise<() => void> {
  let releaseMutex: (() => void) | undefined;
  let snapshot: EnvSnapshot | undefined;
  let released = false;
  const release = () => {
    if (released) return;
    released = true;
    if (snapshot) applyEnv(snapshot);
    releaseMutex?.();
  };

  // A test may time out while its async fixture is still pending. Bun still
  // runs this finish hook, so the next fixture can safely acquire the env lock.
  onTestFinished(release);
  releaseMutex = await acquireEnvMutex();
  if (released) {
    releaseMutex();
    return release;
  }

  snapshot = snapshotEnv(overrides);
  applyEnv(overrides);
  return release;
}
