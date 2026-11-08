//! Writes the audio stream of an ASF file as input for tools/wma_decode.exe:
//! OBJECTS holds one record per media object (u32 length, u32 presentation
//! time in ms with the preroll, then the object), FORMAT the WAVEFORMATEX
//! with its codec specific bytes.
//!
//! usage: cargo run --example extract_audio -- FILE.wmv OBJECTS FORMAT

use std::io::Write;

use wmv9::asf::{Asf, TypeSpecificData};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let [_, input, objects, format] = args.as_slice() else {
        return Err("usage: extract_audio FILE.wmv OBJECTS FORMAT".into());
    };
    let asf = Asf::parse(&std::fs::read(input)?)?;
    let (sp, wave) = asf
        .stream_properties()
        .find_map(|s| match &s.type_specific {
            TypeSpecificData::Audio(w) => Some((s, w)),
            _ => None,
        })
        .ok_or("no audio stream")?;
    let number = sp.flags.stream_number();
    let mut out = std::io::BufWriter::new(std::fs::File::create(objects)?);
    for o in asf
        .media_objects()?
        .into_iter()
        .filter(|o| o.stream == number)
    {
        out.write_all(&u32::try_from(o.data.len())?.to_le_bytes())?;
        out.write_all(&u32::try_from(o.presentation_time.0)?.to_le_bytes())?;
        out.write_all(&o.data)?;
    }
    out.flush()?;
    let mut w = Vec::new();
    w.extend(wave.format_tag.to_le_bytes());
    w.extend(wave.channels.to_le_bytes());
    w.extend(wave.samples_per_sec.to_le_bytes());
    w.extend(wave.avg_bytes_per_sec.to_le_bytes());
    w.extend(wave.block_align.to_le_bytes());
    w.extend(wave.bits_per_sample.to_le_bytes());
    w.extend(u16::try_from(wave.extra.len())?.to_le_bytes());
    w.extend(&wave.extra);
    std::fs::write(format, w)?;
    println!(
        "format {:#06x}, {} Hz, {} channels, block_align {}",
        wave.format_tag, wave.samples_per_sec, wave.channels, wave.block_align
    );
    Ok(())
}
