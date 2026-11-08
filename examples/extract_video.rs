//! Writes the video stream of an ASF file as input for tools/dmo_decode.exe:
//! FRAMES holds one record per frame (u32 length, u32 flags with bit 0 set
//! on key frames, then the frame), SEQHDR the codec private data, and the
//! coded width and height go to stdout.
//!
//! usage: cargo run --example extract_video -- FILE.wmv FRAMES SEQHDR

use std::io::Write;

use wmv9::asf::{Asf, TypeSpecificData};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let [_, input, frames, seqhdr] = args.as_slice() else {
        return Err("usage: extract_video FILE.wmv FRAMES SEQHDR".into());
    };
    let asf = Asf::parse(&std::fs::read(input)?)?;
    let (sp, video) = asf
        .stream_properties()
        .find_map(|s| match &s.type_specific {
            TypeSpecificData::Video(v) => Some((s, v)),
            _ => None,
        })
        .ok_or("no video stream")?;
    let number = sp.flags.stream_number();
    let mut out = std::io::BufWriter::new(std::fs::File::create(frames)?);
    for o in asf
        .media_objects()?
        .into_iter()
        .filter(|o| o.stream == number)
    {
        out.write_all(&u32::try_from(o.data.len())?.to_le_bytes())?;
        out.write_all(&u32::from(o.key_frame).to_le_bytes())?;
        out.write_all(&o.data)?;
    }
    out.flush()?;
    std::fs::write(seqhdr, &video.codec_data)?;
    println!("{} {}", video.encoded_width, video.encoded_height);
    Ok(())
}
