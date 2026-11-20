use crate::*;
use player_domain::*;
use player_queue::*;
use std::{
    collections::{BTreeMap, VecDeque},
    time::Duration,
};
mod host_analysis;
mod owned_recovery;
mod remote;
mod remote_session;

const PREVIOUS_RESTART_MS: u64 = 4_000;

pub struct PlaybackCoordinator {
    remote: remote::RemoteRegistry,
    host_analysis: host_analysis::HostAnalysis,
    providers: BTreeMap<String, Box<dyn PlaybackProvider>>,
    catalogs: BTreeMap<String, Box<dyn CatalogProvider>>,
    queue: QueueEngine,
    library: Option<Box<dyn LibraryProvider>>,
    library_play_token: u64,
    state: PlaybackState,
    position: u64,
    volume: f32,
    sound: SoundState,
    sound_serial: u64,
    sound_changed: bool,
    sound_loading: bool,
    sound_dirty: bool,
    sound_elapsed: Duration,
    active: Option<String>,
    revision: u64,
    events: VecDeque<PlayerEvent>,
    last_analysis: Option<(MediaId, AnalysisFrame)>,
    modulation: player_visuals::ModulationBus,
    session_store: Option<Box<dyn SessionStore>>,
    session_state: SessionState,
    session_load_epoch: Option<u64>,
    transport_epoch: u64,
    session_serial: u64,
    session_dirty: bool,
    session_archive_dirty: bool,
    session_elapsed: Duration,
    queue_issues: Vec<QueueIssue>,
    provider_loaded: bool,
    wanted_playing: bool,
    initial_volume: Option<f32>,
    initial_shuffle: Option<bool>,
    initial_repeat: Option<RepeatMode>,
}
impl Default for PlaybackCoordinator {
    fn default() -> Self {
        Self {
            remote: remote::RemoteRegistry::default(),
            host_analysis: host_analysis::HostAnalysis::default(),
            providers: BTreeMap::new(),
            catalogs: BTreeMap::new(),
            queue: QueueEngine::default(),
            library: None,
            library_play_token: 0,
            state: PlaybackState::Idle,
            position: 0,
            volume: 0.7,
            sound: SoundState::default(),
            sound_serial: 0,
            sound_changed: false,
            sound_loading: false,
            sound_dirty: false,
            sound_elapsed: Duration::ZERO,
            active: None,
            revision: 0,
            events: VecDeque::new(),
            last_analysis: None,
            modulation: player_visuals::ModulationBus::default(),
            session_store: None,
            session_state: SessionState::default(),
            session_load_epoch: None,
            transport_epoch: 0,
            session_serial: 0,
            session_dirty: false,
            session_archive_dirty: false,
            session_elapsed: Duration::ZERO,
            queue_issues: vec![],
            provider_loaded: false,
            wanted_playing: false,
            initial_volume: None,
            initial_shuffle: None,
            initial_repeat: None,
        }
    }
}
impl PlaybackCoordinator {
    pub fn register_session_store(&mut self, store: impl SessionStore + 'static) {
        self.session_store = Some(Box::new(store));
    }
    fn session_action(&mut self, action: SessionAction) -> Result<(), PlayerError> {
        self.session_store
            .as_mut()
            .ok_or_else(|| {
                PlayerError::new(ErrorCode::DatabaseError, "Queue storage is unavailable")
            })?
            .submit(action)
    }
    fn checkpoint(&mut self, force: bool) {
        if !self.session_state.ready
            || !self.session_dirty
            || (!force && self.session_elapsed < Duration::from_secs(1))
        {
            return;
        }
        self.session_serial = self.session_serial.wrapping_add(1);
        let remote = self.remote_selection_for_storage();
        if self.remote.restored.is_none()
            && self.remote.candidate.is_none()
            && self.remote.active.is_some()
        {
            self.remote.last_selection = remote.clone();
        }
        let remote_only = !self.session_archive_dirty
            && (self.remote.active.is_some() || self.remote.candidate.is_some());
        let action = if remote_only {
            SessionAction::SaveRemote {
                serial: self.session_serial,
                selection: remote.map(Box::new),
            }
        } else {
            let session = TransportSession {
                remote,
                version: 2,
                queue: self.queue.clone(),
                position_ms: self.position,
                volume: self.volume,
                stopped: self
                    .remote
                    .local_stopped
                    .filter(|_| self.remote.active.is_some())
                    .unwrap_or(matches!(
                        self.state,
                        PlaybackState::Stopped | PlaybackState::Idle
                    )),
                root_paths: vec![],
            };
            SessionAction::Save {
                serial: self.session_serial,
                session: Box::new(session),
            }
        };
        self.session_elapsed = Duration::ZERO;
        match self.session_action(action) {
            Ok(()) => {
                self.session_dirty = false;
                if !remote_only {
                    self.session_archive_dirty = false;
                }
                self.session_state.saving = true;
            }
            Err(error) => {
                self.session_state.issue = Some(error);
                self.session_state.saving = false;
            }
        }
    }
    fn apply_equalizer(&mut self) -> Result<(), PlayerError> {
        for (id, provider) in &mut self.providers {
            if provider.capabilities().equalizer {
                let restricted = self.active.as_ref() == Some(id)
                    && self
                        .queue
                        .current()
                        .is_some_and(|item| !item.capabilities.equalizer);
                provider.set_equalizer(if restricted {
                    EqualizerSettings::default()
                } else {
                    self.sound.settings.equalizer
                })?;
            }
        }
        Ok(())
    }
    fn checkpoint_sound(&mut self, force: bool) {
        if !self.sound.ready
            || !self.sound_dirty
            || self.sound.issue.is_some()
            || (!force && self.sound_elapsed < Duration::from_millis(250))
        {
            return;
        }
        self.sound_serial = self.sound_serial.wrapping_add(1);
        match self.session_action(SessionAction::SaveSound {
            serial: self.sound_serial,
            settings: self.sound.settings.clone(),
        }) {
            Ok(()) => {
                self.sound_dirty = false;
                self.sound.saving = true;
            }
            Err(error) => self.sound.issue = Some(error),
        }
    }
    fn poll_session(&mut self) {
        for _ in 0..4 {
            let Some(update) = self.session_store.as_mut().and_then(|s| s.poll()) else {
                break;
            };
            match update {
                SessionUpdate::SoundLoaded(result) => {
                    self.sound_loading = false;
                    match result {
                        Ok(settings) => {
                            self.sound.ready = true;
                            self.sound.saving = self.sound_dirty;
                            self.sound.issue = None;
                            if !self.sound_changed {
                                self.sound.settings = settings;
                                if let Err(error) = self.apply_equalizer() {
                                    self.sound.issue = Some(error);
                                }
                            }
                            self.checkpoint_sound(true);
                        }
                        Err(error) => {
                            self.sound.ready = false;
                            self.sound.saving = false;
                            self.sound.issue = Some(error);
                        }
                    }
                }
                SessionUpdate::SoundSaved { serial, result } => {
                    if serial == self.sound_serial {
                        self.sound.saving = self.sound_dirty;
                        if let Err(error) = result {
                            self.sound.saving = false;
                            self.sound.issue = Some(error);
                            self.sound_dirty = true;
                        } else {
                            self.sound.issue = None;
                        }
                    }
                }
                SessionUpdate::Loaded { session, issues } => {
                    let apply = self.session_load_epoch.take() == Some(self.transport_epoch);
                    self.session_state.ready = true;
                    self.session_state.issue = None;
                    if apply && let Some(session) = session {
                        self.remote.local_stopped =
                            self.remote.active.as_ref().map(|_| session.stopped);
                        if !self.remote.restore_cancelled {
                            self.remote.candidate = session.remote;
                        }
                        self.queue = session.queue;
                        self.volume = self.initial_volume.unwrap_or(session.volume);
                        if let Some(enabled) = self.initial_shuffle {
                            self.queue.set_shuffle(enabled);
                        }
                        if let Some(mode) = self.initial_repeat {
                            self.queue.set_repeat(mode);
                        }
                        self.position = self
                            .queue
                            .current()
                            .map_or(0, |i| session.position_ms.min(i.duration_ms));
                        self.state = if self.queue.current().is_none() {
                            PlaybackState::Idle
                        } else if session.stopped {
                            PlaybackState::Stopped
                        } else {
                            PlaybackState::Paused
                        };
                        self.active = self.queue.current().map(|i| i.id.provider().to_owned());
                        self.provider_loaded = false;
                        self.queue_issues = issues;
                        self.wanted_playing = false;
                        self.session_state.restored = true;
                        self.maybe_restore_remote();
                    }
                    if !apply {
                        self.session_dirty = true;
                    }
                    self.checkpoint(true);
                }
                SessionUpdate::Validated {
                    revision,
                    queue,
                    issues,
                } => {
                    if self.queue.revision() == revision {
                        self.queue = *queue;
                        self.queue_issues = issues;
                        self.session_archive_dirty = true;
                        self.session_dirty = true;
                        self.checkpoint(true);
                    }
                }
                SessionUpdate::Saved { serial } => {
                    if serial == self.session_serial {
                        self.session_state.saving = false;
                        self.session_state.issue = None;
                    }
                }
                SessionUpdate::Error(error) => {
                    self.session_state.issue = Some(error);
                    self.session_state.saving = false;
                    if self.session_state.ready {
                        self.session_dirty = true;
                        self.session_archive_dirty = true;
                    } else {
                        self.session_load_epoch = None;
                    }
                }
            }
            self.publish_snapshot();
        }
    }
    pub fn register(&mut self, provider: impl PlaybackProvider + 'static) {
        self.providers
            .insert(provider.id().into(), Box::new(provider));
    }
    pub fn register_catalog(&mut self, catalog: impl CatalogProvider + 'static) {
        self.catalogs.insert(catalog.id().into(), Box::new(catalog));
    }
    pub fn register_library(&mut self, library: impl LibraryProvider + 'static) {
        self.library = Some(Box::new(library));
    }
    fn library_action(&mut self, action: LibraryAction) -> Result<(), PlayerError> {
        self.library
            .as_mut()
            .ok_or_else(|| {
                PlayerError::new(ErrorCode::LibraryError, "Library service is unavailable")
            })?
            .submit(action)
    }
    fn poll_library(&mut self) {
        for _ in 0..12 {
            let Some(update) = self.library.as_mut().and_then(|l| l.poll()) else {
                break;
            };
            match update {
                LibraryUpdate::State(state) => self.publish(PlayerEvent::LibraryState { state }),
                LibraryUpdate::Page(page) => self.publish(PlayerEvent::LibraryPage { page }),
                LibraryUpdate::Error(error) => self.publish(PlayerEvent::Error { error }),
                LibraryUpdate::Playback {
                    token,
                    items,
                    start,
                    album_id,
                } => {
                    if token != self.library_play_token {
                        continue;
                    }
                    let context = album_id
                        .and_then(|id| MediaId::parse(id).ok())
                        .map(PlaybackContext::Album)
                        .unwrap_or(PlaybackContext::UserQueue);
                    if let Err(error) = self.execute(
                        PlayerCommand::ReplaceQueue {
                            items,
                            start,
                            context,
                        },
                        None,
                    ) {
                        self.publish(PlayerEvent::Error { error });
                    }
                    self.publish_snapshot();
                }
            }
        }
    }
    pub fn snapshot(&self) -> PlayerSnapshot {
        if let Some(snapshot) = self.remote_snapshot() {
            return snapshot;
        }
        let provider = self.active.as_ref().and_then(|id| self.providers.get(id));
        PlayerSnapshot {
            host_analysis_status: None,
            host_spatial_status: None,
            host_analysis_revoked_through: self.host_analysis_revoked_through(),
            enhancement: None,
            restored_remote: None,
            revision: self.revision,
            state: self.state,
            position_ms: self.position,
            playback_rate: 1.0,
            volume: self.volume,
            sound: self.sound.clone(),
            output_statistics: provider.and_then(|p| p.output_statistics()),
            active_provider: self.active.clone(),
            provider_connection: provider
                .map_or(ProviderConnectionState::Disconnected, |p| p.connection()),
            capabilities: provider.map_or(Capabilities::default(), |p| {
                self.queue.current().map_or(p.capabilities(), |item| {
                    let mut capabilities = p.capabilities().intersection(item.capabilities);
                    if self.queue_issues.iter().any(|issue| issue.id == item.id) {
                        capabilities.play = false;
                        capabilities.seek = false;
                    }
                    capabilities
                })
            }),
            queue: self.queue.snapshot(),
            transport: TransportControls::LOCAL,
            playback_session_active: self.provider_loaded
                && matches!(self.state, PlaybackState::Playing | PlaybackState::Paused),
            session: self.session_state.clone(),
            queue_issues: self.queue_issues.clone(),
        }
    }
    pub fn peek_event(&self) -> Option<&PlayerEvent> {
        self.events.front()
    }
    pub fn pop_event(&mut self) -> Option<PlayerEvent> {
        self.events.pop_front()
    }
    pub fn publish(&mut self, event: PlayerEvent) {
        self.observe_host_spatial_dispatch(&event);
        // Only the latest state is needed while UI delivery is suspended.
        if matches!(event, PlayerEvent::Snapshot { .. }) {
            self.events
                .retain(|e| !matches!(e, PlayerEvent::Snapshot { .. }));
        }
        if matches!(event, PlayerEvent::AudioAnalysis { .. }) {
            self.events
                .retain(|e| !matches!(e, PlayerEvent::AudioAnalysis { .. }));
        }
        if self.events.len() == 64 {
            self.events.pop_front();
        }
        self.events.push_back(event);
    }
    /// Polled independently of transport snapshots; never clones the whole queue at 50 Hz.
    pub fn poll_analysis(&mut self) {
        let mut capabilities = Capabilities::default();
        let owned = self.poll_host_analysis();
        let current = if let Some(current) = owned {
            capabilities.pcm_analysis = true;
            Some(current)
        } else if self.remote.active.is_none()
            && self.provider_loaded
            && self.state == PlaybackState::Playing
        {
            self.active
                .as_ref()
                .and_then(|id| self.providers.get(id))
                .and_then(|provider| {
                    let item = self.queue.current()?;
                    capabilities = provider.capabilities().intersection(item.capabilities);
                    if provider.connection() != ProviderConnectionState::Connected
                        || !capabilities.pcm_analysis
                    {
                        return None;
                    }
                    provider
                        .analysis()
                        .filter(AnalysisFrame::is_valid)
                        .map(|frame| (item.id.clone(), frame))
                })
        } else {
            None
        };
        if current != self.last_analysis {
            self.last_analysis = current.clone();
            let modulation = self.modulation.update_audio(
                capabilities,
                current.as_ref().map(|(id, _)| id),
                current.as_ref().map(|(_, frame)| frame),
            );
            self.publish(PlayerEvent::AudioAnalysis {
                media_id: current.as_ref().map(|(id, _)| id.clone()),
                frame: current.map(|(_, frame)| Box::new(frame)),
                modulation,
            });
        }
    }
    pub fn publish_snapshot(&mut self) {
        self.revoke_stale_host_analysis();
        self.revision += 1;
        self.publish(PlayerEvent::Snapshot {
            state: Box::new(self.snapshot()),
        });
        self.poll_analysis();
    }
    pub fn send(&mut self, command: PlayerCommand) -> Result<(), PlayerError> {
        let result = self.execute(command, None);
        self.checkpoint(true);
        if let Err(error) = &result {
            self.publish(PlayerEvent::Error {
                error: error.clone(),
            });
        }
        self.publish_snapshot();
        result
    }
    pub fn send_request(&mut self, request: CommandRequest) {
        let is_next = matches!(&request.command, PlayerCommand::Next);
        let is_play_pause = matches!(
            &request.command,
            PlayerCommand::Play
                | PlayerCommand::Pause
                | PlayerCommand::PauseConfirmedRestoredResume { .. }
        ) || matches!(&request.command, PlayerCommand::Remote {
            provider,
            action: RemoteAction::Control {
                control: RemoteControl::PauseOnDevice { .. },
            },
        } if self.remote.active.as_ref() == Some(provider));
        let is_seek = matches!(&request.command, PlayerCommand::Seek { .. })
            || (matches!(&request.command, PlayerCommand::Previous)
                && self.remote.active.is_some()
                && self.snapshot().position_ms >= PREVIOUS_RESTART_MS);
        let check_owned_handoff = self.may_confirm_owned_handoff(&request.command);
        let result = self.execute(request.command, Some(request.request_id));
        let admitted = result.is_ok();
        self.checkpoint(true);
        self.publish(PlayerEvent::CommandResult {
            request_id: request.request_id,
            error: result.err(),
        });
        self.publish_snapshot();
        if admitted && is_next {
            self.poll_owned_request(|provider| {
                provider.owned_next_ready_for_request(request.request_id)
            });
        } else if admitted && is_seek {
            self.poll_owned_request(|provider| {
                provider.owned_seek_ready_for_request(request.request_id)
            });
        } else if admitted && is_play_pause {
            self.poll_owned_request(|provider| {
                provider.owned_play_pause_ready_for_request(request.request_id)
            });
        }
        if admitted && check_owned_handoff {
            self.poll_owned_handoff();
        }
    }
    pub fn shutdown(&mut self) {
        self.cancel_deferred_spatial_play();
        self.clear_host_analysis();
        self.host_analysis.source.take();
        // Only destruction may wait for initial IO. Normal command/tick paths never wait.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while (self.session_load_epoch.is_some() || self.sound_loading)
            && std::time::Instant::now() < deadline
        {
            self.poll_session();
            if self.session_load_epoch.is_some() || self.sound_loading {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        self.session_dirty = true;
        self.checkpoint(true);
        // Destruction runs on the bridge queue. Preserve commands made while the initial
        // disk read was in flight; the store writes only after a successful read.
        if !self.session_state.ready
            && self
                .session_load_epoch
                .is_some_and(|epoch| epoch != self.transport_epoch)
        {
            let session = TransportSession {
                remote: self.remote_selection_for_storage(),
                version: 2,
                queue: self.queue.clone(),
                position_ms: self.position,
                volume: self.volume,
                stopped: matches!(self.state, PlaybackState::Stopped | PlaybackState::Idle),
                root_paths: vec![],
            };
            let _ = self.session_action(SessionAction::Save {
                serial: self.session_serial.wrapping_add(1),
                session: Box::new(session),
            });
        }
        if let Ok(provider) = self.active_mut() {
            let _ = provider.stop();
        }
        self.state = PlaybackState::Stopped;
        self.checkpoint_sound(true);
        if self.sound_loading && self.sound_dirty {
            let _ = self.session_action(SessionAction::SaveSound {
                serial: self.sound_serial.wrapping_add(1),
                settings: self.sound.settings.clone(),
            });
        }
        self.session_store.take(); // Drains the latest checkpoint and joins outside MainActor.
    }
    fn active_mut(&mut self) -> Result<&mut Box<dyn PlaybackProvider>, PlayerError> {
        self.active
            .as_ref()
            .and_then(|id| self.providers.get_mut(id))
            .ok_or_else(|| PlayerError::invalid("Select a track first"))
    }
    fn accept(&mut self, playback: ProviderPlayback) {
        self.state = playback.state;
        self.position = playback.position_ms;
    }
    fn require_capability(&self, available: bool) -> Result<(), PlayerError> {
        if available {
            Ok(())
        } else {
            Err(PlayerError::new(
                ErrorCode::UnsupportedCapability,
                "The active source does not support this control",
            ))
        }
    }
    fn start(&mut self, item: &MediaItem) -> Result<(), PlayerError> {
        self.start_at(item, 0, true)
    }
    fn start_at(
        &mut self,
        item: &MediaItem,
        position: u64,
        autoplay: bool,
    ) -> Result<(), PlayerError> {
        if let Some(issue) = self.queue_issues.iter().find(|issue| issue.id == item.id) {
            return Err(issue.error.clone());
        }
        let id = item.id.provider();
        let provider = self
            .providers
            .get(id)
            .ok_or_else(|| PlayerError::unavailable(id))?;
        if provider.connection() != ProviderConnectionState::Connected {
            return Err(PlayerError::unavailable(id));
        }
        provider.validate(item)?;
        self.require_capability(provider.capabilities().intersection(item.capabilities).play)?;
        self.leave_remote()?;
        if self.active.as_deref().is_some_and(|old| old != id) {
            self.active_mut()?.stop()?;
            self.state = PlaybackState::Stopped;
            self.position = 0;
        }
        self.state = PlaybackState::Loading;
        let provider = self.providers.get_mut(id).unwrap();
        let result = (|| {
            if provider.capabilities().volume {
                provider.set_volume(self.volume)?;
            }
            if provider.capabilities().equalizer {
                provider.set_equalizer(if item.capabilities.equalizer {
                    self.sound.settings.equalizer
                } else {
                    EqualizerSettings::default()
                })?;
            }
            provider.load_at(item, position, autoplay)
        })();
        match result {
            Ok(playback) => {
                self.active = Some(id.into());
                self.provider_loaded = true;
                self.wanted_playing = autoplay;
                self.accept(playback);
                Ok(())
            }
            Err(error) => {
                let _ = provider.stop();
                self.state = PlaybackState::Error;
                Err(error)
            }
        }
    }
    fn execute(
        &mut self,
        mut command: PlayerCommand,
        request_id: Option<u64>,
    ) -> Result<(), PlayerError> {
        match command {
            PlayerCommand::OwnedOutputInterrupted {
                provider,
                generation,
                sequence,
                device_id,
            } => {
                return self.owned_output_interrupted(&provider, generation, sequence, &device_id);
            }
            PlayerCommand::CancelOwnedOutputRecovery { provider } => {
                return self.cancel_owned_output_recovery(&provider);
            }
            PlayerCommand::HostAnalysis { lease } => {
                return self.update_host_analysis(lease);
            }
            PlayerCommand::RetireHostAnalysis { generation } => {
                return self.retire_host_analysis_generation(generation);
            }
            PlayerCommand::HostAnalysisAfterSeek {
                lease,
                request_id,
                sequence,
            } => {
                return self.update_host_analysis_after_seek(lease, request_id, sequence);
            }
            PlayerCommand::HostAnalysisAfterClockChange {
                lease,
                retired_generation,
                origin_sequence,
                sequence,
            } => {
                return self.update_host_analysis_after_clock_change(
                    lease,
                    retired_generation,
                    origin_sequence,
                    sequence,
                );
            }
            PlayerCommand::CancelRestoredResume {
                operation_id,
                device_id,
            } => {
                return self.cancel_restored_resume(operation_id, &device_id);
            }
            PlayerCommand::PauseConfirmedRestoredResume {
                operation_id,
                device_id,
            } => {
                return self.pause_confirmed_restored_resume(operation_id, &device_id, request_id);
            }
            _ => {}
        }
        self.note_owned_recovery_intent(&command);
        self.check_host_spatial_clock(&command);
        if let PlayerCommand::Remote { provider, action } = &mut command
            && matches!(action, RemoteAction::AdoptOutput)
        {
            // Host snapshots can lag a profile page. Guard startup adoption at
            // the coordinator before clearing any analysis or transport state.
            if !self.session_state.ready
                || !self.remote.accounts.contains_key(provider)
                || self.remote.restored.is_some()
                || self.remote.candidate.is_some()
                || self.remote.restore_cancelled
            {
                return Ok(());
            }
            *action = RemoteAction::Activate;
        }
        let spatial_transport = if host_analysis::invalidates_lease(&command) {
            self.remote.confirmed_resume_pause = None;
            let preserved = self.preserve_host_spatial_transport(&command);
            let paused_seek = self.preserve_paused_spatial_seek(&command);
            let relative = self.preserve_host_spatial_relative(&command, request_id);
            if !paused_seek
                && (!matches!(command, PlayerCommand::Play) || preserved.is_none())
                && !host_analysis::independent_remote_queue_edit(&command)
            {
                self.cancel_deferred_spatial_play();
            }
            if preserved.is_none()
                && !paused_seek
                && !relative
                && !(matches!(command, PlayerCommand::Next)
                    && self.prepare_host_spatial_next(request_id))
                && !(matches!(command, PlayerCommand::Seek { .. })
                    && request_id.is_some_and(|id| self.prepare_host_spatial_seek(id)))
            {
                self.clear_host_analysis();
            }
            preserved
        } else {
            None
        };
        if matches!(
            &command,
            PlayerCommand::OpenFile { .. }
                | PlayerCommand::ReplaceQueue { .. }
                | PlayerCommand::PlayLibrary { .. }
        ) {
            self.cancel_remote_restore_for_local();
        }
        if matches!(
            &command,
            PlayerCommand::OpenFile { .. }
                | PlayerCommand::ReplaceQueue { .. }
                | PlayerCommand::PlayLibrary { .. }
                | PlayerCommand::Play
                | PlayerCommand::Pause
                | PlayerCommand::Stop
                | PlayerCommand::Next
                | PlayerCommand::Previous
                | PlayerCommand::SkipPrevious
                | PlayerCommand::Seek { .. }
                | PlayerCommand::JumpQueue { .. }
                | PlayerCommand::ClearQueue
                | PlayerCommand::Remote {
                    action: RemoteAction::Activate
                        | RemoteAction::Disconnect
                        | RemoteAction::Control { .. },
                    ..
                }
        ) {
            self.library_play_token = self.library_play_token.wrapping_add(1);
            self.cancel_pending_remote_switch();
        }
        if let Some(result) = self.defer_local_switch(&command) {
            return result;
        }
        if spatial_transport == Some(PlaybackState::Playing)
            && matches!(command, PlayerCommand::Play)
            && self.defer_host_spatial_resume(request_id)
        {
            return Ok(());
        }
        if let Some(result) = self.route_remote(&command, request_id) {
            self.note_applied_host_output_clock(&command, result.is_ok());
            if result.is_err() && spatial_transport == Some(PlaybackState::Playing) {
                self.cancel_spatial_resume_clock_gap();
            }
            if result.is_err() && spatial_transport == Some(PlaybackState::Paused) {
                self.cancel_host_spatial_pause();
            }
            if result.is_err()
                && (matches!(
                    command,
                    PlayerCommand::Next | PlayerCommand::SkipPrevious | PlayerCommand::Seek { .. }
                ) || matches!(&command, PlayerCommand::Remote {
                        provider, action: RemoteAction::OutputClock { .. },
                    } if self.remote.active.as_ref() == Some(provider)))
            {
                self.cancel_host_spatial_next();
            }
            return result;
        }
        if matches!(
            &command,
            PlayerCommand::OpenFile { .. }
                | PlayerCommand::ReplaceQueue { .. }
                | PlayerCommand::PlayNext { .. }
                | PlayerCommand::Play
                | PlayerCommand::Pause
                | PlayerCommand::Stop
                | PlayerCommand::Next
                | PlayerCommand::Previous
                | PlayerCommand::SkipPrevious
                | PlayerCommand::Seek { .. }
                | PlayerCommand::JumpQueue { .. }
                | PlayerCommand::RemoveQueueItem { .. }
                | PlayerCommand::ClearQueue
        ) {
            self.remote.restore_cancelled = true;
            self.remote.candidate = None;
            self.transport_epoch = self.transport_epoch.wrapping_add(1);
            self.session_dirty = true;
            self.session_archive_dirty = true;
            self.session_state.restored = false;
        }
        if matches!(
            &command,
            PlayerCommand::OpenFile { .. }
                | PlayerCommand::ReplaceQueue { .. }
                | PlayerCommand::Play
                | PlayerCommand::Pause
                | PlayerCommand::Stop
                | PlayerCommand::Next
                | PlayerCommand::Previous
                | PlayerCommand::SkipPrevious
                | PlayerCommand::Seek { .. }
                | PlayerCommand::JumpQueue { .. }
                | PlayerCommand::ClearQueue
        ) {
            self.library_play_token = self.library_play_token.wrapping_add(1);
        }
        match command {
            PlayerCommand::HostAnalysis { .. }
            | PlayerCommand::OwnedOutputInterrupted { .. }
            | PlayerCommand::CancelOwnedOutputRecovery { .. }
            | PlayerCommand::RetireHostAnalysis { .. }
            | PlayerCommand::HostAnalysisAfterSeek { .. }
            | PlayerCommand::HostAnalysisAfterClockChange { .. }
            | PlayerCommand::CancelRestoredResume { .. }
            | PlayerCommand::PauseConfirmedRestoredResume { .. } => {
                unreachable!("handled before transport routing")
            }
            PlayerCommand::RetryPlayback => {
                self.require_capability(self.snapshot().transport.retry)?;
                if self.state != PlaybackState::Error {
                    return Err(PlayerError::invalid(
                        "Retry is only available after a playback error",
                    ));
                }
                let item = self
                    .queue
                    .current()
                    .cloned()
                    .ok_or_else(|| PlayerError::invalid("Select a track first"))?;
                self.transport_epoch = self.transport_epoch.wrapping_add(1);
                self.start_at(&item, self.position.min(item.duration_ms), true)?;
                self.session_dirty = true;
                self.session_archive_dirty = true;
            }
            PlayerCommand::SetSound { settings } => {
                settings.validate()?;
                let previous = self.sound.settings.clone();
                self.sound.settings = settings;
                if let Err(error) = self.apply_equalizer() {
                    self.sound.settings = previous;
                    let _ = self.apply_equalizer();
                    return Err(error);
                }
                self.sound_changed = true;
                self.sound_dirty = true;
                self.sound.saving = self.session_state.enabled;
                self.sound_elapsed = Duration::ZERO;
            }
            PlayerCommand::RetrySoundStorage => {
                self.session_action(SessionAction::ReloadSound)?;
                self.sound_loading = true;
                self.sound.ready = false;
                self.sound.saving = true;
                self.sound.issue = None;
            }
            PlayerCommand::Remote { .. } => unreachable!("Remote commands are routed above"),
            PlayerCommand::InitializeSession { directory, access } => {
                if self.session_state.ready || self.session_load_epoch.is_some() {
                    return Err(PlayerError::invalid("Queue storage is already initialized"));
                }
                self.session_action(SessionAction::Initialize { directory, access })?;
                self.session_load_epoch = Some(self.transport_epoch);
                self.sound_loading = true;
                self.session_state.enabled = true;
                self.session_state.issue = None;
            }
            PlayerCommand::RefreshSessionAccess { access } => {
                self.session_action(SessionAction::Validate {
                    revision: self.queue.revision(),
                    queue: Box::new(self.queue.clone()),
                    access,
                })?;
            }
            PlayerCommand::JumpQueue { revision, index } => {
                self.queue.check_revision(revision)?;
                let mut queue = self.queue.clone();
                let item = queue.jump(index)?;
                self.start_at(
                    &item,
                    0,
                    self.wanted_playing || self.queue.current().is_none(),
                )?;
                self.queue = queue;
            }
            PlayerCommand::RemoveQueueItem { revision, index } => {
                self.queue.check_revision(revision)?;
                self.queue.remove_upcoming(index)?;
            }
            PlayerCommand::ClearQueue => {
                if self.active.is_some() && self.provider_loaded {
                    self.active_mut()?.stop()?;
                }
                self.queue.clear();
                self.queue_issues.clear();
                self.active = None;
                self.provider_loaded = false;
                self.state = PlaybackState::Idle;
                self.position = 0;
                self.wanted_playing = false;
            }
            PlayerCommand::InitializeLibrary { directory, roots } => {
                self.library_action(LibraryAction::Initialize { directory, roots })?
            }
            PlayerCommand::AddLibraryRoot { root } => {
                self.library_action(LibraryAction::AddRoot(root))?
            }
            PlayerCommand::RemoveLibraryRoot { id } => {
                self.library_action(LibraryAction::RemoveRoot(id))?
            }
            PlayerCommand::ScanLibrary { root_id } => {
                self.library_action(LibraryAction::Scan(root_id))?
            }
            PlayerCommand::CancelLibraryScan => self.library_action(LibraryAction::CancelScan)?,
            PlayerCommand::QueryLibrary { query } => {
                self.library_action(LibraryAction::Query(query))?
            }
            PlayerCommand::PlayLibrary { query, track_id } => {
                self.library_play_token = self.library_play_token.wrapping_add(1);
                self.library_action(LibraryAction::PreparePlayback {
                    token: self.library_play_token,
                    query,
                    track_id,
                })?;
            }
            PlayerCommand::OpenFile { path } => {
                let catalog = self
                    .catalogs
                    .get("local")
                    .ok_or_else(|| PlayerError::unavailable("local"))?;
                let item = catalog.resolve_file(&path)?;
                item.validate()?;
                let mut queue = self.queue.clone();
                queue.replace(vec![item.clone()], 0, PlaybackContext::UserQueue)?;
                self.queue_issues.clear();
                self.start(&item)?;
                self.queue = queue;
                self.publish(PlayerEvent::MediaUpdated { item });
            }
            PlayerCommand::ReplaceQueue {
                items,
                start,
                context,
            } => {
                for item in &items {
                    item.validate()?;
                }
                let mut queue = self.queue.clone();
                let item = queue.replace(items, start, context)?;
                self.queue_issues.clear();
                self.start(&item)?;
                self.queue = queue;
            }
            PlayerCommand::PlayNext { item } => {
                item.validate()?;
                let provider = self
                    .providers
                    .get(item.id.provider())
                    .ok_or_else(|| PlayerError::unavailable(item.id.provider()))?;
                if provider.connection() != ProviderConnectionState::Connected {
                    return Err(PlayerError::unavailable(item.id.provider()));
                }
                self.require_capability(
                    provider.capabilities().intersection(item.capabilities).play,
                )?;
                self.queue_issues.retain(|issue| issue.id != item.id);
                self.queue.play_next(item)?;
            }
            PlayerCommand::Play => {
                if self.queue.current().is_none() {
                    return self.advance(false);
                }
                if self.state == PlaybackState::Playing {
                    return Ok(());
                }
                self.require_capability(self.snapshot().capabilities.play)?;
                if !self.provider_loaded
                    || matches!(self.state, PlaybackState::Stopped | PlaybackState::Error)
                {
                    let item = self
                        .queue
                        .current()
                        .cloned()
                        .ok_or_else(|| PlayerError::invalid("Select a track first"))?;
                    let position =
                        if matches!(self.state, PlaybackState::Paused | PlaybackState::Error) {
                            self.position
                        } else {
                            0
                        };
                    self.start_at(&item, position, true)?;
                } else {
                    let playback = self.active_mut()?.resume()?;
                    self.wanted_playing = true;
                    let ended = playback.ended;
                    self.accept(playback);
                    if ended {
                        self.advance(true)?;
                    }
                }
            }
            PlayerCommand::Pause => {
                if self.state == PlaybackState::Paused {
                    return Ok(());
                }
                if !matches!(self.state, PlaybackState::Playing | PlaybackState::Loading) {
                    return Err(PlayerError::invalid(
                        "Only playing/loading media can be paused",
                    ));
                }
                self.require_capability(self.snapshot().capabilities.pause)?;
                let playback = self.active_mut()?.pause()?;
                self.wanted_playing = false;
                self.accept(playback);
            }
            PlayerCommand::Stop => {
                if self.active.is_some() {
                    if self.provider_loaded {
                        self.active_mut()?.stop()?;
                    }
                    self.state = PlaybackState::Stopped;
                }
                self.position = 0;
                self.wanted_playing = false;
            }
            PlayerCommand::Seek { position_ms } => {
                self.require_capability(self.snapshot().capabilities.seek)?;
                if !self.provider_loaded {
                    self.position =
                        position_ms.min(self.queue.current().map_or(0, |i| i.duration_ms));
                } else {
                    let playback = self.active_mut()?.seek(position_ms)?;
                    self.accept(playback);
                }
            }
            PlayerCommand::SetVolume { volume } => {
                if !volume.is_finite() || !(0.0..=1.0).contains(&volume) {
                    return Err(PlayerError::invalid(
                        "Volume must be finite and between 0 and 1",
                    ));
                }
                if self.active.is_some() {
                    self.require_capability(self.snapshot().capabilities.volume)?;
                    self.active_mut()?.set_volume(volume)?;
                }
                self.volume = volume;
                self.initial_volume = Some(volume);
                self.session_dirty = true;
                self.session_archive_dirty = true;
            }
            PlayerCommand::SetShuffle { enabled } => {
                self.queue.set_shuffle(enabled);
                self.initial_shuffle = Some(enabled);
                self.session_dirty = true;
                self.session_archive_dirty = true;
            }
            PlayerCommand::SetRepeat { mode } => {
                self.queue.set_repeat(mode);
                self.initial_repeat = Some(mode);
                self.session_dirty = true;
                self.session_archive_dirty = true;
            }
            PlayerCommand::Next => self.advance(false)?,
            PlayerCommand::Previous if self.position >= PREVIOUS_RESTART_MS => {
                return self.execute(PlayerCommand::Seek { position_ms: 0 }, None);
            }
            PlayerCommand::Previous | PlayerCommand::SkipPrevious => {
                let mut queue = self.queue.clone();
                if let Some(item) = queue.previous() {
                    self.start_at(&item, 0, self.wanted_playing)?;
                    self.queue = queue;
                } else if self.queue.current().is_some() {
                    self.require_capability(self.snapshot().capabilities.seek)?;
                    if !self.provider_loaded {
                        self.position = 0;
                    } else {
                        let playback = self.active_mut()?.seek(0)?;
                        self.accept(playback);
                    }
                } else {
                    return Err(PlayerError::invalid("Select a track first"));
                }
            }
        }
        Ok(())
    }
    fn advance(&mut self, automatic: bool) -> Result<(), PlayerError> {
        let mut queue = self.queue.clone();
        if let Some(item) = queue.next(automatic) {
            self.start_at(
                &item,
                0,
                automatic || self.wanted_playing || self.queue.current().is_none(),
            )?;
            self.queue = queue;
        } else if self.active.is_some() {
            self.active_mut()?.stop()?;
            self.state = PlaybackState::Stopped;
            self.wanted_playing = false;
            if !automatic {
                self.position = 0;
            }
        } else {
            return Err(PlayerError::invalid("Select a track first"));
        }
        Ok(())
    }
    /// M1 advances a silent mock clock. Real local providers reconcile from presented samples.
    pub fn tick(&mut self, elapsed: Duration) {
        self.poll_session();
        self.poll_remote();
        self.session_elapsed = self.session_elapsed.saturating_add(elapsed);
        if self.remote.active.is_some() && self.snapshot().state == PlaybackState::Playing {
            self.session_dirty = true;
        }
        self.sound_elapsed = self.sound_elapsed.saturating_add(elapsed);
        self.checkpoint_sound(false);
        self.checkpoint(false);
        self.poll_library();
        self.poll_local_playback(elapsed);
    }

    pub(crate) fn poll_loading_local_output(&mut self) {
        if self.state == PlaybackState::Loading
            && self
                .active
                .as_ref()
                .and_then(|id| self.providers.get(id))
                .is_some_and(|provider| provider.capabilities().pcm_analysis)
        {
            // Local PCM providers reconcile their device clock without advancing it.
            self.poll_local_playback(Duration::ZERO);
        }
    }

    fn poll_local_playback(&mut self, elapsed: Duration) {
        if self.remote.active.is_some() {
            return;
        }
        if !self.provider_loaded {
            return;
        }
        if !matches!(
            self.state,
            PlaybackState::Playing | PlaybackState::Loading | PlaybackState::Paused
        ) {
            return;
        }
        let result = self.active_mut().and_then(|p| p.tick(elapsed));
        let update = self
            .active_mut()
            .ok()
            .and_then(|provider| provider.take_media_update());
        let metadata_updated = update.is_some();
        if let Some(item) = update {
            self.queue.update_current(item.clone());
            self.publish(PlayerEvent::MediaUpdated { item });
        }
        match result {
            Ok(Some(playback)) => {
                self.session_dirty = true;
                self.session_archive_dirty = true;
                self.accept(playback);
                if playback.ended
                    && let Err(error) = self.advance(true)
                {
                    self.state = PlaybackState::Error;
                    self.publish(PlayerEvent::Error { error });
                }
                self.publish_snapshot();
                if playback.ended {
                    self.checkpoint(true);
                }
            }
            Ok(None) => {
                if metadata_updated {
                    self.publish_snapshot();
                }
            }
            Err(error) => {
                self.state = PlaybackState::Error;
                self.publish(PlayerEvent::Error { error });
                self.publish_snapshot();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use provider_mock::{MockProvider, demo_tracks};
    fn coordinator() -> PlaybackCoordinator {
        let mut c = PlaybackCoordinator::default();
        c.register(MockProvider::default());
        c.register(provider_local::LocalPlaybackProvider::default());
        c.send(PlayerCommand::ReplaceQueue {
            items: demo_tracks(),
            start: 0,
            context: PlaybackContext::UserQueue,
        })
        .unwrap();
        c
    }
    #[test]
    fn commands_and_clock_have_one_source_of_truth() {
        let mut c = coordinator();
        c.tick(Duration::from_millis(1200));
        assert_eq!(c.snapshot().position_ms, 1200);
        c.send(PlayerCommand::Pause).unwrap();
        c.tick(Duration::from_secs(5));
        assert_eq!(c.snapshot().position_ms, 1200);
        c.send(PlayerCommand::Seek {
            position_ms: u64::MAX,
        })
        .unwrap();
        assert_eq!(c.snapshot().position_ms, 213000);
        c.send(PlayerCommand::Play).unwrap();
        c.tick(Duration::from_millis(1));
        assert_eq!(c.snapshot().queue.current.unwrap().title, "Soft Focus");
    }
    #[test]
    fn provider_failure_keeps_the_playing_queue_and_rejects_bad_volume() {
        let mut c = coordinator();
        let before = c.snapshot();
        let mut local = demo_tracks()[0].clone();
        local.id = MediaId::parse("local:track:missing").unwrap();
        assert!(
            c.send(PlayerCommand::ReplaceQueue {
                items: vec![local],
                start: 0,
                context: PlaybackContext::UserQueue
            })
            .is_err()
        );
        assert_eq!(c.snapshot().queue.current, before.queue.current);
        assert_eq!(c.snapshot().state, PlaybackState::Playing);
        assert!(
            c.send(PlayerCommand::SetVolume { volume: f32::NAN })
                .is_err()
        );
        assert_eq!(c.snapshot().volume, 0.7);
    }
    #[test]
    fn completion_repeat_and_manual_next_are_distinct() {
        let mut c = coordinator();
        c.send(PlayerCommand::SetRepeat {
            mode: RepeatMode::One,
        })
        .unwrap();
        c.tick(Duration::from_secs(1000));
        assert_eq!(c.snapshot().queue.current.unwrap().title, "First Light");
        assert_eq!(c.snapshot().position_ms, 0);
        c.send(PlayerCommand::Next).unwrap();
        assert_eq!(c.snapshot().queue.current.unwrap().title, "Soft Focus");
    }
    #[test]
    fn track_restrictions_cannot_grant_provider_capabilities() {
        let mut c = coordinator();
        let mut item = demo_tracks()[0].clone();
        item.capabilities.seek = false;
        item.capabilities.pcm_analysis = true;
        c.send(PlayerCommand::ReplaceQueue {
            items: vec![item],
            start: 0,
            context: PlaybackContext::UserQueue,
        })
        .unwrap();
        assert!(!c.snapshot().capabilities.seek);
        assert!(!c.snapshot().capabilities.pcm_analysis);
        assert!(c.send(PlayerCommand::Seek { position_ms: 10 }).is_err());
        assert_eq!(c.snapshot().position_ms, 0);
    }
    #[test]
    fn pending_snapshots_are_coalesced() {
        let mut c = coordinator();
        for _ in 0..1000 {
            c.tick(Duration::from_millis(1));
        }
        assert_eq!(c.events.len(), 1);
    }
    #[test]
    fn stop_pause_and_play_follow_explicit_transitions() {
        let mut idle = PlaybackCoordinator::default();
        idle.register(MockProvider::default());
        idle.send(PlayerCommand::Stop).unwrap();
        assert_eq!(idle.snapshot().state, PlaybackState::Idle);
        assert!(idle.send(PlayerCommand::Play).is_err());
        assert!(idle.send(PlayerCommand::Pause).is_err());
        let mut c = coordinator();
        c.send(PlayerCommand::Pause).unwrap();
        c.send(PlayerCommand::Pause).unwrap();
        assert_eq!(c.snapshot().state, PlaybackState::Paused);
        c.send(PlayerCommand::Stop).unwrap();
        assert_eq!(c.snapshot().position_ms, 0);
        assert!(c.send(PlayerCommand::Pause).is_err());
        assert_eq!(c.snapshot().state, PlaybackState::Stopped);
        c.send(PlayerCommand::Play).unwrap();
        assert_eq!(c.snapshot().state, PlaybackState::Playing);
    }
    #[test]
    fn load_failure_stops_unknown_output_and_a_valid_load_recovers() {
        let provider = crate::test_provider::TestProvider::default();
        let stops = provider.stops.clone();
        let mut c = PlaybackCoordinator::default();
        c.register(provider);
        let mut item = demo_tracks()[0].clone();
        item.title = "Cannot Load".into();
        assert!(
            c.send(PlayerCommand::ReplaceQueue {
                items: vec![item],
                start: 0,
                context: PlaybackContext::UserQueue
            })
            .is_err()
        );
        assert_eq!(c.snapshot().state, PlaybackState::Error);
        assert!(c.snapshot().queue.current.is_none());
        assert_eq!(stops.load(std::sync::atomic::Ordering::Relaxed), 1);
        c.send(PlayerCommand::ReplaceQueue {
            items: demo_tracks(),
            start: 0,
            context: PlaybackContext::UserQueue,
        })
        .unwrap();
        assert_eq!(c.snapshot().state, PlaybackState::Playing);
    }
    #[test]
    fn connection_failure_is_distinct_from_paused_playback() {
        let provider = crate::test_provider::TestProvider::default();
        let unavailable = provider.unavailable.clone();
        let mut c = PlaybackCoordinator::default();
        c.register(provider);
        c.send(PlayerCommand::ReplaceQueue {
            items: demo_tracks(),
            start: 0,
            context: PlaybackContext::UserQueue,
        })
        .unwrap();
        unavailable.store(true, std::sync::atomic::Ordering::Relaxed);
        c.tick(Duration::from_millis(10));
        assert_eq!(c.snapshot().state, PlaybackState::Error);
        assert_eq!(
            c.snapshot().provider_connection,
            ProviderConnectionState::Unavailable
        );
    }
    #[test]
    fn request_rejection_has_one_correlated_result_and_preserves_state() {
        let mut c = coordinator();
        while c.pop_event().is_some() {}
        c.send_request(CommandRequest {
            request_id: 42,
            command: PlayerCommand::SetVolume { volume: 2.0 },
        });
        assert!(matches!(
            c.pop_event(),
            Some(PlayerEvent::CommandResult {
                request_id: 42,
                error: Some(_)
            })
        ));
        assert!(matches!(c.pop_event(), Some(PlayerEvent::Snapshot { .. })));
        assert!(c.pop_event().is_none());
        assert_eq!(c.snapshot().volume, 0.7);
        assert_eq!(c.snapshot().state, PlaybackState::Playing);
    }
    #[test]
    fn fake_clock_preserves_fractional_milliseconds() {
        let mut c = coordinator();
        for _ in 0..1000 {
            c.tick(Duration::from_micros(500));
        }
        assert_eq!(c.snapshot().position_ms, 500);
    }
    #[test]
    fn loading_and_paused_metadata_reconcile_without_ui_commands() {
        for paused in [false, true] {
            let mut updated = demo_tracks()[0].clone();
            updated.title = "Decoded title".into();
            updated.duration_ms = 8000;
            let provider = crate::test_provider::TestProvider {
                deferred_load: true,
                media_update: Some(updated.clone()),
                ..Default::default()
            };
            let mut c = PlaybackCoordinator::default();
            c.register(provider);
            c.send(PlayerCommand::ReplaceQueue {
                items: demo_tracks(),
                start: 0,
                context: PlaybackContext::UserQueue,
            })
            .unwrap();
            assert_eq!(c.snapshot().state, PlaybackState::Loading);
            if paused {
                c.send(PlayerCommand::Pause).unwrap();
            }
            while c.pop_event().is_some() {}
            c.tick(Duration::ZERO);
            assert_eq!(
                c.snapshot().state,
                if paused {
                    PlaybackState::Paused
                } else {
                    PlaybackState::Playing
                }
            );
            assert_eq!(c.snapshot().queue.current, Some(updated));
            assert!(matches!(
                c.pop_event(),
                Some(PlayerEvent::MediaUpdated { .. })
            ));
            assert!(
                matches!(c.pop_event(),Some(PlayerEvent::Snapshot{state}) if state.queue.current.as_ref().unwrap().title=="Decoded title")
            );
        }
    }
    #[test]
    fn delayed_library_selection_cannot_revive_playback_after_stop() {
        struct LibraryFixture {
            pending: Option<LibraryUpdate>,
        }
        impl LibraryProvider for LibraryFixture {
            fn submit(&mut self, action: LibraryAction) -> Result<(), PlayerError> {
                if let LibraryAction::PreparePlayback { token, .. } = action {
                    self.pending = Some(LibraryUpdate::Playback {
                        token,
                        items: demo_tracks(),
                        start: 1,
                        album_id: None,
                    });
                }
                Ok(())
            }
            fn poll(&mut self) -> Option<LibraryUpdate> {
                self.pending.take()
            }
        }
        let mut c = coordinator();
        c.register_library(LibraryFixture { pending: None });
        let query = LibraryQuery {
            limit: 100,
            ..Default::default()
        };
        c.send(PlayerCommand::PlayLibrary {
            query: query.clone(),
            track_id: demo_tracks()[1].id.as_str().into(),
        })
        .unwrap();
        c.send(PlayerCommand::Stop).unwrap();
        c.tick(Duration::ZERO);
        assert_eq!(c.snapshot().state, PlaybackState::Stopped);
        assert_eq!(c.snapshot().queue.current.unwrap().title, "First Light");
        c.send(PlayerCommand::PlayLibrary {
            query,
            track_id: demo_tracks()[1].id.as_str().into(),
        })
        .unwrap();
        c.tick(Duration::ZERO);
        assert_eq!(c.snapshot().state, PlaybackState::Playing);
        assert_eq!(c.snapshot().queue.current.unwrap().title, "Soft Focus");
    }
}

#[cfg(test)]
mod transport_tests {
    use super::*;
    use provider_mock::{MockProvider, demo_tracks};
    use std::sync::{Arc, Mutex};
    #[derive(Default)]
    struct StoreState {
        updates: VecDeque<SessionUpdate>,
        actions: Vec<SessionAction>,
    }
    struct Store(Arc<Mutex<StoreState>>);
    impl SessionStore for Store {
        fn submit(&mut self, action: SessionAction) -> Result<(), PlayerError> {
            self.0.lock().unwrap().actions.push(action);
            Ok(())
        }
        fn poll(&mut self) -> Option<SessionUpdate> {
            self.0.lock().unwrap().updates.pop_front()
        }
    }
    fn setup() -> (
        PlaybackCoordinator,
        Arc<Mutex<StoreState>>,
        TransportSession,
    ) {
        let mut queue = QueueEngine::default();
        queue
            .replace(demo_tracks(), 0, PlaybackContext::UserQueue)
            .unwrap();
        queue.play_next(demo_tracks()[2].clone()).unwrap();
        queue.next(false);
        queue.next(false);
        queue.previous();
        queue.set_repeat(RepeatMode::All);
        queue.set_shuffle(true);
        let session = TransportSession {
            remote: None,
            version: 1,
            queue,
            position_ms: 3456,
            volume: 0.3,
            stopped: false,
            root_paths: vec![],
        };
        let shared = Arc::new(Mutex::new(StoreState::default()));
        let mut core = PlaybackCoordinator::default();
        core.register(MockProvider::default());
        core.register_session_store(Store(shared.clone()));
        core.send(PlayerCommand::InitializeSession {
            directory: "/unused".into(),
            access: SessionAccess::default(),
        })
        .unwrap();
        (core, shared, session)
    }
    fn loaded(
        core: &mut PlaybackCoordinator,
        shared: &Mutex<StoreState>,
        session: TransportSession,
    ) {
        shared
            .lock()
            .unwrap()
            .updates
            .push_back(SessionUpdate::Loaded {
                session: Some(Box::new(session)),
                issues: vec![],
            });
        core.tick(Duration::ZERO);
    }
    #[test]
    fn restored_transport_stays_paused_until_play_and_resumes_at_saved_position() {
        let (mut c, store, session) = setup();
        let current = session.queue.current().cloned();
        let upcoming = session.queue.snapshot().upcoming;
        loaded(&mut c, &store, session);
        c.tick(Duration::from_secs(100));
        assert_eq!(c.snapshot().state, PlaybackState::Paused);
        assert_eq!(c.position, 3456);
        assert!(!c.provider_loaded);
        assert_eq!(c.snapshot().queue.current, current);
        assert_eq!(c.snapshot().queue.upcoming, upcoming);
        c.send(PlayerCommand::Play).unwrap();
        assert_eq!(c.snapshot().state, PlaybackState::Playing);
        assert_eq!(c.position, 3456);
        c.send(PlayerCommand::Pause).unwrap();
        c.send(PlayerCommand::Next).unwrap();
        assert_eq!(c.state, PlaybackState::Paused);
        assert_eq!(c.position, 0);
        c.send(PlayerCommand::Previous).unwrap();
        assert_eq!(c.state, PlaybackState::Paused);
    }
    #[test]
    fn snapshot_separates_restored_selection_from_an_active_provider_session() {
        let (mut c, store, session) = setup();
        loaded(&mut c, &store, session);
        assert!(!c.snapshot().playback_session_active);
        c.send(PlayerCommand::Seek { position_ms: 1000 }).unwrap();
        assert!(!c.snapshot().playback_session_active);
        c.send(PlayerCommand::Play).unwrap();
        assert!(c.snapshot().playback_session_active);
        assert!(!c.snapshot().capabilities.system_media_session); // The silent source still cannot claim system playback.
        c.send(PlayerCommand::Pause).unwrap();
        assert!(c.snapshot().playback_session_active);
        c.send(PlayerCommand::Stop).unwrap();
        assert!(!c.snapshot().playback_session_active);
        c.send(PlayerCommand::ClearQueue).unwrap();
        assert!(!c.snapshot().playback_session_active);
    }
    #[test]
    fn late_disk_restore_cannot_replace_a_new_selection_or_clear() {
        for clear in [false, true] {
            let (mut c, store, session) = setup();
            if clear {
                c.send(PlayerCommand::ClearQueue).unwrap();
            } else {
                c.send(PlayerCommand::ReplaceQueue {
                    items: demo_tracks(),
                    start: 1,
                    context: PlaybackContext::UserQueue,
                })
                .unwrap();
            }
            loaded(&mut c, &store, session);
            assert_eq!(
                c.queue.current().map(|i| i.title.as_str()),
                if clear { None } else { Some("Soft Focus") }
            );
            assert!(!c.session_state.restored);
            assert!(
                store
                    .lock()
                    .unwrap()
                    .actions
                    .iter()
                    .any(|a| matches!(a, SessionAction::Save { .. }))
            );
        }
    }
    #[test]
    fn settings_changed_during_restore_merge_without_losing_saved_queue() {
        let (mut c, store, session) = setup();
        let current = session.queue.current().cloned();
        c.send(PlayerCommand::SetVolume { volume: 0.2 }).unwrap();
        c.send(PlayerCommand::SetShuffle { enabled: false })
            .unwrap();
        c.send(PlayerCommand::SetRepeat {
            mode: RepeatMode::One,
        })
        .unwrap();
        loaded(&mut c, &store, session);
        assert_eq!(c.queue.current().cloned(), current);
        assert_eq!(c.volume, 0.2);
        assert!(!c.snapshot().queue.shuffle);
        assert_eq!(c.snapshot().queue.repeat, RepeatMode::One);
    }
    #[test]
    fn queue_edits_are_revision_checked_and_previous_uses_restart_threshold() {
        let mut c = PlaybackCoordinator::default();
        c.register(MockProvider::default());
        c.send(PlayerCommand::ReplaceQueue {
            items: demo_tracks(),
            start: 0,
            context: PlaybackContext::UserQueue,
        })
        .unwrap();
        let revision = c.queue.revision();
        c.send(PlayerCommand::RemoveQueueItem { revision, index: 0 })
            .unwrap();
        assert!(
            c.send(PlayerCommand::JumpQueue { revision, index: 0 })
                .is_err()
        );
        let revision = c.queue.revision();
        c.send(PlayerCommand::JumpQueue { revision, index: 0 })
            .unwrap();
        assert_eq!(c.queue.current().unwrap().title, "A Place to Stay");
        c.tick(Duration::from_secs(4));
        assert_eq!(c.position, PREVIOUS_RESTART_MS);
        c.send(PlayerCommand::Previous).unwrap();
        assert_eq!(c.queue.current().unwrap().title, "A Place to Stay");
        assert_eq!(c.position, 0);
        c.tick(Duration::from_millis(PREVIOUS_RESTART_MS - 1));
        c.send(PlayerCommand::Previous).unwrap();
        assert_eq!(c.queue.current().unwrap().title, "First Light");
        c.send(PlayerCommand::Next).unwrap();
        c.tick(Duration::from_millis(PREVIOUS_RESTART_MS + 1));
        assert_eq!(c.queue.current().unwrap().title, "A Place to Stay");
        assert!(c.position > PREVIOUS_RESTART_MS);
        c.send(PlayerCommand::SkipPrevious).unwrap();
        assert_eq!(c.queue.current().unwrap().title, "First Light");
        assert_eq!(c.position, 0);
        c.send(PlayerCommand::ClearQueue).unwrap();
        assert!(c.queue.current().is_none());
        assert!(c.snapshot().queue.upcoming.is_empty());
    }
    #[test]
    fn stale_access_validation_cannot_replace_new_queue_and_missing_current_can_be_skipped() {
        let (mut c, store, session) = setup();
        let old = session.queue.clone();
        loaded(&mut c, &store, session);
        let id = c.queue.current().unwrap().id.clone();
        c.queue_issues = vec![QueueIssue {
            id,
            error: PlayerError::new(ErrorCode::PermissionLost, "fixture"),
        }];
        assert!(!c.snapshot().capabilities.play);
        assert!(c.send(PlayerCommand::Play).is_err());
        c.send(PlayerCommand::Next).unwrap();
        assert_eq!(c.state, PlaybackState::Paused);
        store
            .lock()
            .unwrap()
            .updates
            .push_back(SessionUpdate::Validated {
                revision: old.revision(),
                queue: Box::new(old),
                issues: vec![],
            });
        let expected = c.queue.current().cloned();
        c.tick(Duration::ZERO);
        assert_eq!(c.queue.current().cloned(), expected);
    }
    #[test]
    fn persistence_failure_is_visible_and_retried_without_stopping_transport() {
        let (mut c, store, session) = setup();
        loaded(&mut c, &store, session);
        c.send(PlayerCommand::Play).unwrap();
        store
            .lock()
            .unwrap()
            .updates
            .push_back(SessionUpdate::Error(PlayerError::new(
                ErrorCode::DatabaseError,
                "fixture lock",
            )));
        c.tick(Duration::from_secs(1));
        assert_eq!(c.state, PlaybackState::Playing);
        assert!(c.session_state.issue.is_some());
        let serial = c.session_serial;
        store
            .lock()
            .unwrap()
            .updates
            .push_back(SessionUpdate::Saved { serial });
        c.tick(Duration::ZERO);
        assert!(c.session_state.issue.is_none());
        assert!(!c.session_state.saving);
    }
    #[test]
    fn sound_debounces_and_user_edit_wins_initial_restore_without_starting_playback() {
        let (mut core, store, _) = setup();
        let mut settings = SoundSettings::default();
        settings.equalizer.enabled = true;
        settings.equalizer.bands_db[2] = 6.0;
        core.send(PlayerCommand::SetSound {
            settings: settings.clone(),
        })
        .unwrap();
        store
            .lock()
            .unwrap()
            .updates
            .push_back(SessionUpdate::SoundLoaded(Ok(SoundSettings::default())));
        core.tick(Duration::ZERO);
        assert_eq!(core.sound.settings, settings);
        assert_eq!(core.state, PlaybackState::Idle);
        assert!(store.lock().unwrap().actions.iter().any(
            |a| matches!(a, SessionAction::SaveSound { settings: saved, .. } if *saved == settings)
        ));
        store.lock().unwrap().actions.clear();
        for i in 0..20 {
            settings.equalizer.bands_db[0] = i as f32 / 2.0;
            core.send(PlayerCommand::SetSound {
                settings: settings.clone(),
            })
            .unwrap();
            core.tick(Duration::from_millis(10));
        }
        assert!(
            !store
                .lock()
                .unwrap()
                .actions
                .iter()
                .any(|a| matches!(a, SessionAction::SaveSound { .. }))
        );
        core.tick(Duration::from_millis(250));
        assert_eq!(
            store
                .lock()
                .unwrap()
                .actions
                .iter()
                .filter(|a| matches!(a, SessionAction::SaveSound { .. }))
                .count(),
            1
        );
        assert_eq!(core.snapshot().sound.settings, settings);
    }
    #[test]
    fn sound_retry_without_edits_finishes_and_old_save_cannot_hide_new_dirty_settings() {
        let (mut core, store, _) = setup();
        core.send(PlayerCommand::RetrySoundStorage).unwrap();
        assert!(core.sound.saving);
        store
            .lock()
            .unwrap()
            .updates
            .push_back(SessionUpdate::SoundLoaded(Ok(SoundSettings::default())));
        core.tick(Duration::ZERO);
        assert!(!core.sound.saving);
        let mut settings = SoundSettings::default();
        settings.equalizer.preamp_db = -2.0;
        core.send(PlayerCommand::SetSound {
            settings: settings.clone(),
        })
        .unwrap();
        core.tick(Duration::from_millis(250));
        let serial = core.sound_serial;
        settings.equalizer.preamp_db = -3.0;
        core.send(PlayerCommand::SetSound { settings }).unwrap();
        store
            .lock()
            .unwrap()
            .updates
            .push_back(SessionUpdate::SoundSaved {
                serial,
                result: Ok(()),
            });
        core.tick(Duration::ZERO);
        assert!(core.sound.saving && core.sound_dirty);
    }
    #[test]
    fn damaged_sound_storage_does_not_overwrite_data_and_retry_preserves_edits() {
        let (mut core, store, _) = setup();
        store
            .lock()
            .unwrap()
            .updates
            .push_back(SessionUpdate::SoundLoaded(Err(PlayerError::invalid(
                "damaged fixture",
            ))));
        core.tick(Duration::ZERO);
        let mut settings = SoundSettings::default();
        settings.equalizer.preamp_db = -3.0;
        core.send(PlayerCommand::SetSound {
            settings: settings.clone(),
        })
        .unwrap();
        core.tick(Duration::from_secs(2));
        assert!(
            !store
                .lock()
                .unwrap()
                .actions
                .iter()
                .any(|a| matches!(a, SessionAction::SaveSound { .. }))
        );
        core.send(PlayerCommand::RetrySoundStorage).unwrap();
        store
            .lock()
            .unwrap()
            .updates
            .push_back(SessionUpdate::SoundLoaded(Ok(SoundSettings::default())));
        core.tick(Duration::ZERO);
        assert_eq!(core.sound.settings, settings);
        assert!(core.sound.ready && core.sound.saving);
    }
}

#[cfg(test)]
mod analysis_tests {
    use super::*;
    use crate::test_provider::TestProvider;
    fn observation() -> AnalysisFrame {
        AnalysisFrame {
            generation: 1,
            sample_rate: 48000,
            window_frames: 4096,
            hop_frames: 960,
            window_start_frame: 0,
            window_end_frame: 4096,
            features: AudioFeatures {
                playback_position_ms: 85,
                rms: 0.2,
                ..Default::default()
            },
            spectrum: [0.1; SPECTRUM_BANDS],
            dropped_frames: 0,
        }
    }
    #[test]
    fn features_are_capability_gated_coalesced_and_clear_on_transport_changes() {
        let mut c = PlaybackCoordinator::default();
        c.register(TestProvider {
            analysis: Some(observation()),
            pcm: true,
            ..Default::default()
        });
        let mut tracks = provider_mock::demo_tracks();
        tracks[0].capabilities.pcm_analysis = true;
        c.send(PlayerCommand::ReplaceQueue {
            items: tracks.clone(),
            context: PlaybackContext::UserQueue,
            start: 0,
        })
        .unwrap();
        for _ in 0..200 {
            c.poll_analysis();
        }
        assert_eq!(
            c.events
                .iter()
                .filter(|e| matches!(e, PlayerEvent::AudioAnalysis { .. }))
                .count(),
            1
        );
        assert!(c.last_analysis.is_some());
        assert!(matches!(
            c.events.back(),
            Some(PlayerEvent::AudioAnalysis {
                modulation: Some(_),
                ..
            })
        ));
        c.send(PlayerCommand::Pause).unwrap();
        assert!(c.last_analysis.is_none());
        assert!(matches!(
            c.events.back(),
            Some(PlayerEvent::AudioAnalysis {
                frame: None,
                modulation: None,
                ..
            })
        ));
        c.send(PlayerCommand::Play).unwrap();
        assert!(c.last_analysis.is_some());
        c.send(PlayerCommand::Next).unwrap(); // Another item cannot inherit the capability from its provider.
        assert!(c.last_analysis.is_none());
        tracks[0].capabilities.pcm_analysis = true;
        let mut disabled = PlaybackCoordinator::default();
        disabled.register(TestProvider {
            analysis: Some(observation()),
            pcm: false,
            ..Default::default()
        });
        disabled
            .send(PlayerCommand::ReplaceQueue {
                items: tracks,
                context: PlaybackContext::UserQueue,
                start: 0,
            })
            .unwrap();
        disabled.poll_analysis();
        assert!(disabled.last_analysis.is_none());
    }

    #[test]
    fn remote_presence_clears_previously_accepted_analysis_and_modulation() {
        let mut c = PlaybackCoordinator::default();
        c.register(TestProvider {
            analysis: Some(observation()),
            pcm: true,
            ..Default::default()
        });
        let mut tracks = provider_mock::demo_tracks();
        tracks[0].capabilities.pcm_analysis = true;
        c.send(PlayerCommand::ReplaceQueue {
            items: tracks,
            start: 0,
            context: PlaybackContext::UserQueue,
        })
        .unwrap();
        assert!(matches!(
            c.events.back(),
            Some(PlayerEvent::AudioAnalysis {
                modulation: Some(_),
                ..
            })
        ));
        // A remote session has no analysis contract even while an old local provider is registered.
        c.remote.active = Some("remote_fixture".to_owned());
        c.poll_analysis();
        assert!(matches!(
            c.events.back(),
            Some(PlayerEvent::AudioAnalysis {
                media_id: None,
                frame: None,
                modulation: None
            })
        ));
        assert!(c.last_analysis.is_none());
    }
}

#[cfg(test)]
mod recovery_tests {
    use super::*;
    use crate::test_provider::TestProvider;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    #[test]
    fn device_retry_keeps_track_queue_and_presented_position() {
        let unavailable = Arc::new(AtomicBool::new(false));
        let mut core = PlaybackCoordinator::default();
        core.register(TestProvider {
            unavailable: unavailable.clone(),
            ..Default::default()
        });
        core.send(PlayerCommand::ReplaceQueue {
            items: provider_mock::demo_tracks(),
            start: 0,
            context: PlaybackContext::UserQueue,
        })
        .unwrap();
        core.tick(Duration::from_millis(4567));
        let before = core.snapshot();
        unavailable.store(true, Ordering::Relaxed);
        core.tick(Duration::ZERO);
        assert_eq!(core.state, PlaybackState::Error);
        assert!(core.send(PlayerCommand::RetryPlayback).is_err());
        unavailable.store(false, Ordering::Relaxed);
        core.send(PlayerCommand::RetryPlayback).unwrap();
        let after = core.snapshot();
        assert_eq!(after.position_ms, 4567);
        assert_eq!(after.queue.current, before.queue.current);
        assert_eq!(after.queue.upcoming, before.queue.upcoming);
        assert_eq!(after.state, PlaybackState::Playing);
        assert!(core.send(PlayerCommand::RetryPlayback).is_err());
    }
}
