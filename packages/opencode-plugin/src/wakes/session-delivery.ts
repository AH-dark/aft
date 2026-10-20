import { Effect, Exit } from "effect";
import { warn } from "../logger.js";

export const MAX_ADMITTED_WAKES = 4096;

export class WakeDeliveryOverflow extends Error {
  readonly type = "wake_delivery_overflow";
  constructor() {
    super(`wake_delivery_overflow: ${MAX_ADMITTED_WAKES} unretired wake keys; admission deferred`);
  }
}

/**
 * The `metadata.source` marker for AFT-written session input. Synthetic wakes
 * retain it, and the prompt hook recognises it on machine-originated prompts
 * so they are not treated as text an operator typed.
 */
export const AFT_PROMPT_SOURCE = "aft";

/** True when a prompt's metadata marks it as one AFT admitted itself. */
export function isAftOriginatedPrompt(metadata: unknown): boolean {
  return (
    typeof metadata === "object" &&
    metadata !== null &&
    (metadata as { source?: unknown }).source === AFT_PROMPT_SOURCE
  );
}

export interface V2SessionPromptInput {
  readonly sessionID: string;
  readonly text: string;
  readonly delivery: "steer";
  readonly metadata?: Readonly<Record<string, unknown>>;
  readonly resume?: boolean;
}

export interface V2SessionSyntheticInput {
  readonly sessionID: string;
  readonly text: string;
  readonly description?: string;
  readonly metadata?: Readonly<Record<string, unknown>>;
  readonly delivery?: "steer" | "queue";
  readonly resume?: boolean;
}

export interface V2SessionAdmission {
  get(input: { readonly sessionID: string }): Effect.Effect<{ readonly revert?: unknown }, unknown>;
  prompt(input: V2SessionPromptInput): Effect.Effect<unknown, unknown>;
  synthetic(input: V2SessionSyntheticInput): Effect.Effect<unknown, unknown>;
}

export interface CompletionWake {
  readonly sessionID: string;
  readonly taskIDs: readonly string[];
  readonly text: string;
  readonly metadata?: Readonly<Record<string, unknown>>;
  /** Distinguish a watch match from the later terminal completion of the same task. */
  readonly notificationID?: string;
}

export interface PatternMatchWake extends CompletionWake {
  readonly watchID: string;
  readonly matchOffset: number;
  readonly reason?: string;
}

export interface StatusOnlyUpdate {
  readonly sessionID: string;
  readonly text: string;
  readonly description?: string;
  readonly metadata?: Readonly<Record<string, unknown>>;
}

/**
 * Admit V2 session updates as synthetic input so machine-written notices are
 * not rendered as user turns. Completions and pattern matches wake execution;
 * status does not.
 */
export class V2SessionDelivery {
  private readonly admittedWakes = new Set<string>();
  private disposed = false;

  constructor(private readonly session: V2SessionAdmission) {}

  completion(input: CompletionWake): Effect.Effect<boolean, unknown> {
    return this.admitWake(
      input,
      [...new Set(input.taskIDs)].map(
        (taskID) => `${input.sessionID}\0${taskID}\0${input.notificationID ?? "completion"}`,
      ),
      "Background bash completed",
    );
  }

  patternMatch(input: PatternMatchWake): Effect.Effect<boolean, unknown> {
    // A sticky watch can match repeatedly before the task completes. Neither
    // its later offsets nor the task's completion may share a delivery key.
    return this.admitWake(input, [this.patternKey(input)], "Background bash pattern matched");
  }

  private patternKey(input: PatternMatchWake): string {
    return JSON.stringify([
      "pattern",
      input.sessionID,
      input.taskIDs,
      input.watchID,
      input.matchOffset,
      input.reason,
    ]);
  }

  // The consumer checks durable pending matches before admitting a replay, so
  // an acknowledged or superseded match needs no permanent in-memory tombstone.
  retirePatternMatch(input: PatternMatchWake): void {
    this.admittedWakes.delete(this.patternKey(input));
  }

  retireCompletion(input: Pick<CompletionWake, "sessionID" | "taskIDs">): void {
    for (const taskID of input.taskIDs) this.admittedWakes.delete(`${input.sessionID}\0${taskID}`);
  }

  private admitWake(
    input: CompletionWake,
    keys: string[],
    description: string,
  ): Effect.Effect<boolean, unknown> {
    return Effect.suspend(() => {
      if (this.disposed) return Effect.succeed(false);
      if (keys.length === 0 || keys.every((key) => this.admittedWakes.has(key))) {
        return Effect.succeed(false);
      }
      const reserved = keys.filter((key) => !this.admittedWakes.has(key));
      if (this.admittedWakes.size + reserved.length > MAX_ADMITTED_WAKES) {
        const error = new WakeDeliveryOverflow();
        warn(
          JSON.stringify({
            type: error.type,
            cap: MAX_ADMITTED_WAKES,
            session_id: input.sessionID,
          }),
        );
        // Evicting an existing key could admit the same pattern or completion twice.
        return Effect.fail(error);
      }
      for (const key of reserved) this.admittedWakes.add(key);

      return Effect.suspend(() =>
        this.session.get({ sessionID: input.sessionID }).pipe(
          Effect.flatMap((session) => {
            // A staged revert can later delete synthetic input after its
            // boundary. Keep the wake unacknowledged and retryable instead.
            if (session.revert) {
              return Effect.fail(
                new Error("Background bash wake deferred: session has a staged revert"),
              );
            }
            return this.session.synthetic({
              sessionID: input.sessionID,
              text: input.text,
              description,
              delivery: "steer",
              metadata: {
                ...input.metadata,
                // Keep the origin marker even if the caller supplies its own source.
                source: AFT_PROMPT_SOURCE,
                task_ids: [...new Set(input.taskIDs)],
              },
            });
          }),
        ),
      ).pipe(
        Effect.onExit((exit) =>
          Effect.sync(() => {
            // Reserve while admission is in flight, but a failed or interrupted
            // admission must not suppress the bridge's next delivery attempt.
            if (Exit.isFailure(exit)) for (const key of reserved) this.admittedWakes.delete(key);
          }),
        ),
        Effect.map(() => true),
      );
    });
  }

  status(input: StatusOnlyUpdate): Effect.Effect<void, unknown> {
    return Effect.suspend(() => {
      if (this.disposed) return Effect.succeed(undefined);
      return this.session
        .synthetic({
          sessionID: input.sessionID,
          text: input.text,
          description: input.description,
          metadata: input.metadata,
          resume: false,
        })
        .pipe(Effect.asVoid);
    });
  }

  dispose(): void {
    if (this.disposed) return;
    this.disposed = true;
    this.admittedWakes.clear();
  }
}
