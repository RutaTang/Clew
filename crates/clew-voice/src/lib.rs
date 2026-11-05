//! Voice for Clew, on this machine and on every platform Clew runs on:
//! the microphone ([`capture`]), its audio made ready for a speech model
//! ([`audio`]), whisper.cpp's offline speech recognition ([`whisper`]) on a
//! model from its own published list, fetched once and verified
//! ([`models`]), and what was said read as one of the window's commands
//! when it plainly is one ([`intent`]).
//!
//! Nothing here is tied to a platform or to the rest of Clew: it builds and
//! is tested on Linux, macOS and Windows alike.

pub mod audio;
pub mod capture;
pub mod intent;
pub mod models;
#[cfg(test)]
mod wav;
pub mod whisper;
