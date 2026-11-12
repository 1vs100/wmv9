# Development

## Building

- The minimum Rust version is 1.88.
- Building needs [FFmpeg](https://ffmpeg.org/)'s development libraries (8.1) and clang, because `wmv3-encode` links FFmpeg to read media files.
- `cargo build --no-default-features` builds without FFmpeg. That build reads only raw YUV and WAV.
- The system FFmpeg is often a GPL build. A `wmv3-encode` linked with it falls under the GPL when you distribute it.

## Testing

- `cargo test --release` runs the tests.
- Tests that need the sample files in `testdata/` skip when it is absent.
- The differential and encoder tests need `ffmpeg` on `PATH`.
- `cargo bench --bench codecs` measures encoding and decoding speed. The benchmarks that use `testdata/` skip when it is absent.
- CI (`.github/workflows/ci.yml`) checks formatting, runs clippy and the tests without the `ffmpeg` feature, and checks the build on Rust 1.88.

## How it is checked

- Every decoded frame is compared with FFmpeg's output.
- Every encoded video file is decoded by FFmpeg and compared pixel for pixel.
- Encoded audio is decoded by FFmpeg and compared with the input.
- Encoded audio is also decoded by Microsoft's own decoder under [Wine](https://www.winehq.org/), which must give the same samples as FFmpeg.
- `MODLOG.md` records each milestone, the format facts we confirmed and the measurements behind them.

## Microsoft reference tools

- `tools/` holds the harnesses that run Microsoft's genuine DLLs under Wine as a reference. For example, `tools/wma_decode.c` decodes WMA through `WMADMOD.DLL`, and its header says how to build and run it.
- `docs/acquire-dlls.md` explains how to get those DLLs.
- `docs/vcm-oracle.md` describes the WMV3 video reference encoder.
- Microsoft binaries and decompiler output stay outside the repository, in `~/wmv9-decomp/`.
