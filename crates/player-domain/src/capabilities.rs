use serde::{Deserialize, Serialize};
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    pub play: bool,
    pub pause: bool,
    pub seek: bool,
    pub volume: bool,
    pub pcm_analysis: bool,
    pub system_media_session: bool,
    #[serde(default = "legacy_equalizer")]
    pub equalizer: bool,
    #[serde(default)]
    pub library_save: bool,
}
// Older saved media did not restrict DSP. The provider still has to grant it.
fn legacy_equalizer() -> bool {
    true
}
impl Capabilities {
    /// A track may restrict its provider; it can never grant extra provider abilities.
    pub fn intersection(self, item: Self) -> Self {
        Self {
            play: self.play && item.play,
            pause: self.pause && item.pause,
            seek: self.seek && item.seek,
            volume: self.volume && item.volume,
            pcm_analysis: self.pcm_analysis && item.pcm_analysis,
            system_media_session: self.system_media_session && item.system_media_session,
            equalizer: self.equalizer && item.equalizer,
            library_save: self.library_save && item.library_save,
        }
    }

    pub const LOCAL: Self = Self {
        play: true,
        pause: true,
        seek: true,
        volume: true,
        pcm_analysis: true,
        system_media_session: true,
        equalizer: true,
        library_save: false,
    };
    pub const REMOTE: Self = Self {
        library_save: true,
        equalizer: false,
        pcm_analysis: false,
        system_media_session: false,
        ..Self::LOCAL
    };
    pub const DEMO: Self = Self {
        equalizer: false,
        pcm_analysis: false,
        system_media_session: false,
        ..Self::LOCAL
    };
}
