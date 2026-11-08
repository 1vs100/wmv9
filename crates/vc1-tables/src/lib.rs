//! VC-1 and WMV3 variable length code tables and scan orders, ported from
//! FFmpeg's libavcodec by `tools/port_ffmpeg_tables.py`. This crate is licensed
//! LGPL-2.1-or-later like its source; keep it separate from the rest of the
//! workspace so that license stays contained.

mod ffmpeg;

pub use ffmpeg::*;
