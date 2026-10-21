//! Outside-path approval for consumers that have no plugin permission loop.

use super::*;
use crate::access_policy::AskRoute;
use std::sync::{atomic::AtomicU64, LazyLock, Mutex};

struct Pending {
    writer: WriterSender,
    answer: oneshot::Sender<bool>,
}

// Bash permission requests allocate correlations upward from 1. Start outside
// that range, and use Pending.writer to reject replies from another connection.
static NEXT: AtomicU64 = AtomicU64::new(1 << 63);
static PENDING: LazyLock<Mutex<HashMap<ReverseCorrKey, Pending>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[cfg(test)]
static BEFORE_RERUN: Mutex<Option<Box<dyn FnOnce() + Send>>> = Mutex::new(None);

#[cfg(test)]
pub(super) fn before_rerun_for_test(hook: Option<Box<dyn FnOnce() + Send>>) {
    *BEFORE_RERUN.lock().unwrap() = hook;
}

struct Registration(ReverseCorrKey);
impl Drop for Registration {
    fn drop(&mut self) {
        PENDING
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.0);
    }
}

pub(super) fn reply(writer: &WriterSender, frame: &Frame) -> bool {
    let key = ReverseCorrKey {
        route: route_key(frame.header.channel, frame.header.epoch),
        corr: frame.header.corr,
    };
    let mut pending = PENDING.lock().unwrap_or_else(|e| e.into_inner());
    if !pending
        .get(&key)
        .is_some_and(|p| p.writer.same_channel(writer))
    {
        return false;
    }
    let pending = pending.remove(&key).unwrap();
    let _ = pending.answer.send(
        frame.header.ty == FrameType::Response && bash_elicitation_reply_is_allow(&frame.body),
    );
    true
}

fn stopped(call: &consent::HeldCall, route_hold: &consent::RouteHold) -> bool {
    route_hold.closed()
        || call.connection_cancel.is_cancelled()
        || call.drain.is_active()
        || !active_tool_call_is_registered(&call.active, call.route, call.corr)
        || call
            .identity
            .run_scope
            .get()
            .is_some_and(|scope| scope.revoked())
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn run_with_elicitation<F, Fut>(
    call: consent::HeldCall,
    first: (Response, String),
    mut rerun: F,
    writer: &WriterSender,
    metrics: &DispatchPathMetrics,
    ver: u8,
    flags: Flags,
) -> Option<(Response, Option<String>)>
where
    F: FnMut(AskRoute) -> Fut,
    Fut: std::future::Future<Output = (Response, String)>,
{
    let route_hold = consent::RouteHold::new(call.route);
    let deadline = Instant::now() + bash_elicitation_timeout();
    let mut result = first;
    let mut once = Vec::new();
    while let Some(grant) = consent::requested_ask(&result.0) {
        if stopped(&call, &route_hold) {
            return None;
        }
        // Only the authenticated consumer's elicitation capability enables
        // reverse permission requests; tool arguments cannot opt into them.
        if !matches!(
            access_grants::ask_route(&call.identity),
            AskRoute::Elicitation { .. }
        ) {
            break;
        }
        let key = ReverseCorrKey {
            route: call.route,
            corr: NEXT.fetch_add(1, Ordering::Relaxed),
        };
        let (answer, mut received) = oneshot::channel();
        PENDING.lock().unwrap_or_else(|e| e.into_inner()).insert(
            key,
            Pending {
                writer: writer.clone(),
                answer,
            },
        );
        let _registration = Registration(key);
        let mut body = bash_elicitation_request_body(&call.tool, &[]);
        body["params"]["message"] = json!(format!(
            "Allow {} to {:?} {} once?",
            call.tool,
            grant.operation,
            grant.path.display()
        ));
        body["params"]["requestedSchema"]["properties"]["decision"]["description"] =
            json!("Allow this outside-path access once, or deny it.");
        body["params"]["_meta"]["aft"] = json!({"tool": call.tool, "asks": [{"kind":"external_directory", "path":grant.path, "operation":grant.operation}]});
        let frame = Frame::build_with_version(
            ver,
            FrameType::Request,
            flags,
            call.route.channel,
            call.route.epoch,
            key.corr,
            serde_json::to_vec(&body).unwrap(),
        )
        .ok()?;
        if send_reliable_writer_frame(writer, metrics, frame, "outside-path elicitation")
            .await
            .is_err()
        {
            break;
        }
        let allowed = loop {
            tokio::select! {
                answer = &mut received => break answer.unwrap_or(false),
                _ = tokio::time::sleep_until(deadline.into()) => break false,
                _ = tokio::time::sleep(Duration::from_millis(25)) => {
                    if stopped(&call, &route_hold) {
                        return None;
                    }
                }
            }
        };
        if !allowed {
            break;
        }
        #[cfg(test)]
        {
            let hook = BEFORE_RERUN.lock().unwrap().take();
            if let Some(hook) = hook {
                hook();
            }
        }
        // An allow and a lifecycle change may become ready in the same select.
        // Do not rely on the polling arm winning before admitting protected work.
        if stopped(&call, &route_hold) {
            return None;
        }
        once.push(grant);
        result = rerun(AskRoute::Elicitation { once: once.clone() }).await;
    }
    if consent::requested_ask(&result.0).is_some() {
        let response = Response::error(
            &call.request_id,
            "path_outside_root",
            "outside-path access was not approved",
        );
        Some((response, None))
    } else {
        Some((result.0, Some(result.1)))
    }
}
