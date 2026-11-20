//! Streaming file IO/decoding. This module is used only by the decode worker.
use player_domain::{ErrorCode, PlayerError};
use std::{fs::File, path::Path};
use symphonia::core::{
    codecs::audio::{AudioDecoder, AudioDecoderOptions},
    common::Limit,
    formats::{FormatOptions, FormatReader, SeekMode, SeekTo, TrackType, probe::Hint},
    io::MediaSourceStream,
    meta::{MetadataOptions, StandardTag, StandardVisualKey},
    units::{Time, TimeBase},
};

#[derive(Clone, Debug, PartialEq)]
pub struct AudioInfo {
    pub sample_rate: u32,
    pub channels: u16,
    pub total_frames: u64,
    pub duration_ms: u64,
    pub title: Option<String>,
    pub artists: Vec<String>,
    pub album: Option<String>,
    pub album_artist: Option<String>,
    pub track_number: Option<u32>,
    pub disc_number: Option<u32>,
    pub year: Option<u32>,
}
pub struct FileDecoder {
    format: Box<dyn FormatReader>,
    decoder: Box<dyn AudioDecoder>,
    track_id: u32,
    time_base: TimeBase,
    pub info: AudioInfo,
    buffer: Vec<f32>,
    offset: usize,
    skip_until: u64,
    packet_frame: u64,
    position: u64,
    eof: bool,
}
fn decode_error(error: impl std::fmt::Display) -> PlayerError {
    PlayerError::new(
        ErrorCode::DecodeError,
        format!("Could not decode audio: {error}"),
    )
}
fn unsupported(message: &str) -> PlayerError {
    PlayerError::new(ErrorCode::UnsupportedFormat, message)
}
impl FileDecoder {
    pub fn open(path: &Path) -> Result<Self, PlayerError> {
        let file = File::open(path).map_err(|error| {
            PlayerError::new(
                match error.kind() {
                    std::io::ErrorKind::NotFound => ErrorCode::FileMissing,
                    std::io::ErrorKind::PermissionDenied => ErrorCode::PermissionLost,
                    _ => ErrorCode::DecodeError,
                },
                format!("Could not open audio file: {error}"),
            )
        })?;
        if !file.metadata().map_err(decode_error)?.is_file() {
            return Err(unsupported("Select a regular audio file"));
        }
        let mut hint = Hint::new();
        if let Some(extension) = path.extension().and_then(|s| s.to_str()) {
            hint.with_extension(extension);
        }
        let stream = MediaSourceStream::new(Box::new(file), Default::default());
        let mut format = symphonia::default::get_probe()
            .probe(
                &hint,
                stream,
                FormatOptions::default(),
                MetadataOptions::default()
                    .limit_tag_bytes(Limit::Maximum(64 * 1024))
                    .limit_visual_bytes(Limit::Maximum(16 * 1024 * 1024)),
            )
            .map_err(decode_error)?;
        let track = format
            .default_track(TrackType::Audio)
            .ok_or_else(|| unsupported("No audio track found"))?;
        let parameters = track
            .codec_params
            .as_ref()
            .and_then(|p| p.audio())
            .ok_or_else(|| unsupported("Missing audio codec parameters"))?;
        let rate = parameters
            .sample_rate
            .filter(|rate| (8000..=384000).contains(rate))
            .ok_or_else(|| unsupported("Unsupported sample rate"))?;
        let channels = parameters
            .channels
            .as_ref()
            .map(|c| c.count())
            .filter(|count| (1..=2).contains(count))
            .ok_or_else(|| unsupported("M2 supports mono and stereo files"))?;
        let time_base = track
            .time_base
            .unwrap_or_else(|| TimeBase::try_from_recip(rate).unwrap());
        let track_id = track.id;
        let declared = track.num_frames.or_else(|| {
            track.duration.map(|d| {
                ((u128::from(d.get()) * u128::from(time_base.numer.get()) * u128::from(rate))
                    / u128::from(time_base.denom.get())) as u64
            })
        });
        let decoder = symphonia::default::get_codecs()
            .make_audio_decoder(parameters, &AudioDecoderOptions::default())
            .map_err(decode_error)?;
        let total = if let Some(total) = declared.filter(|v| *v > 0) {
            total
        } else {
            // Some MP3/OGG files lack a duration/index. Probe packets on this IO worker only.
            let mut end = 0u64;
            while let Some(packet) = format.next_packet().map_err(decode_error)? {
                if packet.track_id == track_id {
                    let ticks = (packet.pts.get().max(0) as u64).saturating_add(packet.dur.get());
                    end = end.max(
                        ((u128::from(ticks) * u128::from(time_base.numer.get()) * u128::from(rate))
                            / u128::from(time_base.denom.get())) as u64,
                    );
                }
            }
            if end == 0 {
                return Err(unsupported("Missing or empty audio duration"));
            }
            format
                .seek(
                    SeekMode::Accurate,
                    SeekTo::Time {
                        time: Time::try_from_secs_f64(0.0).unwrap(),
                        track_id: Some(track_id),
                    },
                )
                .map_err(decode_error)?;
            end
        };
        let mut info = AudioInfo {
            sample_rate: rate,
            channels: channels as u16,
            total_frames: total,
            duration_ms: ((total as u128 * 1000) / u128::from(rate)).min(u128::from(u64::MAX))
                as u64,
            title: None,
            artists: vec![],
            album: None,
            album_artist: None,
            track_number: None,
            disc_number: None,
            year: None,
        };
        if let Some(metadata) = format.metadata().current() {
            for tag in &metadata.media.tags {
                match &tag.std {
                    Some(StandardTag::TrackTitle(value)) => {
                        info.title = Some(value.chars().take(256).collect())
                    }
                    Some(StandardTag::Artist(value)) if info.artists.len() < 8 => {
                        info.artists.push(value.chars().take(128).collect())
                    }
                    Some(StandardTag::Album(value)) => {
                        info.album = Some(value.chars().take(256).collect())
                    }
                    Some(StandardTag::AlbumArtist(value)) => {
                        info.album_artist = Some(value.chars().take(128).collect())
                    }
                    Some(StandardTag::TrackNumber(value)) => {
                        info.track_number = u32::try_from(*value).ok()
                    }
                    Some(StandardTag::DiscNumber(value)) => {
                        info.disc_number = u32::try_from(*value).ok()
                    }
                    Some(
                        StandardTag::ReleaseDate(value)
                        | StandardTag::RecordingDate(value)
                        | StandardTag::OriginalReleaseDate(value),
                    ) if info.year.is_none() => {
                        info.year = value.get(..4).and_then(|s| s.parse().ok());
                    }
                    _ => {}
                }
            }
        }
        Ok(Self {
            format,
            decoder,
            track_id,
            time_base,
            info,
            buffer: vec![],
            offset: 0,
            skip_until: 0,
            packet_frame: 0,
            position: 0,
            eof: false,
        })
    }
    /// Copies a bounded embedded cover only when a library worker requests it.
    pub fn embedded_artwork(&mut self) -> Option<Vec<u8>> {
        let metadata = self.format.metadata();
        let visuals = &metadata.current()?.media.visuals;
        let supported = |v: &&symphonia::core::meta::Visual| {
            v.data.len() <= 16 * 1024 * 1024
                && (v.data.starts_with(b"\x89PNG\r\n\x1a\n")
                    || v.data.starts_with(&[0xff, 0xd8, 0xff]))
        };
        visuals
            .iter()
            .filter(supported)
            .find(|v| v.usage == Some(StandardVisualKey::FrontCover))
            .or_else(|| visuals.iter().find(supported))
            .map(|v| v.data.to_vec())
    }
    pub fn seek(&mut self, frame: u64) -> Result<(), PlayerError> {
        let frame = frame.min(self.info.total_frames);
        self.buffer.clear();
        self.offset = 0;
        self.skip_until = frame;
        self.position = frame;
        if frame == self.info.total_frames {
            self.eof = true;
            return Ok(());
        }
        let time = Time::try_from_secs_f64(frame as f64 / f64::from(self.info.sample_rate))
            .ok_or_else(|| unsupported("Seek position is too large"))?;
        self.format
            .seek(
                SeekMode::Accurate,
                SeekTo::Time {
                    time,
                    track_id: Some(self.track_id),
                },
            )
            .map_err(decode_error)?;
        self.decoder.reset();
        self.eof = false;
        Ok(())
    }
    pub fn read_frames(&mut self, count: usize) -> Result<Vec<[f32; 2]>, PlayerError> {
        let mut frames = Vec::with_capacity(count);
        while frames.len() < count {
            if self.offset == self.buffer.len() {
                if self.eof {
                    break;
                }
                let Some(packet) = self.format.next_packet().map_err(decode_error)? else {
                    self.eof = true;
                    if self.position < self.info.total_frames {
                        return Err(decode_error(
                            "Audio data ended before its declared duration",
                        ));
                    }
                    break;
                };
                if packet.track_id != self.track_id {
                    continue;
                }
                self.packet_frame = ((packet.pts.get().max(0) as u128
                    * u128::from(self.time_base.numer.get())
                    * u128::from(self.info.sample_rate))
                    / u128::from(self.time_base.denom.get()))
                .min(u128::from(u64::MAX)) as u64;
                let audio = self.decoder.decode(&packet).map_err(decode_error)?;
                if audio.spec().rate() != self.info.sample_rate
                    || audio.spec().channels().count() != usize::from(self.info.channels)
                {
                    return Err(unsupported("Changing audio formats are not supported"));
                }
                if audio.frames() > 65536 {
                    return Err(unsupported("Audio packet is too large"));
                }
                self.buffer.resize(audio.samples_interleaved(), 0.0);
                audio.copy_to_slice_interleaved(&mut self.buffer);
                self.offset = 0;
                let skip = self
                    .skip_until
                    .saturating_sub(self.packet_frame)
                    .min(audio.frames() as u64) as usize;
                self.offset = skip * usize::from(self.info.channels);
                self.packet_frame += skip as u64;
                if self.offset == self.buffer.len() {
                    continue;
                }
            }
            let left = self.buffer[self.offset];
            let right = if self.info.channels == 2 {
                self.buffer[self.offset + 1]
            } else {
                left
            };
            if !left.is_finite() || !right.is_finite() {
                return Err(decode_error("Non-finite PCM sample"));
            }
            frames.push([left, right]);
            self.offset += usize::from(self.info.channels);
            self.packet_frame += 1;
            self.position = self.packet_frame;
            if self.position >= self.info.total_frames {
                self.eof = true;
                self.buffer.clear();
                self.offset = 0;
            }
        }
        Ok(frames)
    }
}
