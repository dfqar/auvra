//! Provider-independent vocabulary. No platform, HTTP, audio or GPU dependencies.
pub mod capabilities;
pub mod error;
pub mod media;
pub mod playback;
pub mod provider;
pub use capabilities::*;
pub use error::*;
pub use media::*;
pub use playback::*;
pub use provider::*;
mod library;
pub use library::*;
mod remote;
pub use remote::*;

mod analysis;
pub use analysis::*;
mod sound;
pub use sound::*;

mod host_analysis;
pub use host_analysis::*;
