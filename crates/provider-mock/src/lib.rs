//! Deterministic silent provider for M0/M1. Never advertises PCM or system audio.
use player_domain::*;
use std::time::Duration;
#[derive(Default)]
pub struct MockProvider {
    item: Option<MediaItem>,
    position: u64,
    fractional_time: Duration,
    state: PlaybackState,
}
impl MockProvider {
    fn snapshot(&self, ended: bool) -> ProviderPlayback {
        ProviderPlayback {
            state: self.state,
            position_ms: self.position,
            ended,
        }
    }
    fn require_item(&self) -> Result<(), PlayerError> {
        self.item
            .as_ref()
            .map(|_| ())
            .ok_or_else(|| PlayerError::invalid("No track selected"))
    }
}
impl PlaybackProvider for MockProvider {
    fn id(&self) -> &str {
        "mock"
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities::DEMO
    }
    fn connection(&self) -> ProviderConnectionState {
        ProviderConnectionState::Connected
    }
    fn load(&mut self, item: &MediaItem) -> Result<ProviderPlayback, PlayerError> {
        if item.id.provider() != self.id() {
            return Err(PlayerError::invalid("Wrong provider namespace"));
        }
        self.item = Some(item.clone());
        self.position = 0;
        self.fractional_time = Duration::ZERO;
        self.state = PlaybackState::Playing;
        Ok(self.snapshot(false))
    }
    fn pause(&mut self) -> Result<ProviderPlayback, PlayerError> {
        self.require_item()?;
        self.state = PlaybackState::Paused;
        Ok(self.snapshot(false))
    }
    fn load_at(
        &mut self,
        item: &MediaItem,
        position_ms: u64,
        autoplay: bool,
    ) -> Result<ProviderPlayback, PlayerError> {
        self.load(item)?;
        self.seek(position_ms)?;
        if !autoplay {
            self.pause()?;
        }
        Ok(self.snapshot(false))
    }
    fn resume(&mut self) -> Result<ProviderPlayback, PlayerError> {
        self.require_item()?;
        self.state = PlaybackState::Playing;
        Ok(self.snapshot(false))
    }
    fn stop(&mut self) -> Result<(), PlayerError> {
        self.state = PlaybackState::Stopped;
        self.position = 0;
        Ok(())
    }
    fn seek(&mut self, position_ms: u64) -> Result<ProviderPlayback, PlayerError> {
        self.require_item()?;
        self.position = position_ms.min(self.item.as_ref().unwrap().duration_ms);
        self.fractional_time = Duration::ZERO;
        Ok(self.snapshot(false))
    }
    fn set_volume(&mut self, _volume: f32) -> Result<(), PlayerError> {
        Ok(())
    }
    fn tick(&mut self, elapsed: Duration) -> Result<Option<ProviderPlayback>, PlayerError> {
        if self.state != PlaybackState::Playing {
            return Ok(None);
        }
        let duration = self.item.as_ref().unwrap().duration_ms;
        let elapsed = elapsed.saturating_add(self.fractional_time);
        let whole_ms = elapsed.as_millis().min(u64::MAX as u128) as u64;
        self.fractional_time = elapsed.saturating_sub(Duration::from_millis(whole_ms));
        self.position = self.position.saturating_add(whole_ms).min(duration);
        let ended = self.position >= duration;
        if ended {
            self.state = PlaybackState::Stopped;
        }
        Ok(Some(self.snapshot(ended)))
    }
}
pub fn demo_tracks() -> Vec<MediaItem> {
    [
        ("01", "First Light", "Auvra Sessions", 213000),
        ("02", "Soft Focus", "Auvra Sessions", 248000),
        ("03", "A Place to Stay", "Auvra Sessions", 196000),
    ]
    .into_iter()
    .map(|(id, title, artist, duration_ms)| MediaItem {
        id: MediaId::parse(format!("mock:track:{id}")).unwrap(),
        title: title.into(),
        artists: vec![artist.into()],
        album: Some("Quiet Hours".into()),
        duration_ms,
        artwork: None,
        location: None,
        capabilities: Capabilities::DEMO,
    })
    .collect()
}
