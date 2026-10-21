import type { BridgeClock, BridgeTimer } from "../../bridge.js";

/**
 * Bridge clock that only moves when a test calls `advance`, so request
 * deadlines and the hang watchdog fire at exact, load-independent points.
 * The starting time is arbitrary.
 */
export class ManualClock implements BridgeClock {
  private current: number;
  private nextId = 1;
  private readonly timers = new Map<number, { due: number; callback: () => void }>();

  constructor(start = 1_000_000) {
    this.current = start;
  }

  now(): number {
    return this.current;
  }

  setTimeout(callback: () => void, ms: number): BridgeTimer {
    const id = this.nextId++;
    this.timers.set(id, { due: this.current + Math.max(0, ms), callback });
    return id;
  }

  clearTimeout(timer: BridgeTimer): void {
    this.timers.delete(timer as number);
  }

  /**
   * Move time forward by `ms`, firing every timer that falls due on the way in
   * deadline order (ties in creation order), with `now()` set to each timer's
   * deadline while it runs. Timers scheduled by those callbacks fire too if
   * they fall due within the same advance.
   */
  advance(ms: number): void {
    const target = this.current + ms;
    for (;;) {
      let nextId: number | undefined;
      let nextDue = Number.POSITIVE_INFINITY;
      for (const [id, timer] of this.timers) {
        if (timer.due <= target && timer.due < nextDue) {
          nextDue = timer.due;
          nextId = id;
        }
      }
      if (nextId === undefined) break;
      const timer = this.timers.get(nextId);
      this.timers.delete(nextId);
      this.current = nextDue;
      timer?.callback();
    }
    this.current = target;
  }
}
