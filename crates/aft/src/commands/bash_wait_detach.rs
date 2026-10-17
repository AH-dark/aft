use serde_json::json;

use crate::context::AppContext;
use crate::protocol::{RawRequest, Response};

pub fn handle(req: &RawRequest, ctx: &AppContext) -> Response {
    let registry = ctx.bash_background();
    let detached = if req.params.get("clear_pending").and_then(|v| v.as_bool()) == Some(true) {
        registry.clear_pending_wait_detach(req.session());
        false
    } else if req.params.get("pending").and_then(|v| v.as_bool()) == Some(true) {
        registry.request_wait_mode_detach(req.session())
    } else {
        registry.signal_wait_mode_detach(req.session())
    };
    // The signal is fire-and-forget from the plugin's user-message hook, so a
    // silent session mismatch is invisible without this trace: log which
    // session the signal resolved against and whether a wait was active.
    crate::slog_info!(
        "bash_wait_detach: session={} detached={} active_wait_sessions={}",
        req.session(),
        detached,
        ctx.bash_background().active_wait_session_count()
    );
    Response::success(&req.id, json!({ "detached": detached }))
}
