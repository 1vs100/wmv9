//! WMA encoding: WMA 9 Standard (WMA version 2) in `encode`, WMA 10
//! Professional (version 3) in `pro`, and whole tracks of either through
//! `encode_track`.

pub mod encode;
mod mdct;
mod params;
pub mod pro;
mod track;

pub use params::{ByteRate, Channels, Format, FormatError, SampleRate};
pub use track::{Codec, TrackError, encode_track};
