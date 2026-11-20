//! Single-file local catalog and asynchronous playback, independent of the native UI.
use audio_engine::{AudioEngine, AudioSnapshot};
use player_domain::*;
use std::{path::Path, time::Duration};

#[derive(Default)]
pub struct LocalCatalog;
impl CatalogProvider for LocalCatalog {
    fn id(&self) -> &str {
        "local"
    }
    fn search(&self, _query: &str) -> Result<Vec<MediaItem>, PlayerError> {
        Err(PlayerError::new(
            ErrorCode::UnsupportedCapability,
            "Query the asynchronous library service",
        ))
    }
    fn get_track(&self, _id: &MediaId) -> Result<MediaItem, PlayerError> {
        Err(PlayerError::new(
            ErrorCode::TrackUnavailable,
            "Open a local file first",
        ))
    }
    fn resolve_file(&self, path: &str) -> Result<MediaItem, PlayerError> {
        let file = Path::new(path);
        if !file.is_absolute() || path.len() > 3500 || path.contains('\0') {
            return Err(PlayerError::invalid("Select an absolute local file path"));
        }
        if !file
            .extension()
            .and_then(|s| s.to_str())
            .is_some_and(player_library::is_supported_extension)
        {
            return Err(PlayerError::new(
                ErrorCode::UnsupportedFormat,
                "Choose a supported local audio file",
            ));
        }
        let title = file
            .file_stem()
            .and_then(|s| s.to_str())
            .filter(|s| !s.is_empty())
            .unwrap_or("Local track")
            .to_owned();
        let id = MediaId::parse(format!(
            "local:track:{}",
            blake3::hash(path.as_bytes()).to_hex()
        ))?;
        let item = MediaItem {
            id,
            title,
            artists: vec![],
            album: None,
            duration_ms: 0,
            artwork: None,
            location: Some(MediaLocation::File { path: path.into() }),
            capabilities: Capabilities::LOCAL,
        };
        item.validate()?;
        Ok(item)
    }
}
#[derive(Default)]
pub struct LocalPlaybackProvider {
    engine: Option<AudioEngine>,
    current: Option<MediaItem>,
    update: Option<MediaItem>,
    last: Option<ProviderPlayback>,
    ended_generation: u64,
    volume: Option<f32>,
    equalizer: EqualizerSettings,
}
impl LocalPlaybackProvider {
    fn snapshot(&mut self) -> Result<ProviderPlayback, PlayerError> {
        let engine = self
            .engine
            .as_ref()
            .ok_or_else(|| PlayerError::invalid("No local file is open"))?;
        let snapshot = engine.snapshot();
        if let Some(error) = snapshot.error.clone() {
            return Err(error);
        }
        self.update_metadata(&snapshot);
        let ended = snapshot.state == PlaybackState::Stopped
            && snapshot.position_ms > 0
            && snapshot.generation != self.ended_generation;
        if ended {
            self.ended_generation = snapshot.generation;
        }
        Ok(ProviderPlayback {
            state: snapshot.state,
            position_ms: snapshot.position_ms,
            ended,
        })
    }
    fn update_metadata(&mut self, snapshot: &AudioSnapshot) {
        if let (Some(info), Some(current)) = (&snapshot.info, &self.current) {
            let mut item = current.clone();
            item.duration_ms = info.duration_ms;
            if let Some(title) = &info.title
                && !title.trim().is_empty()
            {
                item.title = title.clone();
            }
            item.artists = info.artists.clone();
            item.album = info.album.clone();
            if item.validate().is_err() {
                item = current.clone();
                item.duration_ms = info.duration_ms;
            }
            if &item != current {
                self.current = Some(item.clone());
                self.update = Some(item);
            }
        }
    }
}
impl PlaybackProvider for LocalPlaybackProvider {
    fn id(&self) -> &str {
        "local"
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities::LOCAL
    }
    fn connection(&self) -> ProviderConnectionState {
        ProviderConnectionState::Connected
    }
    fn validate(&self, item: &MediaItem) -> Result<(), PlayerError> {
        item.validate()?;
        let Some(MediaLocation::File { path }) = &item.location else {
            return Err(PlayerError::invalid(
                "A local track requires a file locator",
            ));
        };
        if item.id.provider() != "local" {
            return Err(PlayerError::invalid("Wrong provider namespace"));
        }
        LocalCatalog.resolve_file(path)?;
        Ok(())
    }
    fn load(&mut self, item: &MediaItem) -> Result<ProviderPlayback, PlayerError> {
        self.load_at(item, 0, true)
    }
    fn load_at(
        &mut self,
        item: &MediaItem,
        position_ms: u64,
        autoplay: bool,
    ) -> Result<ProviderPlayback, PlayerError> {
        self.validate(item)?;
        if self.engine.as_ref().is_none_or(|engine| !engine.is_alive()) {
            self.engine = Some(AudioEngine::new()?);
        }
        let engine = self.engine.as_ref().unwrap();
        engine.set_volume(self.volume.unwrap_or(0.7))?;
        engine.set_equalizer(self.equalizer)?;
        let Some(MediaLocation::File { path }) = &item.location else {
            unreachable!()
        };
        engine.load_at(path.into(), position_ms, autoplay);
        self.current = Some(item.clone());
        self.update = None;
        self.last = None;
        self.ended_generation = 0;
        Ok(ProviderPlayback {
            state: PlaybackState::Loading,
            position_ms,
            ended: false,
        })
    }
    fn pause(&mut self) -> Result<ProviderPlayback, PlayerError> {
        self.engine
            .as_ref()
            .ok_or_else(|| PlayerError::invalid("No local file is open"))?
            .pause();
        self.snapshot()
    }
    fn resume(&mut self) -> Result<ProviderPlayback, PlayerError> {
        self.engine
            .as_ref()
            .ok_or_else(|| PlayerError::invalid("No local file is open"))?
            .resume();
        self.snapshot()
    }
    fn stop(&mut self) -> Result<(), PlayerError> {
        if let Some(engine) = &self.engine {
            engine.stop();
        }
        self.last = Some(ProviderPlayback {
            state: PlaybackState::Stopped,
            position_ms: 0,
            ended: false,
        });
        Ok(())
    }
    fn seek(&mut self, position_ms: u64) -> Result<ProviderPlayback, PlayerError> {
        let duration = self
            .current
            .as_ref()
            .ok_or_else(|| PlayerError::invalid("No local file is open"))?
            .duration_ms;
        self.engine
            .as_ref()
            .ok_or_else(|| PlayerError::invalid("No local file is open"))?
            .seek(position_ms.min(duration));
        self.snapshot()
    }
    fn set_volume(&mut self, volume: f32) -> Result<(), PlayerError> {
        if !volume.is_finite() || !(0.0..=1.0).contains(&volume) {
            return Err(PlayerError::invalid("Volume must be between 0 and 1"));
        }
        self.volume = Some(volume);
        if let Some(engine) = &self.engine {
            engine.set_volume(volume)?;
        }
        Ok(())
    }
    fn tick(&mut self, _elapsed: Duration) -> Result<Option<ProviderPlayback>, PlayerError> {
        if self.engine.is_none() {
            return Ok(None);
        }
        let snapshot = self.snapshot()?;
        if self.last == Some(snapshot) && !snapshot.ended {
            return Ok(None);
        }
        self.last = Some(snapshot);
        Ok(Some(snapshot))
    }
    fn set_equalizer(&mut self, settings: EqualizerSettings) -> Result<(), PlayerError> {
        settings.validate()?;
        if let Some(engine) = &self.engine {
            engine.set_equalizer(settings)?;
        }
        self.equalizer = settings;
        Ok(())
    }
    fn output_statistics(&self) -> Option<OutputStatistics> {
        self.engine.as_ref().map(AudioEngine::output_statistics)
    }
    fn analysis(&self) -> Option<AnalysisFrame> {
        self.engine.as_ref().and_then(|engine| engine.analysis())
    }
    fn take_media_update(&mut self) -> Option<MediaItem> {
        self.update.take()
    }
}
