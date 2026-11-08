use super::guid::{self, Guid};
use super::*;
use crate::bytes::PutLe;

pub(super) fn write(asf: &Asf) -> Result<Vec<u8>, Error> {
    let mut out = Vec::new();
    object(&mut out, guid::HEADER, |b| header(b, &asf.header))?;
    let packet_size = asf
        .file_properties()
        .ok_or(Error::MissingFileProperties)?
        .max_data_packet_size;
    for obj in &asf.objects {
        match obj {
            TopLevelObject::Data(d) => object(&mut out, guid::DATA, |b| data(b, d, packet_size))?,
            TopLevelObject::Raw(raw) => raw_object(&mut out, raw),
        }
    }
    Ok(out)
}

/// Writes GUID, a size placeholder, the body, then patches the size.
fn object(
    out: &mut Vec<u8>,
    guid: Guid,
    body: impl FnOnce(&mut Vec<u8>) -> Result<(), Error>,
) -> Result<(), Error> {
    let start = out.len();
    out.extend_from_slice(&guid.0);
    out.put_u64(0);
    body(out)?;
    let size = (out.len() - start) as u64;
    out[start + 16..start + 24].copy_from_slice(&size.to_le_bytes());
    Ok(())
}

fn raw_object(out: &mut Vec<u8>, raw: &RawObject) {
    out.extend_from_slice(&raw.guid.0);
    out.put_u64(24 + raw.body.len() as u64);
    out.extend_from_slice(&raw.body);
}

fn len_u32(field: &'static str, len: usize) -> Result<u32, Error> {
    u32::try_from(len).map_err(|_| Error::FieldOverflow {
        field,
        value: len as u64,
        width: FieldWidth::Dword,
    })
}

fn len_u16(field: &'static str, len: usize) -> Result<u16, Error> {
    u16::try_from(len).map_err(|_| Error::FieldOverflow {
        field,
        value: len as u64,
        width: FieldWidth::Word,
    })
}

fn header(out: &mut Vec<u8>, h: &Header) -> Result<(), Error> {
    out.put_u32(len_u32("header object count", h.objects.len())?);
    out.put_u8(h.reserved1);
    out.put_u8(h.reserved2);
    header_objects(out, &h.objects)
}

fn header_objects(out: &mut Vec<u8>, objects: &[HeaderObject]) -> Result<(), Error> {
    for obj in objects {
        match obj {
            HeaderObject::FileProperties(p) => object(out, guid::FILE_PROPERTIES, |b| {
                file_properties(b, p);
                Ok(())
            })?,
            HeaderObject::StreamProperties(p) => {
                object(out, guid::STREAM_PROPERTIES, |b| stream_properties(b, p))?
            }
            HeaderObject::HeaderExtension(e) => {
                object(out, guid::HEADER_EXTENSION, |b| header_extension(b, e))?
            }
            HeaderObject::Raw(raw) => raw_object(out, raw),
        }
    }
    Ok(())
}

fn file_properties(out: &mut Vec<u8>, p: &FileProperties) {
    out.extend_from_slice(&p.file_id.0);
    out.put_u64(p.file_size);
    out.put_u64(p.creation_date);
    out.put_u64(p.data_packets_count);
    out.put_u64(p.play_duration.0);
    out.put_u64(p.send_duration.0);
    out.put_u64(p.preroll.0);
    out.put_u32(p.flags.0);
    out.put_u32(p.min_data_packet_size);
    out.put_u32(p.max_data_packet_size);
    out.put_u32(p.max_bitrate);
}

fn stream_properties(out: &mut Vec<u8>, p: &StreamProperties) -> Result<(), Error> {
    let mut ts = Vec::new();
    match &p.type_specific {
        TypeSpecificData::Audio(w) => wave_format_ex(&mut ts, w)?,
        TypeSpecificData::Video(v) => video_media_type(&mut ts, v)?,
        TypeSpecificData::Raw(raw) => ts.extend_from_slice(raw),
    }
    out.extend_from_slice(&p.stream_type.0);
    out.extend_from_slice(&p.error_correction_type.0);
    out.put_u64(p.time_offset.0);
    out.put_u32(len_u32("type specific data length", ts.len())?);
    out.put_u32(len_u32(
        "error correction data length",
        p.error_correction_data.len(),
    )?);
    out.put_u16(p.flags.0);
    out.put_u32(p.reserved);
    out.extend_from_slice(&ts);
    out.extend_from_slice(&p.error_correction_data);
    Ok(())
}

fn wave_format_ex(out: &mut Vec<u8>, w: &WaveFormatEx) -> Result<(), Error> {
    out.put_u16(w.format_tag);
    out.put_u16(w.channels);
    out.put_u32(w.samples_per_sec);
    out.put_u32(w.avg_bytes_per_sec);
    out.put_u16(w.block_align);
    out.put_u16(w.bits_per_sample);
    out.put_u16(len_u16("WAVEFORMATEX cbSize", w.extra.len())?);
    out.extend_from_slice(&w.extra);
    Ok(())
}

fn video_media_type(out: &mut Vec<u8>, v: &VideoMediaType) -> Result<(), Error> {
    out.put_u32(v.encoded_width);
    out.put_u32(v.encoded_height);
    out.put_u8(v.reserved_flags);
    out.put_u16(len_u16("format data size", 40 + v.codec_data.len())?);
    let b = &v.bitmap;
    out.put_u32(b.size);
    out.put_i32(b.width);
    out.put_i32(b.height);
    out.put_u16(b.planes);
    out.put_u16(b.bit_count);
    out.extend_from_slice(&b.compression.0);
    out.put_u32(b.size_image);
    out.put_i32(b.x_pels_per_meter);
    out.put_i32(b.y_pels_per_meter);
    out.put_u32(b.clr_used);
    out.put_u32(b.clr_important);
    out.extend_from_slice(&v.codec_data);
    Ok(())
}

fn header_extension(out: &mut Vec<u8>, e: &HeaderExtension) -> Result<(), Error> {
    let mut inner = Vec::new();
    header_objects(&mut inner, &e.objects)?;
    out.extend_from_slice(&e.reserved_field1.0);
    out.put_u16(e.reserved_field2);
    out.put_u32(len_u32("header extension data size", inner.len())?);
    out.extend_from_slice(&inner);
    Ok(())
}

fn data(out: &mut Vec<u8>, d: &Data, packet_size: u32) -> Result<(), Error> {
    out.extend_from_slice(&d.file_id.0);
    out.put_u64(d.packets.len() as u64);
    out.put_u16(d.reserved);
    for (index, p) in d.packets.iter().enumerate() {
        packet(out, p, packet_size).map_err(|reason| Error::Packet {
            packet: index,
            reason,
        })?;
    }
    Ok(())
}

fn put_var(out: &mut Vec<u8>, v: VarField) {
    match v {
        VarField::Absent => {}
        VarField::Byte(x) => out.put_u8(x),
        VarField::Word(x) => out.put_u16(x),
        VarField::Dword(x) => out.put_u32(x),
    }
}

/// Writes `value` at `width`. An absent field can only encode zero.
fn put_width(
    out: &mut Vec<u8>,
    field: &'static str,
    width: FieldWidth,
    value: u32,
) -> Result<(), PacketError> {
    let overflow = PacketError::FieldOverflow {
        field,
        value: value.into(),
        width,
    };
    match width {
        FieldWidth::Absent if value == 0 => {}
        FieldWidth::Absent => return Err(overflow),
        FieldWidth::Byte => out.put_u8(u8::try_from(value).map_err(|_| overflow)?),
        FieldWidth::Word => out.put_u16(u16::try_from(value).map_err(|_| overflow)?),
        FieldWidth::Dword => out.put_u32(value),
    }
    Ok(())
}

fn data_len(field: &'static str, len: usize) -> Result<u32, PacketError> {
    u32::try_from(len).map_err(|_| PacketError::FieldOverflow {
        field,
        value: len as u64,
        width: FieldWidth::Dword,
    })
}

fn packet(out: &mut Vec<u8>, p: &Packet, packet_size: u32) -> Result<(), PacketError> {
    if let Some(declared) = p.packet_length.value()
        && declared != packet_size
    {
        return Err(PacketError::PacketLength {
            declared,
            size: packet_size,
        });
    }
    let start = out.len();
    let mut length_flags_bit = false;
    if let Some(ec) = &p.error_correction {
        if ec.data.len() > 0x0f {
            return Err(PacketError::ErrorCorrectionTooLong { len: ec.data.len() });
        }
        out.put_u8(0x80 | if ec.opaque { 0x10 } else { 0 } | ec.data.len() as u8);
        out.extend_from_slice(&ec.data);
        length_flags_bit = ec.length_flags_bit;
    }
    let multiple = matches!(p.payloads, Payloads::Multiple { .. });
    out.put_u8(
        u8::from(multiple)
            | p.sequence.width().bits() << 1
            | p.padding_length.width().bits() << 3
            | p.packet_length.width().bits() << 5
            | u8::from(length_flags_bit) << 7,
    );
    out.put_u8(
        p.widths.replicated_data_length.bits()
            | p.widths.offset_into_media_object.bits() << 2
            | p.widths.media_object_number.bits() << 4
            | 1 << 6,
    );
    put_var(out, p.packet_length);
    put_var(out, p.sequence);
    put_var(out, p.padding_length);
    out.put_u32(p.send_time.0);
    out.put_u16(p.duration.0);
    match &p.payloads {
        Payloads::Single(pl) => {
            payload_head(out, pl, p.widths)?;
            out.extend_from_slice(&pl.data);
        }
        Payloads::Multiple {
            payload_length_width,
            payloads,
        } => {
            if payloads.len() > 0x3f {
                return Err(PacketError::TooManyPayloads);
            }
            if *payload_length_width == FieldWidth::Absent {
                return Err(PacketError::MissingPayloadLength);
            }
            out.put_u8(payload_length_width.bits() << 6 | payloads.len() as u8);
            for pl in payloads {
                payload_head(out, pl, p.widths)?;
                put_width(
                    out,
                    "payload length",
                    *payload_length_width,
                    data_len("payload length", pl.data.len())?,
                )?;
                out.extend_from_slice(&pl.data);
            }
        }
    }
    let used = out.len() - start;
    let padding = p.padding_len();
    if used + padding as usize != packet_size as usize {
        return Err(PacketError::SizeMismatch {
            used,
            padding,
            size: packet_size,
        });
    }
    out.resize(out.len() + padding as usize, 0);
    Ok(())
}

fn payload_head(out: &mut Vec<u8>, pl: &Payload, w: PayloadFieldWidths) -> Result<(), PacketError> {
    out.put_u8(pl.stream.get() | u8::from(pl.key_frame) << 7);
    put_width(
        out,
        "media object number",
        w.media_object_number,
        pl.media_object_number.0,
    )?;
    match &pl.kind {
        PayloadKind::Fragment {
            offset,
            replicated_data,
        } => {
            if replicated_data.len() == 1 {
                return Err(PacketError::AmbiguousReplicatedData);
            }
            put_width(
                out,
                "offset into media object",
                w.offset_into_media_object,
                *offset,
            )?;
            put_width(
                out,
                "replicated data length",
                w.replicated_data_length,
                data_len("replicated data length", replicated_data.len())?,
            )?;
            out.extend_from_slice(replicated_data);
        }
        PayloadKind::Compressed {
            presentation_time,
            time_delta,
        } => {
            if w.replicated_data_length == FieldWidth::Absent {
                return Err(PacketError::CompressedWithoutReplicatedLength);
            }
            put_width(
                out,
                "presentation time",
                w.offset_into_media_object,
                *presentation_time,
            )?;
            put_width(out, "replicated data length", w.replicated_data_length, 1)?;
            out.put_u8(*time_delta);
        }
    }
    Ok(())
}
