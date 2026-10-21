import { clearSessionBgState } from "../bg-notifications.js";
import { clearSyncWatchAbort } from "../sync-watch-abort.js";
import { clearSessionPermissions } from "../tools/permissions.js";

/**
 * A session can start another execution after one completes, so its state is
 * released only when the host removes the session.
 */
export function releaseSessionState(sessionID: string): void {
  clearSyncWatchAbort(sessionID);
  clearSessionPermissions(sessionID);
  clearSessionBgState(sessionID);
}
