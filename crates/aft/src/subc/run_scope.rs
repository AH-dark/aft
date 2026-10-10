//! Lifetime controls for long work on daemon-stamped agent run routes.

use super::*;

#[derive(Clone, Copy, Debug)]
pub(super) struct RunBudget {
    deadline: Option<Instant>,
}

impl RunBudget {
    #[cfg(test)]
    fn from_record(record: &subc_protocol::scope::ScopeRecord) -> Self {
        Self::from_expires_at_ms(record.expires_at_ms)
    }

    fn from_expires_at_ms(expires_at_ms: Option<u64>) -> Self {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        Self {
            deadline: expires_at_ms
                .map(|at| Instant::now() + Duration::from_millis(at.saturating_sub(now_ms))),
        }
    }

    pub(super) fn constrain_bash(
        &self,
        args: &mut serde_json::Map<String, Value>,
    ) -> Result<(), ErrorBody> {
        if args.get("background").and_then(Value::as_bool) == Some(true)
            || args.get("pty").and_then(Value::as_bool) == Some(true)
        {
            return Err(ErrorBody::new(
                "run_background_refused",
                "codemode runs can't background; the run's time budget bounds the command (PTY also requires background work)",
            ));
        }
        if let Some(deadline) = self.deadline {
            let remaining = deadline
                .saturating_duration_since(Instant::now())
                .as_millis() as u64;
            if remaining == 0 {
                return Err(ErrorBody::new(
                    "run_deadline_expired",
                    "the codemode run's time budget has expired",
                ));
            }
            let normal = args
                .get("timeout")
                .and_then(Value::as_u64)
                .unwrap_or(crate::bash_background::registry::DEFAULT_BG_TIMEOUT.as_millis() as u64);
            args.insert("timeout".into(), json!(normal.min(remaining)));
        }
        args.insert("block_to_completion".into(), json!(true));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn budget(ms: Option<u64>) -> RunBudget {
        let record = serde_json::from_value(json!({
            "ref": "run-scope", "scope_epoch": 1, "kind": "worker",
            "expires_at_ms": ms.map(|ms| {
                std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
                    .unwrap().as_millis() as u64 + ms
            })
        }))
        .unwrap();
        RunBudget::from_record(&record)
    }

    #[test]
    fn run_background_is_refused_by_name() {
        for args in [json!({"background": true}), json!({"pty": true})] {
            let mut args = args.as_object().unwrap().clone();
            let error = budget(Some(5000)).constrain_bash(&mut args).unwrap_err();
            assert_eq!(error.code, "run_background_refused");
            assert!(error.message.contains("codemode runs can't background"));
        }
    }

    #[test]
    fn run_timeout_is_clamped_to_remaining_time() {
        let mut args = json!({"timeout": 5000}).as_object().unwrap().clone();
        budget(Some(1000)).constrain_bash(&mut args).unwrap();
        let timeout = args["timeout"].as_u64().unwrap();
        assert!(timeout > 0 && timeout <= 1000, "{timeout}");
        assert_eq!(args["block_to_completion"], true);
        let mut shorter = json!({"timeout": 1}).as_object().unwrap().clone();
        budget(Some(1000)).constrain_bash(&mut shorter).unwrap();
        assert_eq!(shorter["timeout"], 1);
    }

    #[test]
    fn run_missing_deadline_falls_back_only_for_timeout() {
        let mut args = json!({"command": "sleep 1"}).as_object().unwrap().clone();
        budget(None).constrain_bash(&mut args).unwrap();
        assert!(args.get("timeout").is_none());
        assert_eq!(args["block_to_completion"], true);
        let mut bg = json!({"background": true}).as_object().unwrap().clone();
        assert_eq!(
            budget(None).constrain_bash(&mut bg).unwrap_err().code,
            "run_background_refused"
        );
    }
}

/// The daemon connection is shared with readiness checks. Run requests use
/// separate correlation IDs so a run reply cannot be mistaken for a readiness reply.
struct ScopeControl {
    writer: WriterSender,
    metrics: Arc<DispatchPathMetrics>,
    pending: readiness::PendingControlReplies,
    ver: u8,
    next_corr: AtomicU64,
    slots: tokio::sync::Semaphore,
}

impl ScopeControl {
    async fn describe(
        &self,
        stamp: &subc_protocol::scope::ScopeStamp,
    ) -> Result<subc_protocol::session::ModuleControlResponseToModule, ErrorBody> {
        let request = async {
            let _slot = self
                .slots
                .acquire()
                .await
                .map_err(|_| ErrorBody::new("run_scope_unavailable", "scope worker stopped"))?;
            let corr = self.next_corr.fetch_add(1, Ordering::Relaxed);
            let rx = self.pending.register(corr);
            struct PendingGuard<'a>(&'a readiness::PendingControlReplies, u64);
            impl Drop for PendingGuard<'_> {
                fn drop(&mut self) {
                    self.0.remove(self.1);
                }
            }
            let _guard = PendingGuard(&self.pending, corr);
            let body = subc_protocol::session::ModuleControlRequestFromModule::ScopeDescribe {
                owner: stamp.owner.clone(),
                scope_ref: stamp.scope_ref.clone(),
            };
            let frame = Frame::build_with_version(
                self.ver,
                FrameType::Request,
                control_flags(),
                0,
                0,
                corr,
                serde_json::to_vec(&body)
                    .map_err(|e| ErrorBody::new("run_scope_unavailable", e.to_string()))?,
            )
            .map_err(|e| ErrorBody::new("run_scope_unavailable", e.to_string()))?;
            send_reliable_writer_frame(&self.writer, &self.metrics, frame, "scope.describe")
                .await
                .map_err(|e| ErrorBody::new("run_scope_unavailable", e.to_string()))?;
            let reply = rx.await.map_err(|_| {
                ErrorBody::new("run_scope_unavailable", "scope.describe connection closed")
            })?;
            if reply.header.ty == FrameType::Error {
                return Err(ErrorBody::new(
                    "run_scope_unavailable",
                    "scope.describe was refused by the daemon",
                ));
            }
            serde_json::from_slice(&reply.body)
                .map_err(|e| ErrorBody::new("run_scope_unavailable", e.to_string()))
        };
        tokio::time::timeout(Duration::from_secs(2), request)
            .await
            .map_err(|_| ErrorBody::new("run_scope_unavailable", "scope.describe timed out"))?
    }
}

pub(super) struct RunScope {
    stamp: subc_protocol::scope::ScopeStamp,
    control: Arc<ScopeControl>,
    describe_lock: tokio::sync::Mutex<()>,
    budget: StdMutex<Option<RunBudget>>,
    tasks: StdMutex<Vec<(crate::bash_background::BgTaskRegistry, String, String)>>,
    revoked: AtomicBool,
    closing: AtomicBool,
}

impl RunScope {
    pub(super) fn cached_budget(&self) -> Result<RunBudget, ErrorBody> {
        if self.revoked.load(Ordering::Acquire) {
            return Err(ErrorBody::new(
                "run_scope_revoked",
                "the codemode run's scope has ended",
            ));
        }
        self.budget
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .ok_or_else(|| {
                ErrorBody::new(
                    "run_scope_unavailable",
                    "run scope record has not been read",
                )
            })
    }

    async fn read_budget(&self) -> Result<RunBudget, ErrorBody> {
        tokio::time::timeout(Duration::from_secs(3), self.read_budget_serial())
            .await
            .map_err(|_| {
                ErrorBody::new(
                    "run_scope_unavailable",
                    "run scope read timed out waiting for describe",
                )
            })?
    }

    async fn read_budget_serial(&self) -> Result<RunBudget, ErrorBody> {
        let _describe = self.describe_lock.lock().await;
        if self
            .budget
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some()
        {
            return self.cached_budget();
        }
        let reply = self.control.describe(&self.stamp).await?;
        use subc_protocol::{scope::ScopeStatus, session::ModuleControlResponseToModule};
        let ModuleControlResponseToModule::ScopeDescribe {
            status: ScopeStatus::Live,
            scope_epoch: Some(epoch),
            scope: Some(stamp),
            ..
        } = &reply
        else {
            return Err(ErrorBody::new(
                "run_scope_unavailable",
                "scope.describe did not confirm a live run scope",
            ));
        };
        if *epoch != self.stamp.scope_epoch
            || stamp.scope_epoch != *epoch
            || stamp.owner != self.stamp.owner
            || stamp.scope_ref != self.stamp.scope_ref
        {
            return Err(ErrorBody::new(
                "run_scope_unavailable",
                "scope.describe did not match the route's run scope",
            ));
        }
        let expires_at_ms = deadline_from_describe(stamp);
        let budget = RunBudget::from_expires_at_ms(expires_at_ms);
        *self
            .budget
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(budget);
        self.cached_budget()
    }

    pub(super) fn route_closed(self: &Arc<Self>) {
        if self.revoked.load(Ordering::Acquire) || self.closing.swap(true, Ordering::AcqRel) {
            return;
        }
        let scope = Arc::clone(self);
        tokio::spawn(async move {
            // A reconnect can close a route without ending its scope. Only
            // scope.describe confirming an end, a different live epoch, or a
            // synced owner's missing scope permits terminating the run's work.
            for attempt in 0..3 {
                let reply = tokio::time::timeout(Duration::from_secs(3), async {
                    let _describe = scope.describe_lock.lock().await;
                    scope.control.describe(&scope.stamp).await
                })
                .await
                .unwrap_or_else(|_| {
                    Err(ErrorBody::new(
                        "run_scope_unavailable",
                        "scope close confirmation timed out",
                    ))
                });
                match reply {
                    Ok(reply) if scope.ended(&reply) => {
                        scope.revoked.store(true, Ordering::Release);
                        let scope_for_kill = Arc::clone(&scope);
                        let _ = tokio::task::spawn_blocking(move || {
                            let tasks = scope_for_kill
                                .tasks
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .clone();
                            for (registry, task, session) in tasks {
                                if let Err(error) =
                                    registry.kill_for_scope_revocation(&task, &session)
                                {
                                    log::warn!(
                                        "run scope process-group kill failed task={task}: {error}"
                                    );
                                }
                            }
                        })
                        .await;
                        break;
                    }
                    Ok(_) => {}
                    Err(error) => {
                        log::warn!("run scope close confirmation held: {}", error.message)
                    }
                }
                if attempt != 2 {
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
            }
            scope.closing.store(false, Ordering::Release);
        });
    }

    fn ended(&self, reply: &subc_protocol::session::ModuleControlResponseToModule) -> bool {
        use subc_protocol::{scope::ScopeStatus, session::ModuleControlResponseToModule};
        match reply {
            ModuleControlResponseToModule::ScopeDescribe {
                status,
                scope_epoch,
                owner_synced,
                ..
            } => match status {
                ScopeStatus::Ended => true,
                ScopeStatus::Live => {
                    scope_epoch.is_some_and(|epoch| epoch != self.stamp.scope_epoch)
                }
                ScopeStatus::NotLive => *owner_synced,
            },
            _ => false,
        }
    }

    pub(super) fn track(
        &self,
        registry: &crate::bash_background::BgTaskRegistry,
        task: &str,
        session: &str,
    ) {
        self.tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((registry.clone(), task.into(), session.into()));
        // Registration can race the close worker's snapshot. A task registered
        // after revocation must be killed too, not missed by that snapshot.
        if self.revoked.load(Ordering::Acquire) {
            let _ = registry.kill_for_scope_revocation(task, session);
        }
    }
}

pub(super) struct RunScopes {
    control: Arc<ScopeControl>,
    scopes: StdMutex<HashMap<(String, String, u64), Arc<RunScope>>>,
}

impl RunScopes {
    pub(super) fn new(
        writer: WriterSender,
        metrics: Arc<DispatchPathMetrics>,
        pending: readiness::PendingControlReplies,
        ver: u8,
    ) -> Self {
        Self {
            control: Arc::new(ScopeControl {
                writer,
                metrics,
                pending,
                ver,
                next_corr: AtomicU64::new(1 << 62),
                slots: tokio::sync::Semaphore::new(8),
            }),
            scopes: StdMutex::new(HashMap::new()),
        }
    }

    pub(super) fn bind(&self, identity: &RouteIdentity) -> Option<Arc<RunScope>> {
        let stamp = identity
            .scope
            .as_ref()
            .filter(|scope| scope.attributes.run_id.is_some())?;
        Some(Arc::clone(identity.run_scope.get_or_init(|| {
            let key = (
                serde_json::to_string(&stamp.owner).unwrap(),
                stamp.scope_ref.clone(),
                stamp.scope_epoch,
            );
            Arc::clone(
                self.scopes
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .entry(key)
                    .or_insert_with(|| {
                        Arc::new(RunScope {
                            stamp: stamp.clone(),
                            control: Arc::clone(&self.control),
                            describe_lock: tokio::sync::Mutex::new(()),
                            budget: StdMutex::new(None),
                            tasks: StdMutex::new(Vec::new()),
                            revoked: AtomicBool::new(false),
                            closing: AtomicBool::new(false),
                        })
                    }),
            )
        })))
    }
}

// subc-protocol 0.30 describes a ScopeStamp, which does not expose the
// ScopeRecord's expires_at_ms. Keep the wire limitation in one place: once
// the protocol carries it, read that field here. Do not infer a deadline
// from route lifetime or treat a failed describe as a missing deadline.
fn deadline_from_describe(_stamp: &subc_protocol::scope::ScopeStamp) -> Option<u64> {
    None
}

impl RunScopes {
    /// Defer only long operations. Admission and permissions still use the
    /// agent's existing catalog and policy; runs add lifetime controls only.
    pub(super) fn defer(
        &self,
        decoded: DecodedFrame,
        identity: &RouteIdentity,
        ready: &mpsc::UnboundedSender<DecodedFrame>,
    ) -> Result<(), DecodedFrame> {
        let Some(scope) = self.bind(identity) else {
            return Err(decoded);
        };
        let body = serde_json::from_slice::<Value>(&decoded.frame.body).unwrap_or(Value::Null);
        let name = body.get("name").and_then(Value::as_str).unwrap_or("");
        if !matches!(
            name,
            "bash" | "powershell" | "bash_watch" | "inspect" | "delete"
        ) && !crate::commands::lsp_navigation::is_lsp_navigation_command(name)
        {
            return Err(decoded);
        }
        let raw_args = body
            .get("arguments")
            .or_else(|| body.get("params"))
            .unwrap_or(&Value::Null);
        let args = raw_args.get("params").unwrap_or(raw_args);
        let background_error = if matches!(name, "bash" | "powershell" | "bash_watch") {
            let mut args = args.as_object().cloned().unwrap_or_default();
            RunBudget { deadline: None }.constrain_bash(&mut args).err()
        } else {
            None
        };
        if background_error.is_none() && scope.cached_budget().is_ok() {
            return Err(decoded);
        }
        let control = Arc::clone(&self.control);
        let ready = ready.clone();
        tokio::spawn(async move {
            let result = match background_error {
                Some(error) => Err(error),
                None => scope.read_budget().await,
            };
            match result {
                Ok(_) => {
                    let _ = ready.send(decoded);
                }
                Err(error) => {
                    let frame = &decoded.frame;
                    if let Ok(refusal) = build_error_frame(
                        frame.header.ver,
                        frame.header.channel,
                        frame.header.epoch,
                        frame.header.corr,
                        frame.header.flags,
                        &error.code,
                        &error.message,
                    ) {
                        let _ = send_reliable_writer_frame(
                            &control.writer,
                            &control.metrics,
                            refusal,
                            "run scope refusal",
                        )
                        .await;
                    }
                }
            }
        });
        Ok(())
    }
}

impl fmt::Debug for RunScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RunScope")
            .field("stamp", &self.stamp)
            .finish_non_exhaustive()
    }
}

impl RunBudget {
    pub(super) fn guard_pending(&self, pending: PendingResponse) -> PendingResponse {
        let Some(deadline) = self.deadline else {
            return pending;
        };
        let mut pending = pending.with_poll_interval(PENDING_POLL_INTERVAL);
        let mut poll = pending.poll;
        let cancellation = pending.cancellation.clone();
        let id = pending.request_id.clone();
        pending.poll = Box::new(move |ctx| {
            if Instant::now() >= deadline {
                if let Some(cancel) = &cancellation {
                    cancel.request_cancel();
                }
                Some(Response::error(
                    &id,
                    "run_deadline_expired",
                    "the codemode run's time budget has expired",
                ))
            } else {
                poll(ctx)
            }
        });
        pending
    }
}

#[cfg(test)]
impl RunScope {
    pub(super) fn for_test(deadline: Option<Instant>) -> Arc<Self> {
        let (writer, _) = mpsc::channel(8);
        let scopes = RunScopes::new(
            writer,
            Arc::new(DispatchPathMetrics::new()),
            readiness::PendingControlReplies::default(),
            PROTOCOL_VERSION,
        );
        Arc::new(Self {
            stamp: serde_json::from_value(json!({
                "owner":{"kind":"reserved", "module_id":"prefrontal-core"},
                "ref":"run", "scope_epoch":1, "kind":"worker",
                "attributes":{"agent_id":"agent", "run_id":"run"}, "owner_authorized":true
            }))
            .unwrap(),
            control: scopes.control,
            describe_lock: tokio::sync::Mutex::new(()),
            budget: StdMutex::new(Some(RunBudget { deadline })),
            tasks: StdMutex::new(Vec::new()),
            revoked: AtomicBool::new(false),
            closing: AtomicBool::new(false),
        })
    }
}

#[cfg(test)]
mod lifetime_tests {
    use super::*;

    #[test]
    fn close_outcomes_are_definitive_only() {
        use subc_protocol::{scope::ScopeStatus, session::ModuleControlResponseToModule};
        let scope = RunScope::for_test(None);
        for (status, epoch, synced, expected) in [
            (ScopeStatus::Live, Some(1), true, false),
            (ScopeStatus::Live, Some(2), true, true),
            (ScopeStatus::Live, None, true, false),
            (ScopeStatus::Ended, Some(1), true, true),
            (ScopeStatus::NotLive, None, true, true),
            (ScopeStatus::NotLive, None, false, false),
        ] {
            let reply = ModuleControlResponseToModule::ScopeDescribe {
                status,
                scope_epoch: epoch,
                daemon_incarnation: "daemon".into(),
                owner_synced: synced,
                owner_configured: true,
                scope: None,
            };
            assert_eq!(
                scope.ended(&reply),
                expected,
                "{status:?}, {epoch:?}, synced={synced}"
            );
        }
        assert!(!scope.ended(&ModuleControlResponseToModule::CatalogUpdate {}));
    }

    #[test]
    fn run_budget_cancels_pending_long_work() {
        let cancel = JobCancellation::new();
        let pending = PendingResponse::polling(
            "run-inspect".into(),
            "run".into(),
            "inspect".into(),
            Box::new(|_| None),
        )
        .with_cancellation(cancel.clone());
        let mut pending = RunBudget {
            deadline: Some(Instant::now()),
        }
        .guard_pending(pending);
        let response = (pending.poll)(&test_support::test_ctx())
            .expect("expired run must resolve pending work");
        assert!(!response.success);
        assert_eq!(response.data["code"], "run_deadline_expired");
        assert!(cancel.cancel_already_requested());
    }
}
