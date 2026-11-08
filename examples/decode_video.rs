//! Decodes the video stream of an ASF file to raw YUV 4:2:0 in output
//! order, the pictures FFmpeg drops left out.
//!
//! usage: cargo run --example decode_video -- FILE.wmv OUT.yuv

use std::io::Write;

use wmv9::asf::{Asf, TypeSpecificData};
use wmv9::wmv3::decode::{Decoder, PictureRole, display_order};
use wmv9::wmv3::{Frame, FrameContext, SequenceHeader};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let [_, input, output] = args.as_slice() else {
        return Err("usage: decode_video FILE.wmv OUT.yuv".into());
    };
    let asf = Asf::parse(&std::fs::read(input)?)?;
    let (sp, video) = asf
        .stream_properties()
        .find_map(|s| match &s.type_specific {
            TypeSpecificData::Video(v) => Some((s, v)),
            _ => None,
        })
        .ok_or("no video stream")?;
    let seq = SequenceHeader::parse(&video.codec_data)?;
    let ctx = FrameContext {
        seq: &seq,
        width: u16::try_from(video.encoded_width)?,
        height: u16::try_from(video.encoded_height)?,
    };
    let number = sp.flags.stream_number();
    let frames = asf
        .media_objects()?
        .into_iter()
        .filter(|o| o.stream == number)
        .map(|o| Frame::parse(&o.data, &ctx))
        .collect::<Result<Vec<_>, _>>()?;
    let order = display_order(frames.iter().map(|f| PictureRole::of(&f.header.kind)));
    let mut shown = vec![Vec::new(); order.iter().flatten().count()];
    let mut decoder = Decoder::new(ctx);
    for (f, slot) in frames.iter().zip(&order) {
        if let (Some(pic), Some(slot)) = (decoder.decode(f)?, slot) {
            shown[*slot] = pic.to_yuv420p(ctx.width.into(), ctx.height.into());
        }
    }
    let mut out = std::io::BufWriter::new(std::fs::File::create(output)?);
    for frame in shown {
        out.write_all(&frame)?;
    }
    out.flush()?;
    Ok(())
}
