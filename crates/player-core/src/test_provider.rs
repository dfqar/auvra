use player_domain::*;
use provider_mock::MockProvider;
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

#[derive(Default)]
pub(crate) struct TestProvider {
    pub inner: MockProvider,
    pub analysis: Option<AnalysisFrame>,
    pub pcm: bool,
    pub stops: Arc<AtomicUsize>,
    pub unavailable: Arc<AtomicBool>,
    pub panic_on_load: bool,
    pub deferred_load: bool,
    pub media_update: Option<MediaItem>,
}
impl PlaybackProvider for TestProvider {
    fn id(&self) -> &str {
        "mock"
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            pcm_analysis: self.pcm,
            ..Capabilities::DEMO
        }
    }
    fn connection(&self) -> ProviderConnectionState {
        if self.unavailable.load(Ordering::Relaxed) {
            ProviderConnectionState::Unavailable
        } else {
            ProviderConnectionState::Connected
        }
    }
    fn load(&mut self, item: &MediaItem) -> Result<ProviderPlayback, PlayerError> {
        assert!(!self.panic_on_load, "test provider panic");
        if item.title == "Cannot Load" {
            return Err(PlayerError::new(
                ErrorCode::DecodeError,
                "Fixture cannot load",
            ));
        }
        let mut loaded = self.inner.load(item)?;
        if self.deferred_load {
            loaded.state = PlaybackState::Loading;
        }
        Ok(loaded)
    }
    fn pause(&mut self) -> Result<ProviderPlayback, PlayerError> {
        self.inner.pause()
    }
    fn load_at(
        &mut self,
        item: &MediaItem,
        position_ms: u64,
        autoplay: bool,
    ) -> Result<ProviderPlayback, PlayerError> {
        if position_ms == 0 && autoplay {
            self.load(item)
        } else {
            self.load(item)?;
            self.inner.load_at(item, position_ms, autoplay)
        }
    }
    fn resume(&mut self) -> Result<ProviderPlayback, PlayerError> {
        self.inner.resume()
    }
    fn stop(&mut self) -> Result<(), PlayerError> {
        self.stops.fetch_add(1, Ordering::Relaxed);
        self.inner.stop()
    }
    fn seek(&mut self, position_ms: u64) -> Result<ProviderPlayback, PlayerError> {
        self.inner.seek(position_ms)
    }
    fn set_volume(&mut self, volume: f32) -> Result<(), PlayerError> {
        self.inner.set_volume(volume)
    }
    fn analysis(&self) -> Option<AnalysisFrame> {
        self.analysis
    }
    fn take_media_update(&mut self) -> Option<MediaItem> {
        self.media_update.take()
    }
    fn tick(&mut self, elapsed: Duration) -> Result<Option<ProviderPlayback>, PlayerError> {
        if self.unavailable.load(Ordering::Relaxed) {
            Err(PlayerError::new(
                ErrorCode::AudioDeviceLost,
                "Fixture device disconnected",
            ))
        } else {
            self.inner.tick(elapsed)
        }
    }
}
