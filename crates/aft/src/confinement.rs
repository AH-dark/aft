//! Request-local root fencing for the synchronous, read-only worker surface.

use crate::{context::AppContext, protocol::Response};
use serde_json::Value;
use std::{cell::RefCell, path::Path};

thread_local! {
    static REQUESTS: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
    #[cfg(test)]
    static ASK_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn note_ask() {
    ASK_CALLS.with(|calls| calls.set(calls.get() + 1));
}

#[cfg(all(test, unix))]
pub(crate) fn ask_calls() -> usize {
    ASK_CALLS.with(|calls| calls.get())
}

pub(crate) fn active() -> bool {
    REQUESTS.with(|requests| !requests.borrow().is_empty())
}

pub(crate) fn for_request(id: &str) -> bool {
    REQUESTS.with(|requests| requests.borrow().iter().any(|request| request == id))
}

pub(crate) fn capture() -> Vec<String> {
    REQUESTS.with(|requests| requests.borrow().clone())
}

pub(crate) fn inherit<R>(requests: Vec<String>, run: impl FnOnce() -> R) -> R {
    struct Restore(Vec<String>);
    impl Drop for Restore {
        fn drop(&mut self) {
            REQUESTS.with(|requests| *requests.borrow_mut() = std::mem::take(&mut self.0));
        }
    }
    let _restore = Restore(REQUESTS.with(|current| current.replace(requests)));
    run()
}

pub(crate) fn refusal(id: &str, input: &Path) -> Response {
    Response::error(
        id,
        "path_outside_confined_root",
        format!(
            "path '{}' is not available in this confined root",
            input.display()
        ),
    )
}

pub(crate) fn with_root<R>(ctx: &AppContext, id: &str, run: impl FnOnce() -> R) -> R {
    struct Guard;
    impl Drop for Guard {
        fn drop(&mut self) {
            REQUESTS.with(|requests| {
                requests.borrow_mut().pop();
            });
        }
    }
    REQUESTS.with(|requests| requests.borrow_mut().push(id.to_owned()));
    let _guard = Guard;
    ctx.with_force_restrict(id, run)
}

/// Validate original spellings before translation can join them onto a root or
/// reinterpret an absent file as a repository selector or remote resource.
pub(crate) fn preflight(
    ctx: &AppContext,
    id: &str,
    session: &str,
    args: &Value,
) -> Result<(), Response> {
    fn validate(ctx: &AppContext, id: &str, session: &str, value: &Value) -> Result<(), Response> {
        if let Some(input) = value.as_str() {
            if input.contains("://") {
                return Err(refusal(id, Path::new(input)));
            }
            ctx.validate_read_path(id, session, Path::new(input))?;
        } else if let Some(items) = value.as_array() {
            for item in items {
                validate(ctx, id, session, item)?;
            }
        }
        Ok(())
    }
    for key in ["path", "filePath", "target", "directory", "files", "url"] {
        if let Some(value) = args.get(key) {
            validate(ctx, id, session, value)?;
        }
    }
    ctx.validate_read_path(id, session, Path::new("."))?;
    Ok(())
}
