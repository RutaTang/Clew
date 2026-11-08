//! Voice for Clew, on this machine and on every platform Clew runs on:
//! the microphone ([`capture`]), its audio made ready for a speech model
//! ([`audio`]), whisper.cpp's offline speech recognition ([`whisper`]) on a
//! model from its own published list, fetched once and verified
//! ([`models`]), and what was said read as one of the window's commands
//! when it plainly is one ([`intent`]) — and answers said aloud: an
//! on-device voice model, fetched as verified ([`speech`]), run by the
//! `clew-speak` program beside Clew ([`speaker`]), the answer made ready to
//! be said ([`say`]) and played on the speaker ([`playback`]).
//!
//! Nothing here is tied to a platform or to the rest of Clew: it builds and
//! is tested on Linux, macOS and Windows alike.

pub mod audio;
pub mod capture;
pub mod intent;
pub mod models;
pub mod playback;
pub mod say;
pub mod speaker;
pub mod speech;
pub mod turn;
#[cfg(test)]
mod wav;
pub mod whisper;
