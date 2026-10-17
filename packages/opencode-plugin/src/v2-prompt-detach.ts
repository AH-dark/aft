/**
 * OpenCode 2 has no `chat.message` hook. The equivalent is the session
 * `prompt` hook, which the host runs for every prompt before admitting it
 * (a steer into a busy session included) and whose edited `prompt` it keeps.
 * This module wires that hook to the same decision the OpenCode 1 entry makes
 * in `chat.message`: a new message detaches a waiting `bash({wait:true})` and
 * aborts a sync `bash_watch`, and the operator's `&detach` token is stripped
 * from the text before the model sees it.
 */
import { detachStripEdits } from "@cortexkit/aft-bridge";
import { Effect } from "effect";

import {
  clearPendingBashWaitDetach,
  interruptBashWaitsForChatMessage,
} from "./bash-wait-detach.js";
import type { AftConfig } from "./config.js";
import { warn } from "./logger.js";
import { isAftOriginatedPrompt } from "./wakes/session-delivery.js";

/** An offset range into `prompt.text`; `end` is exclusive. */
interface PromptMention {
  start: number;
  end: number;
  text: string;
}

interface MentionCarrier {
  mention?: PromptMention;
}

/** The fields of OpenCode 2's `SessionHooks["prompt"]` event this hook reads or edits. */
export interface V2PromptHookEvent {
  readonly sessionID: string;
  prompt: {
    text: string;
    files?: MentionCarrier[];
    agents?: MentionCarrier[];
    skills?: MentionCarrier[];
  };
  metadata?: Record<string, unknown>;
}

type ChatMessagePool = Parameters<typeof interruptBashWaitsForChatMessage>[0];

export interface V2PromptDetachRuntime {
  /** The Location's bridge pool, where a session's waits live. */
  readonly pool: ChatMessagePool;
  /**
   * The root this Location's tools run on (the Location's own directory); its
   * bridge is asked to detach before any other.
   */
  readonly projectRoot: string;
  /**
   * Read on every prompt so a live config reload of
   * `bash.detach_on_user_message` applies to the next message.
   */
  getConfig(): AftConfig;
  registerSession?: (sessionID: string) => void;
}

/** A text edit in one strip stage: `[start, end)` became `length` characters. */
interface TextEdit {
  start: number;
  end: number;
  length: number;
}

/** Move an offset through edits (sorted, non-overlapping) made to its text. */
function mapOffset(offset: number, edits: readonly TextEdit[]): number {
  let shift = 0;
  for (const edit of edits) {
    if (offset <= edit.start) break;
    if (offset >= edit.end) {
      shift += edit.length - (edit.end - edit.start);
      continue;
    }
    // Inside an edited span: land on the matching spot of its replacement.
    return edit.start + shift + Math.min(offset - edit.start, edit.length);
  }
  return offset + shift;
}

/** Move mention positions through the same localized edits used to strip the prompt. */
function offsetMapper(original: string, finalText: string): (offset: number) => number {
  const edits = detachStripEdits(original);
  return (offset) => Math.min(mapOffset(offset, edits), finalText.length);
}

function mentionCarriers(prompt: V2PromptHookEvent["prompt"]): MentionCarrier[] {
  return [...(prompt.files ?? []), ...(prompt.agents ?? []), ...(prompt.skills ?? [])].filter(
    (item): item is MentionCarrier & { mention: PromptMention } =>
      typeof item?.mention?.start === "number" && typeof item.mention.end === "number",
  );
}

/**
 * Run the shared chat-message decision on one OpenCode 2 prompt and write the
 * stripped text (and moved mention offsets) back onto it. Returns whether the
 * session's waits were interrupted.
 *
 * A prompt AFT admitted itself (a completion wake) is passed to the shared
 * decision as a synthetic part, exactly as OpenCode 1 marks such parts: it
 * interrupts a wait whenever `bash.detach_on_user_message` is on, but its text
 * is never read for `&detach` and never edited.
 *
 * All edits are computed before any is applied, so a throw leaves the prompt
 * exactly as it arrived.
 */
export function interruptBashWaitsForV2Prompt(
  runtime: V2PromptDetachRuntime,
  event: V2PromptHookEvent,
): boolean {
  const prompt = event.prompt;
  const original = prompt.text;
  const synthetic = isAftOriginatedPrompt(event.metadata);
  const output = { parts: [{ type: "text", text: original, synthetic }] };
  const interrupted = interruptBashWaitsForChatMessage(
    runtime.pool,
    runtime.getConfig(),
    runtime.projectRoot,
    event.sessionID,
    output,
  );
  const finalText = output.parts[0]?.text ?? original;
  if (synthetic || finalText === original) return interrupted;

  const map = offsetMapper(original, finalText);
  const moved = mentionCarriers(prompt).map((item) => {
    const mention = item.mention as PromptMention;
    return { item, mention: { ...mention, start: map(mention.start), end: map(mention.end) } };
  });
  prompt.text = finalText;
  for (const { item, mention } of moved) item.mention = mention;
  return interrupted;
}

function failureReason(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

/**
 * The callback registered as OpenCode 2's session `prompt` hook. The host
 * waits for it before admitting the message and a failure would reject the
 * message, so it never fails and never waits: errors are caught, logged once
 * per distinct reason, and the prompt passes through unchanged; the bridge
 * detach request it may start is not awaited.
 */
export function createV2PromptDetachHook(
  runtime: V2PromptDetachRuntime,
): (event: V2PromptHookEvent) => Effect.Effect<void> {
  const loggedReasons = new Set<string>();
  return (event) =>
    Effect.sync(() => {
      try {
        runtime.registerSession?.(event.sessionID);
        interruptBashWaitsForV2Prompt(runtime, event);
      } catch (error) {
        const reason = failureReason(error);
        if (loggedReasons.has(reason)) return;
        loggedReasons.add(reason);
        warn(
          `[bash_wait_detach] prompt hook failed for session ${String(event?.sessionID)}; the prompt passes through unchanged: ${reason}`,
        );
      }
    });
}

type SessionHookRegistrar = (
  name: "prompt",
  callback: (event: V2PromptHookEvent) => Effect.Effect<void>,
) => Effect.Effect<unknown, never, unknown>;

/** OpenCode 2 prepares model-step context after admitting queued prompts, so old requests can be cleared. */
export function createV2DetachDeliveryHook(
  runtime: V2PromptDetachRuntime,
): (event: { sessionID: string }) => Effect.Effect<void> {
  return (event) => Effect.promise(() => clearPendingBashWaitDetach(runtime.pool, event.sessionID));
}

/**
 * Register the prompt hook on an OpenCode 2 host context. The registration is
 * scoped by the host, so it ends with the Location. A host context without
 * `session.hook` gets no hook, which leaves waits un-detachable by messages
 * but otherwise working.
 */
export function registerV2PromptDetachHook(
  context: unknown,
  runtime: V2PromptDetachRuntime,
): Effect.Effect<boolean, never, unknown> {
  const session = (context as { session?: { hook?: unknown } } | null)?.session;
  const hook = session?.hook;
  if (typeof hook !== "function") {
    warn(
      "[bash_wait_detach] host has no session prompt hook; a new message cannot detach a waiting bash here",
    );
    return Effect.succeed(false);
  }
  return Effect.gen(function* () {
    yield* (hook as SessionHookRegistrar).call(
      session,
      "prompt",
      createV2PromptDetachHook(runtime),
    );
    yield* (
      hook as (
        name: "context",
        callback: (event: { sessionID: string }) => Effect.Effect<void>,
      ) => Effect.Effect<unknown, never, unknown>
    ).call(session, "context", createV2DetachDeliveryHook(runtime));
    return true;
  });
}
