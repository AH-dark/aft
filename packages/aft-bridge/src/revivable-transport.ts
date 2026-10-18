import { warn } from "./active-logger.js";
import { asCanonicalRootPath, type CanonicalRootPath } from "./lifecycle-registry.js";
import { canonicalizeProjectRoot } from "./project-identity.js";
import type { BgNudgeRef } from "./subc-transport.js";
import type {
  AftProjectTransport,
  AftTransportPool,
  ToolCallArguments,
  ToolCallOptions,
  ToolCallResult,
} from "./transport.js";

interface StatusSubscribableBridge {
  subscribeStatus(listener: (snapshot: Record<string, unknown>) => void): () => void;
}

type StatusListener = (snapshot: Record<string, unknown>) => void;

type PoolFactory = () => Promise<AftTransportPool>;

/** After a failed terminal-pool revival, wait before retrying so repeated traffic cannot immediately start another connection attempt. */
const REVIVAL_RETRY_FLOOR_MS = 100;
const REVIVAL_RETRY_CAP_MS = 2_000;
/** Matches the subc pool's own bound on unused fresh-session hints. */
const MAX_PENDING_SESSION_STARTS = 256;

/**
 * Owns one transport instance (or defers its creation) and replaces it when new demand arrives
 * after the host has shut it down. The replaced instance is never reused: its
 * routes, sessions, and sockets remain owned by the dead instance.
 */
export class RevivableTransportPool implements AftTransportPool {
  private activePool: AftTransportPool | null;
  private revival: Promise<AftTransportPool> | null = null;
  private revivalRetryDelayMs = REVIVAL_RETRY_FLOOR_MS;
  private revivalRetryNotBefore = 0;
  private shutdownReason: string | null = null;
  private readonly statusTransports = new Set<RevivableProjectTransport>();
  private readonly evictionListeners = new Set<(root: string) => void>();
  private unsubscribeEvictions: (() => void) | undefined;
  private cancelRevivalDelay: (() => void) | undefined;
  private readonly transports = new Map<string, RevivableProjectTransport>();
  private readonly configureOverrides = new Map<string, unknown>();
  private editSlotSurvivesCaptured = false;
  private readonly pendingSessionStarts = new Map<string, [string, string]>();

  constructor(
    // A null initial pool defers construction until the first request. Hosts
    // can register tools without resolving/downloading a standalone binary.
    initialPool: AftTransportPool | null,
    private readonly createPool: PoolFactory,
    private readonly onBinaryReplaced?: (path: string) => void,
    private readonly onDemand?: (pool: AftTransportPool, canonicalRoot: CanonicalRootPath) => void,
  ) {
    this.activePool = initialPool;
    this.bindEvictions(initialPool);
  }

  getBridge(projectRoot: string): RevivableProjectTransport {
    return this.getBridgeForCanonicalRoot(
      asCanonicalRootPath(canonicalizeProjectRoot(projectRoot)),
    );
  }

  getBridgeForCanonicalRoot(key: CanonicalRootPath): RevivableProjectTransport {
    if (this.activePool && !this.activePool.isShutdown()) this.onDemand?.(this.activePool, key);
    let transport = this.transports.get(key);
    if (!transport) {
      transport = new RevivableProjectTransport(this, key);
      this.transports.set(key, transport);
    }
    return transport;
  }

  subscribeBridgeEvictions(listener: (root: string) => void): () => void {
    this.evictionListeners.add(listener);
    return () => {
      this.evictionListeners.delete(listener);
    };
  }

  private bindEvictions(pool: AftTransportPool | null): void {
    this.unsubscribeEvictions?.();
    this.unsubscribeEvictions = pool?.subscribeBridgeEvictions?.((root) =>
      this.evictInactiveFacade(root),
    );
  }

  /** Ignore a late eviction from an earlier pool when the root already has a live replacement bridge. */
  evictInactiveFacade(root: string): void {
    if (this.currentBridge(root) !== null) return;
    this.refreshRootStatus(root, null);
    this.transports.delete(root);
    for (const listener of this.evictionListeners) listener(root);
  }

  trackStatusFacade(transport: RevivableProjectTransport, tracked: boolean): void {
    if (tracked) this.statusTransports.add(transport);
    else this.statusTransports.delete(transport);
  }

  private refreshRootStatus(root: string, bridge: AftProjectTransport | null): void {
    this.transports.get(root)?.refreshStatusSubscription(bridge);
    for (const transport of this.statusTransports) {
      if (transport.getCwd() === root) transport.refreshStatusSubscription(bridge);
    }
  }

  observeSessionStart(projectRoot: string, session: string): void {
    if (this.activePool && !this.activePool.isShutdown()) {
      this.activePool.observeSessionStart?.(projectRoot, session);
      return;
    }
    // Hosts report a fresh session before the lazily created pool exists. Keep
    // the hint for the pool that serves the session's first call; without it the
    // session would count as unobserved and could never be reclaimed when idle.
    const key = `${projectRoot}\u0000${session}`;
    this.pendingSessionStarts.delete(key);
    this.pendingSessionStarts.set(key, [projectRoot, session]);
    if (this.pendingSessionStarts.size > MAX_PENDING_SESSION_STARTS) {
      const oldest = this.pendingSessionStarts.keys().next().value;
      if (oldest !== undefined) this.pendingSessionStarts.delete(oldest);
    }
  }

  /** Delegate to the active pool: session-scoped signal fan-out reaches the
   * same live transports the underlying pool would report. */
  activeBridges(): AftProjectTransport[] {
    return this.activePool?.activeBridges() ?? [];
  }

  getActiveBridgeForRoot(projectRoot: string): RevivableProjectTransport | null {
    const key = canonicalizeProjectRoot(projectRoot);
    const bridge = this.currentBridge(key);
    if (!bridge) return null;
    const transport = this.getBridge(key);
    transport.refreshStatusSubscription(bridge);
    return transport;
  }

  async toolCall(
    projectRoot: string,
    runtime: { sessionID?: string },
    name: string,
    rawArgs: ToolCallArguments = {},
    options?: ToolCallOptions,
  ): Promise<ToolCallResult> {
    return this.toolCallForCanonicalRoot(
      asCanonicalRootPath(canonicalizeProjectRoot(projectRoot)),
      runtime.sessionID,
      name,
      rawArgs,
      options,
    );
  }

  setConfigureOverride(key: string, value: unknown): void {
    if (key === "edit_slot_survives") {
      if (typeof value !== "boolean") {
        throw new Error("edit_slot_survives must be set once to a boolean");
      }
      if (this.editSlotSurvivesCaptured) {
        throw new Error("edit_slot_survives is write-once and was already captured");
      }
      this.activePool?.setConfigureOverride(key, value);
      this.editSlotSurvivesCaptured = true;
      this.configureOverrides.set(key, value);
      return;
    }

    if (value === undefined) this.configureOverrides.delete(key);
    else this.configureOverrides.set(key, value);
    this.activePool?.setConfigureOverride(key, value);
  }

  async reconfigure(projectRoot: string, overrides: Record<string, unknown>): Promise<void> {
    if ((!this.activePool || this.activePool.isShutdown()) && !this.revival) {
      // Reconfigure only updates live bridges, and a shut-down pool has none.
      // Reviving here would leave a live pool behind after the host's shutdown
      // hook finished: background work that settles during shutdown (an LSP
      // install aborted by that hook, for example) then keeps an unowned pool
      // around and a headless host never exits. Record the overrides instead;
      // a later revival applies them to the replacement pool.
      for (const [key, value] of Object.entries(overrides)) {
        this.setConfigureOverride(key, value);
      }
      return;
    }
    const pool = await this.ensureActivePool();
    const runtimeOverrides: Record<string, unknown> = {};
    for (const [key, value] of Object.entries(overrides)) {
      if (key === "edit_slot_survives") {
        this.setConfigureOverride(key, value);
        continue;
      }
      if (value === undefined) this.configureOverrides.delete(key);
      else this.configureOverrides.set(key, value);
      runtimeOverrides[key] = value;
    }
    await pool.reconfigure(projectRoot, runtimeOverrides);
  }

  async replaceBinary(path: string): Promise<string> {
    const replaced = this.activePool ? await this.activePool.replaceBinary(path) : path;
    for (const transport of new Set([...this.transports.values(), ...this.statusTransports])) {
      transport.refreshStatusSubscription(this.currentBridge(transport.getCwd()));
    }
    this.onBinaryReplaced?.(replaced);
    return replaced;
  }

  closeSession(projectRoot: string, session: string): Promise<void> {
    return this.activePool?.closeSession(projectRoot, session) ?? Promise.resolve();
  }

  async shutdown(reason = "unknown"): Promise<void> {
    this.cancelRevivalDelay?.();
    const revival = this.revival;
    if (revival) {
      await Promise.allSettled([revival]);
    }
    this.shutdownReason = reason;
    await this.activePool?.shutdown(reason);
    for (const transport of new Set([...this.transports.values(), ...this.statusTransports])) {
      transport.refreshStatusSubscription(null);
    }
  }

  isShutdown(): boolean {
    return this.activePool?.isShutdown() ?? this.shutdownReason !== null;
  }

  async ensureActivePool(): Promise<AftTransportPool> {
    if (this.activePool && !this.activePool.isShutdown()) return this.activePool;
    if (this.revival) return this.revival;

    const delay = Math.max(0, this.revivalRetryNotBefore - Date.now());
    if (delay > 0) {
      let scheduled!: Promise<AftTransportPool>;
      scheduled = new Promise<void>((resolve, reject) => {
        const timer = setTimeout(() => {
          this.cancelRevivalDelay = undefined;
          resolve();
        }, delay);
        this.cancelRevivalDelay = () => {
          clearTimeout(timer);
          this.cancelRevivalDelay = undefined;
          if (this.revival === scheduled) this.revival = null;
          reject(new Error("Transport revival cancelled by shutdown"));
        };
      }).then(() => {
        if (this.revival === scheduled) this.revival = null;
        return this.ensureActivePool();
      });
      this.revival = scheduled;
      void scheduled.then(
        () => undefined,
        () => undefined,
      );
      return scheduled;
    }

    if (this.activePool || this.shutdownReason !== null) {
      warn(
        `transport was shut down (reason: ${this.shutdownReason ?? "unknown"}) but new demand arrived — reviving`,
      );
    }
    const revival = Promise.resolve()
      .then(() => this.createPool())
      .then((pool) => {
        for (const [key, value] of this.configureOverrides) {
          pool.setConfigureOverride(key, value);
        }
        this.activePool = pool;
        this.bindEvictions(pool);
        for (const [root, session] of this.pendingSessionStarts.values()) {
          pool.observeSessionStart?.(root, session);
        }
        this.pendingSessionStarts.clear();
        this.shutdownReason = null;
        this.revivalRetryDelayMs = REVIVAL_RETRY_FLOOR_MS;
        this.revivalRetryNotBefore = 0;
        for (const transport of new Set([...this.transports.values(), ...this.statusTransports])) {
          transport.refreshStatusSubscription(pool.getActiveBridgeForRoot(transport.getCwd()));
        }
        return pool;
      });
    this.revival = revival;
    revival.then(
      () => {
        if (this.revival === revival) this.revival = null;
      },
      () => {
        // A terminal pool is parked after a failed revival. Concurrent and newly
        // arriving calls share the one delayed retry instead of recursively
        // starting connections or registering fresh listeners on every call.
        this.revivalRetryNotBefore = Date.now() + this.revivalRetryDelayMs;
        this.revivalRetryDelayMs = Math.min(this.revivalRetryDelayMs * 2, REVIVAL_RETRY_CAP_MS);
        if (this.revival === revival) this.revival = null;
      },
    );
    return revival;
  }

  /**
   * Return only the existing outer facade that matches a live concrete nudge.
   * Unlike getActiveBridgeForRoot, this path never creates a wrapper facade or
   * asks the active pool for demand.
   */
  getActiveBridgeForRootGeneration(ref: BgNudgeRef): RevivableProjectTransport | null {
    const concretePool = this.activePool as AftTransportPool & {
      getActiveBridgeForRootGeneration?: (value: BgNudgeRef) => AftProjectTransport | null;
    };
    if (!concretePool?.getActiveBridgeForRootGeneration?.(ref)) return null;
    return this.transports.get(ref.canonicalRoot) ?? null;
  }

  recordBgNudgeRejection(ref: BgNudgeRef): void {
    const pool = this.activePool as AftTransportPool & {
      recordBgNudgeRejection?: (value: BgNudgeRef) => void;
    };
    pool?.recordBgNudgeRejection?.(ref);
  }

  getCurrentRootGeneration(root: string): BgNudgeRef["generation"] | undefined {
    const pool = this.activePool as AftTransportPool & {
      getCurrentRootGeneration?: (value: string) => BgNudgeRef["generation"] | undefined;
    };
    return pool?.getCurrentRootGeneration?.(root);
  }

  getConcretePoolId(): BgNudgeRef["concretePoolId"] | undefined {
    const pool = this.activePool as AftTransportPool & {
      getConcretePoolId?: () => BgNudgeRef["concretePoolId"] | undefined;
    };
    return pool?.getConcretePoolId?.();
  }

  currentBridge(projectRoot: string): AftProjectTransport | null {
    return this.activePool?.getActiveBridgeForRoot(projectRoot) ?? null;
  }

  async send(
    projectRoot: string,
    command: string,
    params?: Record<string, unknown>,
    options?: Parameters<AftProjectTransport["send"]>[2],
  ): Promise<Record<string, unknown>> {
    return this.sendForCanonicalRoot(
      asCanonicalRootPath(canonicalizeProjectRoot(projectRoot)),
      command,
      params,
      options,
    );
  }

  async sendForCanonicalRoot(
    projectRoot: CanonicalRootPath,
    command: string,
    params?: Record<string, unknown>,
    options?: Parameters<AftProjectTransport["send"]>[2],
  ): Promise<Record<string, unknown>> {
    this.getBridgeForCanonicalRoot(projectRoot);
    const pool = await this.ensureActivePool();
    const bridge = pool.getBridgeForCanonicalRoot?.(projectRoot) ?? pool.getBridge(projectRoot);
    this.refreshRootStatus(projectRoot, bridge);
    return bridge.send(command, params, options);
  }

  async toolCallOnProject(
    projectRoot: string,
    sessionId: string | undefined,
    name: string,
    rawArgs?: ToolCallArguments,
    options?: ToolCallOptions,
  ): Promise<ToolCallResult> {
    return this.toolCallForCanonicalRoot(
      asCanonicalRootPath(canonicalizeProjectRoot(projectRoot)),
      sessionId,
      name,
      rawArgs,
      options,
    );
  }

  async toolCallForCanonicalRoot(
    projectRoot: CanonicalRootPath,
    sessionId: string | undefined,
    name: string,
    rawArgs?: ToolCallArguments,
    options?: ToolCallOptions,
  ): Promise<ToolCallResult> {
    this.getBridgeForCanonicalRoot(projectRoot);
    const pool = await this.ensureActivePool();
    const bridge = pool.getBridgeForCanonicalRoot?.(projectRoot) ?? pool.getBridge(projectRoot);
    this.refreshRootStatus(projectRoot, bridge);
    return bridge.toolCall(sessionId, name, rawArgs, options);
  }
}

/**
 * Stable project facade returned to callers so a facade acquired before a host
 * quit hook still routes the next call through the replacement pool.
 */
class RevivableProjectTransport implements AftProjectTransport {
  private readonly statusListeners = new Map<StatusListener, () => void>();
  private readonly statusBridges = new Map<StatusListener, AftProjectTransport | null>();

  constructor(
    private readonly owner: RevivableTransportPool,
    private readonly projectRoot: string,
  ) {}

  getCwd(): string {
    return this.projectRoot;
  }

  getCachedStatus() {
    return this.owner.currentBridge(this.projectRoot)?.getCachedStatus() ?? null;
  }

  cacheStatusSnapshot(snapshot: Parameters<AftProjectTransport["cacheStatusSnapshot"]>[0]): void {
    this.owner.currentBridge(this.projectRoot)?.cacheStatusSnapshot(snapshot);
  }

  send(
    command: string,
    params?: Record<string, unknown>,
    options?: Parameters<AftProjectTransport["send"]>[2],
  ): Promise<Record<string, unknown>> {
    return this.owner.send(this.projectRoot, command, params, options);
  }

  toolCall(
    sessionId: string | undefined,
    name: string,
    rawArgs?: ToolCallArguments,
    options?: ToolCallOptions,
  ): Promise<ToolCallResult> {
    return this.owner.toolCallOnProject(this.projectRoot, sessionId, name, rawArgs, options);
  }

  subscribeStatus(listener: StatusListener): () => void {
    if (this.statusListeners.has(listener)) return () => this.removeStatusListener(listener);
    this.statusListeners.set(listener, () => {});
    this.owner.trackStatusFacade(this, true);
    this.bindStatusListener(listener, this.owner.currentBridge(this.projectRoot));
    return () => this.removeStatusListener(listener);
  }

  refreshStatusSubscription(bridge: AftProjectTransport | null): void {
    for (const listener of this.statusListeners.keys()) {
      this.bindStatusListener(listener, bridge);
    }
  }

  private bindStatusListener(listener: StatusListener, bridge: AftProjectTransport | null): void {
    if (this.statusBridges.get(listener) === bridge) return;
    const previousUnsubscribe = this.statusListeners.get(listener);
    previousUnsubscribe?.();
    const maybe = bridge as (AftProjectTransport & Partial<StatusSubscribableBridge>) | null;
    const unsubscribe =
      maybe && typeof maybe.subscribeStatus === "function"
        ? maybe.subscribeStatus(listener)
        : () => {};
    this.statusListeners.set(listener, unsubscribe);
    this.statusBridges.set(listener, bridge);
  }

  private removeStatusListener(listener: StatusListener): void {
    const unsubscribe = this.statusListeners.get(listener);
    this.statusListeners.delete(listener);
    this.statusBridges.delete(listener);
    if (this.statusListeners.size === 0) this.owner.trackStatusFacade(this, false);
    unsubscribe?.();
  }
}
