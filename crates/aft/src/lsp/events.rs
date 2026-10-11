use super::client::LspEvent;
use crossbeam_channel::Sender;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ServerInstanceId(u64);

impl ServerInstanceId {
    pub(crate) fn next() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self(NEXT.fetch_add(1, Ordering::Relaxed))
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct RustCheckBegin {
    pub(crate) ordinal: u64,
    pub(crate) began: SystemTime,
}

/// Receipt metadata stays inside the manager's channel. Public event payloads
/// keep their existing shape, while initialization and replay retain ownership.
#[derive(Debug, Clone)]
pub(crate) struct ReceivedLspEvent {
    pub(crate) event: LspEvent,
    pub(crate) instance: Option<ServerInstanceId>,
    pub(crate) rust_check_begin: Option<RustCheckBegin>,
}

pub(crate) enum LspEventSender {
    Standalone(Sender<LspEvent>),
    Managed {
        sender: Sender<ReceivedLspEvent>,
        instance: ServerInstanceId,
    },
}

impl LspEventSender {
    pub(crate) fn send(&self, event: LspEvent) -> Result<(), ()> {
        self.send_with_begin(event, None)
    }

    pub(crate) fn send_with_begin(
        &self,
        event: LspEvent,
        rust_check_begin: Option<RustCheckBegin>,
    ) -> Result<(), ()> {
        match self {
            Self::Standalone(sender) => sender.send(event).map_err(|_| ()),
            Self::Managed { sender, instance } => sender
                .send(ReceivedLspEvent {
                    event,
                    instance: Some(*instance),
                    rust_check_begin,
                })
                .map_err(|_| ()),
        }
    }
}
