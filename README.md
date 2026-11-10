# wmv9

[![CI](https://github.com/1vs100/wmv9/actions/workflows/ci.yml/badge.svg)](https://github.com/1vs100/wmv9/actions/workflows/ci.yml)
[![License: GPL-3.0-or-later](https://img.shields.io/badge/license-GPL--3.0--or--later-blue.svg)](LICENSE)
[![Rust 1.88+](https://img.shields.io/badge/rust-1.88%2B-orange.svg)](https://www.rust-lang.org)
![ai](https://img.shields.io/badge/ai-assisted-orange?logo=micro%3Abit&logoColor=white&label=ai)

- Encode video and audio to `.wmv` files on Linux: WMV3 (Windows Media Video 9, Simple/Main profile) video and WMA 9 Standard or WMA 10 Professional audio.
- The files follow the layout of files written by Microsoft's Windows Media Format SDK 11, so software that only accepts those can read them.

## Usage

Give any video or audio file that [FFmpeg](https://ffmpeg.org/) reads:

```sh
cargo run --release --bin wmv3-encode -- -i in.mp4 --bitrate 800 -o out.wmv
```

- The first video and audio stream are used. The video keeps its size and frame rate unless you give `--scale WxH` or `--fps N/D`. The audio becomes stereo or mono at a WMA sample rate, and stays in step with the video.
- `--no-audio` or `--no-video` leaves a stream out. A file without video is audio only, so name it `.wma`.
- `--audio FILE` takes the sound from another file. That sound starts with the video.
- `--quality` sets the quantizer from 1 (best) to 31 (smallest). The default is 6.
- `--bitrate` sets a video bit rate in kbit/s instead. The encoder then picks the quantizer of each frame so a player that buffers for the 3 s preroll does not run out of data. The rate counts the video data. The file adds a few percent of ASF overhead. If the rate is too low even at the coarsest quantizer, the encoder warns.
- `--key-interval` sets the distance between key frames in frames. The default is 300. Scene cuts also start a key frame.
- `--audio-bitrate` sets the audio bit rate in kbit/s. The default is 128.
- `--audio-codec pro` writes WMA 10 Professional, the audio of Microsoft's own sample files, instead of WMA 9 Standard. It sounds better at the same rate.

You can also pipe raw YUV 4:2:0 frames in with `--size`:

```sh
ffmpeg -i in.mp4 -f rawvideo -pix_fmt yuv420p - |
    wmv3-encode --size 1280x720 --fps 30000/1001 --audio in.mp4 -o out.wmv
```

The video encoder uses I and P frames. Expect larger files than Microsoft's encoder gives at the same quality. The audio is constant bit rate.

## License

[GPL-3.0-or-later](LICENSE). The code tables in `crates/` are ported from FFmpeg and stay under FFmpeg's LGPL-2.1-or-later.

## Building

You need FFmpeg's development libraries (8.1) and clang. See [DEVELOPMENT.md](DEVELOPMENT.md) for building without FFmpeg, running the tests and how the output is checked.
