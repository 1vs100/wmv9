//! WMA code tables and band layouts, ported from FFmpeg's libavcodec by
//! `tools/port_ffmpeg_tables.py`: versions 1 and 2 at the root, WMA Pro
//! (version 3) in `pro`. This crate is licensed LGPL-2.1-or-later like its
//! source; keep it separate from the rest of the workspace so that license
//! stays contained.

mod ffmpeg;
pub mod pro;

pub use ffmpeg::*;
