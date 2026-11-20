//! A single Rust worker owns the coordinator. UI polling only consumes events.
use crate::{CommandRequest, PlaybackCoordinator, PlayerEvent};
use player_domain::{ErrorCode, PlayerError};
use std::{
    collections::{BTreeMap, VecDeque},
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender, TrySendError},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

pub const MAX_PENDING_COMMANDS: usize = 64;
const ANALYSIS_POLL: Duration = Duration::from_millis(20);
const HEARTBEAT: Duration = Duration::from_millis(100);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubmitError {
    InvalidRequest,
    Busy,
    Unavailable,
}

#[derive(Default)]
struct Outbox {
    events: VecDeque<PlayerEvent>,
    // A completed result retains its credit until the C caller has copied it.
    requests: BTreeMap<u64, bool>,
    failed: bool,
    last_snapshot: Option<Box<crate::PlayerSnapshot>>,
}
impl Outbox {
    fn publish(&mut self, event: PlayerEvent) {
        match &event {
            PlayerEvent::AudioAnalysis { .. } => self.events.retain(|e| !matches!(e, PlayerEvent::AudioAnalysis {..})),
            PlayerEvent::RemoteState { state } => {
                self.events.retain(|event| !matches!(event, PlayerEvent::RemoteState { state: old } if old.provider == state.provider));
                if state.authentication_required || matches!(state.connection, player_domain::ProviderConnectionState::Disconnected | player_domain::ProviderConnectionState::Connecting) {
                    self.events.retain(|event| !matches!(event, PlayerEvent::RemoteOutputCommand { provider, .. } if provider == &state.provider));
                }
            },
            PlayerEvent::RemoteEdited { .. } => self.events.retain(|event| !matches!(event, PlayerEvent::RemoteEdited { .. })),
            PlayerEvent::RemoteOutputCommand { provider, .. } => self.events.retain(|event| !matches!(event, PlayerEvent::RemoteOutputCommand { provider: old, .. } if old == provider)),
            PlayerEvent::RemotePage { page } => {
                self.events.retain(|event| !matches!(event, PlayerEvent::RemotePage { page: old }
                    if old.provider == page.provider && old.detail == page.detail && (!page.detail || old.query_id == page.query_id)));
                if page.detail && self.events.iter().filter(|event| matches!(event, PlayerEvent::RemotePage { page: old } if old.provider == page.provider && old.detail)).count() >= 8
                    && let Some(index) = self.events.iter().position(|event| matches!(event, PlayerEvent::RemotePage { page: old } if old.provider == page.provider && old.detail)) {
                    self.events.remove(index);
                }
            },
            PlayerEvent::LibraryState { .. } => self
                .events
                .retain(|e| !matches!(e, PlayerEvent::LibraryState { .. })),
            PlayerEvent::LibraryPage { .. } => self
                .events
                .retain(|e| !matches!(e, PlayerEvent::LibraryPage { .. })),
            PlayerEvent::MediaUpdated { item } => {
                self.events.retain(
                    |e| !matches!(e, PlayerEvent::MediaUpdated { item: old } if old.id == item.id),
                );
                if self
                    .events
                    .iter()
                    .filter(|e| matches!(e, PlayerEvent::MediaUpdated { .. }))
                    .count()
                    == 64
                    && let Some(index) = self
                        .events
                        .iter()
                        .position(|e| matches!(e, PlayerEvent::MediaUpdated { .. }))
                {
                    self.events.remove(index);
                }
            }
            PlayerEvent::Snapshot { state } => {
                self.last_snapshot = Some(state.clone());
                self.events
                    .retain(|e| !matches!(e, PlayerEvent::Snapshot { .. }));
            }
            PlayerEvent::CommandResult { request_id, .. } => {
                if let Some(completed) = self.requests.get_mut(request_id) {
                    *completed = true;
                }
            }
            PlayerEvent::Error { .. } => {
                if self
                    .events
                    .iter()
                    .filter(|e| matches!(e, PlayerEvent::Error { .. }))
                    .count()
                    == 8
                    && let Some(index) = self
                        .events
                        .iter()
                        .position(|e| matches!(e, PlayerEvent::Error { .. }))
                {
                    self.events.remove(index);
                }
            }
            PlayerEvent::Catalog { .. } => self
                .events
                .retain(|e| !matches!(e, PlayerEvent::Catalog { .. })),
        }
        self.events.push_back(event);
    }
    fn fail(&mut self) {
        self.failed = true;
        self.events
            .retain(|event| !matches!(event, PlayerEvent::RemoteOutputCommand { .. }));
        let error = PlayerError::new(
            ErrorCode::ProviderUnavailable,
            "The player worker stopped unexpectedly",
        );
        let unfinished: Vec<_> = self
            .requests
            .iter()
            .filter_map(|(id, done)| (!done).then_some(*id))
            .collect();
        for request_id in unfinished {
            self.publish(PlayerEvent::CommandResult {
                request_id,
                error: Some(error.clone()),
            });
        }
        self.publish(PlayerEvent::AudioAnalysis {
            media_id: None,
            frame: None,
            modulation: None,
        });
        self.publish(PlayerEvent::Error { error });
        if let Some(mut state) = self.last_snapshot.take() {
            state.state = player_domain::PlaybackState::Error;
            state.playback_session_active = false;
            state.provider_connection = player_domain::ProviderConnectionState::Unavailable;
            state.capabilities = player_domain::Capabilities::default();
            state.revision += 1;
            self.publish(PlayerEvent::Snapshot { state });
        }
    }
}

pub struct PlayerRuntime {
    sender: Option<SyncSender<CommandRequest>>,
    outbox: Arc<Mutex<Outbox>>,
    stopping: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}
impl PlayerRuntime {
    pub fn start(mut core: PlaybackCoordinator) -> std::io::Result<Self> {
        let (sender, receiver) = mpsc::sync_channel(MAX_PENDING_COMMANDS);
        let outbox = Arc::new(Mutex::new(Outbox::default()));
        let stopping = Arc::new(AtomicBool::new(false));
        let worker_outbox = outbox.clone();
        let worker_stopping = stopping.clone();
        core.publish_snapshot();
        let worker = thread::Builder::new()
            .name("auvra-player".into())
            .spawn(move || {
                let outcome = catch_unwind(AssertUnwindSafe(|| {
                    run(&mut core, receiver, &worker_outbox, &worker_stopping)
                }));
                if outcome.is_err() {
                    worker_outbox
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .fail();
                }
                let _ = catch_unwind(AssertUnwindSafe(|| core.shutdown()));
            })?;
        Ok(Self {
            sender: Some(sender),
            outbox,
            stopping,
            worker: Some(worker),
        })
    }
    pub fn submit(&self, request: CommandRequest) -> Result<(), SubmitError> {
        let mut outbox = self.outbox.lock().unwrap_or_else(|e| e.into_inner());
        if outbox.failed {
            return Err(SubmitError::Unavailable);
        }
        if request.request_id == 0 || outbox.requests.contains_key(&request.request_id) {
            return Err(SubmitError::InvalidRequest);
        }
        if outbox.requests.len() == MAX_PENDING_COMMANDS {
            return Err(SubmitError::Busy);
        }
        let id = request.request_id;
        outbox.requests.insert(id, false);
        match self
            .sender
            .as_ref()
            .ok_or(SubmitError::Unavailable)?
            .try_send(request)
        {
            Ok(()) => Ok(()),
            Err(error) => {
                outbox.requests.remove(&id);
                Err(match error {
                    TrySendError::Full(_) => SubmitError::Busy,
                    TrySendError::Disconnected(_) => SubmitError::Unavailable,
                })
            }
        }
    }
    /// Taking an event pins it in the caller. Only acknowledgement releases a result credit.
    pub fn take_event(&self) -> Option<PlayerEvent> {
        self.outbox
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .events
            .pop_front()
    }
    pub fn acknowledge(&self, event: &PlayerEvent) {
        if let PlayerEvent::CommandResult { request_id, .. } = event {
            self.outbox
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .requests
                .remove(request_id);
        }
    }
}
impl Drop for PlayerRuntime {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Release);
        self.sender.take(); // Wake recv_timeout, without waiting for a free command slot.
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
fn pump(core: &mut PlaybackCoordinator, outbox: &Mutex<Outbox>) {
    let mut outbox = outbox.lock().unwrap_or_else(|e| e.into_inner());
    while let Some(event) = core.pop_event() {
        outbox.publish(event);
    }
}
fn run(
    core: &mut PlaybackCoordinator,
    receiver: Receiver<CommandRequest>,
    outbox: &Mutex<Outbox>,
    stopping: &AtomicBool,
) {
    let mut last = Instant::now();
    pump(core, outbox);
    while !stopping.load(Ordering::Acquire) {
        let message = receiver.recv_timeout(ANALYSIS_POLL);
        let now = Instant::now();
        if message.is_ok() || now.duration_since(last) >= HEARTBEAT {
            core.tick(now.duration_since(last));
            last = now;
        } else {
            core.poll_loading_local_output();
        }
        match message {
            Ok(request) => core.send_request(request),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        core.poll_deferred_spatial_play();
        core.poll_analysis();
        pump(core, outbox);
    }
    // Finish admitted commands before the final checkpoint. No new sender survives Drop.
    while let Ok(request) = receiver.try_recv() {
        core.tick(last.elapsed());
        last = Instant::now();
        core.send_request(request);
        pump(core, outbox);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PlayerCommand;
    use player_domain::{PlaybackContext, PlaybackState};
    use provider_mock::{MockProvider, demo_tracks};

    #[test]
    fn catalog_coalescing_preserves_eight_independent_detail_results() {
        let mut outbox = Outbox::default();
        let page = |query_id, detail| PlayerEvent::RemotePage {
            page: player_domain::RemotePage {
                position_edits: false,
                related_collections: vec![],
                detail,
                item_positions: vec![],
                next_cursor: None,
                provider: "spotify".into(),
                query_id,
                offset: 0,
                total: 0,
                items: vec![],
                collections: vec![],
                spoken_progress: vec![],
                recent_plays: vec![],
                issue: None,
            },
        };
        for id in 1..=10 {
            outbox.publish(page(id, true));
            outbox.publish(page(id + 100, false));
        }
        let ids: Vec<_> = outbox
            .events
            .iter()
            .filter_map(|event| {
                if let PlayerEvent::RemotePage { page } = event {
                    Some((page.query_id, page.detail))
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(ids.len(), 9);
        assert_eq!(ids.iter().filter(|(_, detail)| *detail).count(), 8);
        assert!(ids.contains(&(110, false)) && ids.contains(&(10, true)));
        assert!(!ids.contains(&(1, true)) && !ids.contains(&(101, false)));
        outbox.publish(page(10, true));
        assert_eq!(outbox.events.len(), 9);
    }
    fn runtime() -> PlayerRuntime {
        let mut core = PlaybackCoordinator::default();
        core.register(MockProvider::default());
        PlayerRuntime::start(core).unwrap()
    }
    fn request(id: u64, command: PlayerCommand) -> CommandRequest {
        CommandRequest {
            request_id: id,
            command,
        }
    }
    fn wait(runtime: &PlayerRuntime, predicate: impl Fn(&PlayerEvent) -> bool) -> PlayerEvent {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if let Some(event) = runtime.take_event() {
                runtime.acknowledge(&event);
                if predicate(&event) {
                    return event;
                }
            }
            assert!(Instant::now() < deadline, "event timed out");
            thread::sleep(Duration::from_millis(5));
        }
    }
    #[test]
    fn worker_finishes_tracks_without_ui_polling() {
        let runtime = runtime();
        let mut tracks = demo_tracks();
        tracks[0].duration_ms = 100;
        runtime
            .submit(request(
                1,
                PlayerCommand::ReplaceQueue {
                    items: tracks,
                    start: 0,
                    context: PlaybackContext::UserQueue,
                },
            ))
            .unwrap();
        wait(&runtime, |e| {
            matches!(e, PlayerEvent::CommandResult { request_id: 1, .. })
        });
        thread::sleep(Duration::from_millis(350)); // No tick or drain call drives this interval.
        let event = wait(
            &runtime,
            |e| matches!(e,PlayerEvent::Snapshot{state} if state.queue.current.as_ref().is_some_and(|i| i.title=="Soft Focus")),
        );
        if let PlayerEvent::Snapshot { state } = event {
            assert_eq!(state.state, PlaybackState::Playing);
            assert!(state.position_ms >= 100);
        }
    }
    #[test]
    fn receipts_are_ordered_bounded_and_credit_is_released_on_ack() {
        let runtime = runtime();
        for id in 1..=64 {
            runtime
                .submit(request(
                    id,
                    PlayerCommand::SetVolume {
                        volume: id as f32 / 100.0,
                    },
                ))
                .unwrap();
        }
        assert_eq!(
            runtime.submit(request(65, PlayerCommand::Stop)),
            Err(SubmitError::Busy)
        );
        let mut results = vec![];
        for id in 1..=64 {
            let event = wait(&runtime, |e| matches!(e, PlayerEvent::CommandResult { .. }));
            if let PlayerEvent::CommandResult { request_id, error } = event {
                assert!(error.is_none());
                assert_eq!(request_id, id);
                results.push(request_id);
            }
        }
        assert_eq!(results.len(), 64);
        runtime.submit(request(65, PlayerCommand::Stop)).unwrap();
    }
    #[test]
    fn duplicate_and_zero_requests_are_rejected_without_execution() {
        let runtime = runtime();
        assert_eq!(
            runtime.submit(request(0, PlayerCommand::Stop)),
            Err(SubmitError::InvalidRequest)
        );
        runtime.submit(request(1, PlayerCommand::Stop)).unwrap();
        assert_eq!(
            runtime.submit(request(1, PlayerCommand::Stop)),
            Err(SubmitError::InvalidRequest)
        );
        wait(&runtime, |e| {
            matches!(e, PlayerEvent::CommandResult { request_id: 1, .. })
        });
    }
    #[test]
    fn worker_panic_fails_accepted_request_and_rejects_future_work() {
        let mut core = PlaybackCoordinator::default();
        core.register(crate::test_provider::TestProvider {
            panic_on_load: true,
            ..Default::default()
        });
        let runtime = PlayerRuntime::start(core).unwrap();
        runtime
            .submit(request(
                1,
                PlayerCommand::ReplaceQueue {
                    items: demo_tracks(),
                    start: 0,
                    context: PlaybackContext::UserQueue,
                },
            ))
            .unwrap();
        let result = wait(&runtime, |e| {
            matches!(e, PlayerEvent::CommandResult { request_id: 1, .. })
        });
        assert!(matches!(
            result,
            PlayerEvent::CommandResult { error: Some(_), .. }
        ));
        assert_eq!(
            runtime.submit(request(2, PlayerCommand::Stop)),
            Err(SubmitError::Unavailable)
        );
        let stopped = wait(
            &runtime,
            |e| matches!(e, PlayerEvent::Snapshot { state } if state.state == PlaybackState::Error),
        );
        if let PlayerEvent::Snapshot { state } = stopped {
            assert!(!state.capabilities.play);
        }
    }
    #[test]
    fn dropping_runtime_joins_worker_and_stops_active_provider() {
        let provider = crate::test_provider::TestProvider::default();
        let stops = provider.stops.clone();
        let mut core = PlaybackCoordinator::default();
        core.register(provider);
        let runtime = PlayerRuntime::start(core).unwrap();
        runtime
            .submit(request(
                1,
                PlayerCommand::ReplaceQueue {
                    items: demo_tracks(),
                    start: 0,
                    context: PlaybackContext::UserQueue,
                },
            ))
            .unwrap();
        wait(&runtime, |e| {
            matches!(e, PlayerEvent::CommandResult { request_id: 1, .. })
        });
        drop(runtime);
        assert_eq!(stops.load(Ordering::Relaxed), 1);
    }
}

#[cfg(test)]
mod shutdown_tests {
    use super::*;
    use player_queue::*;
    struct Recorder {
        saved: Arc<Mutex<Option<TransportSession>>>,
        updates: VecDeque<SessionUpdate>,
    }
    impl SessionStore for Recorder {
        fn submit(&mut self, action: SessionAction) -> Result<(), PlayerError> {
            match action {
                SessionAction::Initialize { .. } => {
                    self.updates.push_back(SessionUpdate::Loaded {
                        session: None,
                        issues: vec![],
                    });
                    self.updates.push_back(SessionUpdate::SoundLoaded(Ok(
                        player_domain::SoundSettings::default(),
                    )));
                }
                SessionAction::Save { session, .. } => *self.saved.lock().unwrap() = Some(*session),
                _ => {}
            }
            Ok(())
        }
        fn poll(&mut self) -> Option<SessionUpdate> {
            self.updates.pop_front()
        }
    }
    #[test]
    fn immediate_shutdown_preserves_every_admitted_transport_change() {
        for _ in 0..40 {
            let saved = Arc::new(Mutex::new(None));
            let mut core = PlaybackCoordinator::default();
            core.register_session_store(Recorder {
                saved: saved.clone(),
                updates: VecDeque::new(),
            });
            let runtime = PlayerRuntime::start(core).unwrap();
            for (n, command) in [
                crate::PlayerCommand::InitializeSession {
                    directory: "/unused".into(),
                    access: SessionAccess::default(),
                },
                crate::PlayerCommand::ClearQueue,
                crate::PlayerCommand::SetVolume { volume: 0.42 },
                crate::PlayerCommand::SetRepeat {
                    mode: player_domain::RepeatMode::One,
                },
            ]
            .into_iter()
            .enumerate()
            {
                runtime
                    .submit(CommandRequest {
                        request_id: n as u64 + 1,
                        command,
                    })
                    .unwrap();
            }
            drop(runtime);
            let saved = saved.lock().unwrap();
            let session = saved.as_ref().expect("final checkpoint");
            assert_eq!(session.volume, 0.42);
            assert_eq!(
                session.queue.snapshot().repeat,
                player_domain::RepeatMode::One
            );
            assert!(session.queue.current().is_none());
        }
    }
}
