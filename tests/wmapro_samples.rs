//! The WMA Pro streams of the SDK 11 samples read into the frame model and
//! write back bit for bit.

use std::path::Path;

use wmv9::asf::{Asf, TypeSpecificData};
use wmv9::wma::pro::config::{Config, Header};
use wmv9::wma::pro::packets::{join, layout, split};
use wmv9::wma::pro::syntax::Codec;

/// Packet buffers Microsoft's encoder reuses in turn.
const STALE_RING: usize = 52;

/// The audio stream's header and media objects of a sample, if it is WMA
/// Pro.
fn stream(path: &Path) -> Option<(Config, Vec<Vec<u8>>)> {
    let bytes = std::fs::read(path).unwrap();
    let asf = Asf::parse(&bytes).unwrap();
    let (number, wave) = asf
        .stream_properties()
        .find_map(|sp| match &sp.type_specific {
            TypeSpecificData::Audio(w) if w.format_tag == 0x0162 => {
                Some((sp.flags.stream_number(), w.clone()))
            }
            _ => None,
        })?;
    let header = Header::parse(&wave.extra).unwrap();
    let cfg = Config::new(header, usize::from(wave.block_align), wave.samples_per_sec).unwrap();
    let objects = asf
        .media_objects()
        .unwrap()
        .into_iter()
        .filter(|o| o.stream == number)
        .map(|o| o.data)
        .collect();
    Some((cfg, objects))
}

fn samples() -> Vec<std::path::PathBuf> {
    let mut v: Vec<_> = ["testdata/ads", "testdata/movies"]
        .iter()
        .filter_map(|d| std::fs::read_dir(d).ok())
        .flatten()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "wmv"))
        .collect();
    v.sort();
    v
}

#[test]
fn sample_frames_read_and_write_back() {
    let files = samples();
    if files.is_empty() {
        eprintln!("testdata/ absent: skipped");
        return;
    }
    let mut streams = 0;
    for path in &files {
        let Some((cfg, objects)) = stream(path) else {
            continue;
        };
        streams += 1;
        let refs: Vec<&[u8]> = objects.iter().map(Vec::as_slice).collect();
        let split = split(&cfg, &refs).unwrap();
        let codec = Codec::new(&cfg);
        for (i, f) in split.frames.iter().enumerate() {
            let (frame, trailer) = codec
                .read(&f.bits)
                .unwrap_or_else(|e| panic!("{}: frame {i}: {e}", path.display()));
            let again = codec.write(&frame, trailer).unwrap();
            if again != f.bits {
                let at = again.iter().zip(&f.bits).position(|(a, b)| a != b);
                panic!(
                    "{}: frame {i} of {} bits writes {} bits, first difference at {at:?}",
                    path.display(),
                    f.bits.len(),
                    again.len()
                );
            }
        }
        // The packets again, from the frames written back and laid out.
        let models: Vec<_> = split
            .frames
            .iter()
            .map(|f| codec.read(&f.bits).unwrap().0)
            .collect();
        let lens: Vec<usize> = split.frames.iter().map(|f| f.bits.len()).collect();
        let layout = layout(&cfg, &lens).unwrap();
        assert_eq!(
            layout.starts,
            split.frames.iter().map(|f| f.packet).collect::<Vec<_>>(),
            "{}: frame starts",
            path.display()
        );
        // `join` sets the trailer bits.
        let frames: Vec<_> = models
            .iter()
            .map(|m| codec.write(m, false).unwrap())
            .collect();
        let packets = join(&cfg, &frames, &layout, split.headers[0].sequence);
        assert_eq!(packets.len(), objects.len(), "{}: packets", path.display());
        // After its last frame, the last packet holds zeros or the bytes of
        // the packet 52 before: the encoder reuses 52 packet buffers
        // without clearing them. Ours holds zeros.
        let last = objects.len() - 1;
        let end = (cfg.block_align * 8 - split.tail.len()).div_ceil(8);
        let stale = &objects[last][end..];
        assert!(
            stale.iter().all(|&b| b == 0)
                || last >= STALE_RING && *stale == objects[last - STALE_RING][end..],
            "{}: the end of the last packet",
            path.display()
        );
        let mut theirs_cleared = objects.clone();
        let data_bits = cfg.block_align * 8 - split.tail.len();
        let partial = &mut theirs_cleared[last][data_bits / 8];
        *partial &= !(0xffu8.checked_shr((data_bits % 8) as u32).unwrap_or(0));
        theirs_cleared[last][end..].fill(0);
        for (k, (ours, theirs)) in packets.iter().zip(&theirs_cleared).enumerate() {
            if ours != theirs {
                let at = ours.iter().zip(theirs).position(|(a, b)| a != b);
                panic!("{}: packet {k} differs from byte {at:?}", path.display());
            }
        }
        eprintln!("{}: {} frames", path.display(), split.frames.len());
    }
    assert_eq!(streams, 21);
}

/// How often Microsoft's encoder takes each choice the syntax offers,
/// over all sample streams: the target the encoder follows.
#[test]
fn sample_stream_choices() {
    use std::collections::BTreeMap;
    use wmv9::wma::pro::syntax::{CoefTable, ScaleFactors, Transform};

    let files = samples();
    if files.is_empty() {
        eprintln!("testdata/ absent: skipped");
        return;
    }
    let mut n: BTreeMap<String, u64> = BTreeMap::new();
    let mut count = |k: String| *n.entry(k).or_default() += 1;
    for path in &files {
        let Some((cfg, objects)) = stream(path) else {
            continue;
        };
        let refs: Vec<&[u8]> = objects.iter().map(Vec::as_slice).collect();
        let split = split(&cfg, &refs).unwrap();
        let codec = Codec::new(&cfg);
        count(format!("stream block_align {}", cfg.block_align));
        for h in &split.headers {
            count(format!("packet reserved bits {}", h.reserved));
        }
        let mut last_in_packet = BTreeMap::new();
        for (i, f) in split.frames.iter().enumerate() {
            last_in_packet.insert(f.packet, i);
        }
        for (i, f) in split.frames.iter().enumerate() {
            let (frame, trailer) = codec.read(&f.bits).unwrap();
            let last = last_in_packet[&f.packet] == i;
            count(format!(
                "trailer {} on {} frame of its packet",
                u8::from(trailer),
                if last { "last" } else { "other" }
            ));
            let tiles: Vec<String> = frame
                .tiling
                .0
                .iter()
                .map(|t| {
                    t.iter()
                        .map(|l| l.to_string())
                        .collect::<Vec<_>>()
                        .join("+")
                })
                .collect();
            count(format!(
                "tiling same {} {}",
                frame.same_tiling,
                if tiles.iter().all(|t| *t == tiles[0]) {
                    tiles[0].clone()
                } else {
                    tiles.join(" | ")
                }
            ));
            count(format!("postproc {:?}", frame.postproc));
            if let Some(g) = frame.drc_gain {
                count(format!("drc gain {g}"));
            }
            count(format!("trim {:?}", frame.trim));
            for s in &frame.subframes {
                count(format!(
                    "subframe channels {} len {}",
                    s.channel_indexes.len(),
                    s.len
                ));
                count(format!("fill {}", s.fill.is_some()));
                for g in &s.groups {
                    let kind = match &g.transform {
                        Transform::Matrix { .. } => "Matrix".to_string(),
                        t => format!("{t:?}"),
                    };
                    count(format!("transform {kind} per-band {}", g.bands.is_some()));
                }
                if let Some(q) = &s.quant {
                    count(format!("vector counts {}", q.vector_counts));
                    count(format!("modifier len {:?}", q.modifier_len));
                    count(format!("quant step {}", q.step / 10 * 10));
                }
                for ch in &s.channels {
                    count(format!("transmit {}", ch.transmit));
                    if ch.modifier.is_some() {
                        count("modifier coded".into());
                    }
                    match &ch.scale {
                        Some(ScaleFactors::Dpcm { step, .. }) => {
                            count(format!("scale dpcm step {step}"))
                        }
                        Some(ScaleFactors::Kept) => count("scale kept".into()),
                        Some(ScaleFactors::Changed(c)) => {
                            count(format!("scale changed {}", c.len().min(5)))
                        }
                        None => {}
                    }
                    if let Some(c) = &ch.coefficients {
                        let table = match c.table {
                            CoefTable::First => 0,
                            CoefTable::Second => 1,
                        };
                        count(format!("coefficient table {table}"));
                    }
                }
            }
        }
    }
    insta::assert_yaml_snapshot!(n);
}
