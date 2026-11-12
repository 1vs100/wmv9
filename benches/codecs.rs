//! Encoding and decoding speed of each codec.
//!
//! usage: cargo bench --bench codecs

mod common;

use std::hint::black_box;
use std::path::Path;

use criterion::{Criterion, criterion_group, criterion_main};
use wmv9::wma::Channels;

fn audio(c: &mut Criterion) {
    let mono = common::audio(Channels::Mono);
    let stereo = common::audio(Channels::Stereo);
    let mut g = c.benchmark_group("audio 5 s");
    g.sample_size(10);
    g.bench_function("wmapro stereo 128k", |b| {
        b.iter(|| common::wmapro(common::format(Channels::Stereo, 16000), black_box(&stereo)))
    });
    g.bench_function("wmapro mono 64k", |b| {
        b.iter(|| common::wmapro(common::format(Channels::Mono, 8000), black_box(&mono)))
    });
    g.bench_function("wmapro xma stereo 128k", |b| {
        b.iter(|| common::wmapro_xma(common::format(Channels::Stereo, 16000), black_box(&stereo)))
    });
    g.bench_function("wma2 stereo 128k", |b| {
        b.iter(|| common::wma2(common::format(Channels::Stereo, 16000), black_box(&stereo)))
    });
    g.finish();
}

fn video(c: &mut Criterion) {
    let frames = common::video();
    let mut g = c.benchmark_group("video");
    g.sample_size(10);
    g.bench_function("wmv3 encode 320x240 30 frames", |b| {
        b.iter(|| common::wmv3_encode(black_box(&frames)))
    });
    if let Some(stream) = common::read_video(Path::new(common::SAMPLE_FILE)) {
        let pictures = common::sample_pictures(&stream, common::SAMPLE_PICTURES);
        g.bench_function("wmv3 encode sample 60 frames", |b| {
            b.iter(|| common::wmv3_encode(black_box(&pictures)))
        });
        g.bench_function("wmv3 decode sample", |b| {
            b.iter(|| common::wmv3_decode(black_box(&stream)))
        });
    }
    g.finish();
}

criterion_group!(benches, audio, video);
criterion_main!(benches);
