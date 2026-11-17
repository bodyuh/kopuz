//! Audio playback engine for Kopuz: player state machine, decoder, equalizer,
//! system media controls, and audio output via cpal / symphonia.

pub mod decoder;
pub mod engine;
pub mod eq;
#[cfg(windows)]
pub mod loopback;
pub mod player;
pub(crate) mod replaygain;
pub mod systemint;
