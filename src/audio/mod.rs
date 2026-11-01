//! Sound and voice messages: decoding audio files (`decode`), playing them
//! and the app's sounds (`sound`), and transcripts (`voice`), made by
//! Whisper on this computer (`transcribe`). The sounds themselves are in
//! `assets/`.

#[cfg(any(feature = "transcribe", feature = "sound"))]
mod decode;
pub mod sound;
#[cfg(feature = "transcribe")]
pub mod transcribe;
pub mod voice;

#[cfg(any(feature = "transcribe", feature = "sound"))]
pub use decode::decode;
