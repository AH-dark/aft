import { Effect, Exit } from "effect";

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
}

export interface StatusOnlyUpdate {
  readonly sessionID: string;
  readonly text: string;
  readonly description?: string;
  readonly metadata?: Readonly<Record<string, unknown>>;
}

/**
 * Admit V2 session updates as synthetic input so machine-written notices are
 * not rendered as user turns. Completions wake execution; status does not.
 */
export class V2SessionDelivery {
  private readonly admittedCompletions = new Set<string>();
  private disposed = false;

  constructor(private readonly session: V2SessionAdmission) {}

  completion(input: CompletionWake): Effect.Effect<boolean, unknown> {
    return Effect.suspend(() => {
      if (this.disposed) return Effect.succeed(false);
      const keys = [...new Set(input.taskIDs)].map((taskID) => `${input.sessionID}\0${taskID}`);
      if (keys.length === 0 || keys.every((key) => this.admittedCompletions.has(key))) {
        return Effect.succeed(false);
      }
      const reserved = keys.filter((key) => !this.admittedCompletions.has(key));
      for (const key of reserved) this.admittedCompletions.add(key);

      return Effect.suspend(() =>
        this.session.get({ sessionID: input.sessionID }).pipe(
          Effect.flatMap((session) => {
            // A committed revert deletes synthetic input admitted after its
            // boundary. Keep the completion unacknowledged and retryable instead.
            if (session.revert) {
              return Effect.fail(
                new Error("Background bash completion deferred: session has a staged revert"),
              );
            }
            return this.session.synthetic({
              sessionID: input.sessionID,
              text: input.text,
              description: "Background bash completed",
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
            if (Exit.isFailure(exit))
              for (const key of reserved) this.admittedCompletions.delete(key);
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
    this.admittedCompletions.clear();
  }
}
