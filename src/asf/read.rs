use super::guid::{self, Guid};
use super::*;
use crate::bytes::Reader;

const OBJECT_HEADER_LEN: u64 = 24;

pub(super) fn parse(bytes: &[u8]) -> Result<Asf, Error> {
    let mut r = Reader::new(bytes, 0);
    let (guid, mut body) = object(&mut r)?;
    if guid != guid::HEADER {
        return Err(Error::MissingHeader { found: guid });
    }
    let header = header(&mut body)?;
    let packet_size = packet_size(&header)?;

    let mut objects = Vec::new();
    while !r.is_empty() {
        let (guid, mut body) = object(&mut r)?;
        objects.push(if guid == guid::DATA {
            TopLevelObject::Data(data(&mut body, packet_size)?)
        } else {
            TopLevelObject::Raw(RawObject {
                guid,
                body: body.rest().to_vec(),
            })
        });
    }
    Ok(Asf { header, objects })
}

fn packet_size(header: &Header) -> Result<u32, Error> {
    let props = header
        .objects
        .iter()
        .find_map(|o| match o {
            HeaderObject::FileProperties(p) => Some(p),
            _ => None,
        })
        .ok_or(Error::MissingFileProperties)?;
    if props.min_data_packet_size != props.max_data_packet_size {
        return Err(Error::VariablePacketSize {
            min: props.min_data_packet_size,
            max: props.max_data_packet_size,
        });
    }
    Ok(props.max_data_packet_size)
}

/// Reads an object header and returns its GUID and a reader over the body.
fn object<'a>(r: &mut Reader<'a>) -> Result<(Guid, Reader<'a>), Error> {
    let offset = r.offset();
    let guid = Guid(r.array()?);
    let size = r.u64()?;
    let body_len = size
        .checked_sub(OBJECT_HEADER_LEN)
        .filter(|&n| n <= r.remaining() as u64)
        .ok_or(Error::ObjectSize { guid, offset, size })?;
    Ok((guid, r.sub(body_len as usize)?))
}

fn header(r: &mut Reader) -> Result<Header, Error> {
    let declared = r.u32()?;
    let reserved1 = r.u8()?;
    let reserved2 = r.u8()?;
    let objects = header_objects(r)?;
    if objects.len() != declared as usize {
        return Err(Error::HeaderObjectCount {
            declared,
            found: objects.len(),
        });
    }
    Ok(Header {
        reserved1,
        reserved2,
        objects,
    })
}

fn header_objects(r: &mut Reader) -> Result<Vec<HeaderObject>, Error> {
    let mut out = Vec::new();
    while !r.is_empty() {
        let (guid, mut body) = object(r)?;
        let obj = match guid {
            guid::FILE_PROPERTIES => HeaderObject::FileProperties(file_properties(&mut body)?),
            guid::STREAM_PROPERTIES => {
                HeaderObject::StreamProperties(stream_properties(&mut body)?)
            }
            guid::HEADER_EXTENSION => HeaderObject::HeaderExtension(header_extension(&mut body)?),
            _ => HeaderObject::Raw(RawObject {
                guid,
                body: body.rest().to_vec(),
            }),
        };
        expect_consumed("header object body", &body)?;
        out.push(obj);
    }
    Ok(out)
}

fn expect_consumed(what: &'static str, r: &Reader) -> Result<(), Error> {
    if r.is_empty() {
        Ok(())
    } else {
        Err(Error::TrailingBytes {
            what,
            offset: r.offset(),
            len: r.remaining(),
        })
    }
}

/// Splits off `len` bytes whose length was declared by a preceding field.
fn sized<'a>(r: &mut Reader<'a>, what: &'static str, len: u64) -> Result<Reader<'a>, Error> {
    if len > r.remaining() as u64 {
        return Err(Error::LengthMismatch {
            what,
            offset: r.offset(),
            declared: len,
            available: r.remaining() as u64,
        });
    }
    Ok(r.sub(len as usize)?)
}

fn file_properties(r: &mut Reader) -> Result<FileProperties, Error> {
    Ok(FileProperties {
        file_id: Guid(r.array()?),
        file_size: r.u64()?,
        creation_date: r.u64()?,
        data_packets_count: r.u64()?,
        play_duration: Hns(r.u64()?),
        send_duration: Hns(r.u64()?),
        preroll: Millis(r.u64()?),
        flags: FilePropertiesFlags(r.u32()?),
        min_data_packet_size: r.u32()?,
        max_data_packet_size: r.u32()?,
        max_bitrate: r.u32()?,
    })
}

fn stream_properties(r: &mut Reader) -> Result<StreamProperties, Error> {
    let stream_type = Guid(r.array()?);
    let error_correction_type = Guid(r.array()?);
    let time_offset = Hns(r.u64()?);
    let type_specific_len = r.u32()?;
    let error_correction_len = r.u32()?;
    let flags = StreamPropertiesFlags(r.u16()?);
    let reserved = r.u32()?;
    let mut ts = sized(r, "type specific data", type_specific_len.into())?;
    let type_specific = match stream_type {
        guid::AUDIO_MEDIA => TypeSpecificData::Audio(wave_format_ex(&mut ts)?),
        guid::VIDEO_MEDIA => TypeSpecificData::Video(video_media_type(&mut ts)?),
        _ => TypeSpecificData::Raw(ts.rest().to_vec()),
    };
    expect_consumed("type specific data", &ts)?;
    let error_correction_data = sized(r, "error correction data", error_correction_len.into())?
        .rest()
        .to_vec();
    Ok(StreamProperties {
        stream_type,
        error_correction_type,
        time_offset,
        flags,
        reserved,
        type_specific,
        error_correction_data,
    })
}

fn wave_format_ex(r: &mut Reader) -> Result<WaveFormatEx, Error> {
    let format_tag = r.u16()?;
    let channels = r.u16()?;
    let samples_per_sec = r.u32()?;
    let avg_bytes_per_sec = r.u32()?;
    let block_align = r.u16()?;
    let bits_per_sample = r.u16()?;
    let cb_size = r.u16()?;
    let extra = sized(r, "WAVEFORMATEX cbSize", cb_size.into())?
        .rest()
        .to_vec();
    Ok(WaveFormatEx {
        format_tag,
        channels,
        samples_per_sec,
        avg_bytes_per_sec,
        block_align,
        bits_per_sample,
        extra,
    })
}

const BITMAPINFOHEADER_LEN: u16 = 40;

fn video_media_type(r: &mut Reader) -> Result<VideoMediaType, Error> {
    let encoded_width = r.u32()?;
    let encoded_height = r.u32()?;
    let reserved_flags = r.u8()?;
    let format_data_size = r.u16()?;
    let mut fmt = sized(r, "format data", format_data_size.into())?;
    if format_data_size < BITMAPINFOHEADER_LEN {
        return Err(Error::FormatTooShort {
            offset: fmt.offset(),
            len: format_data_size,
        });
    }
    let bitmap = BitmapInfoHeader {
        size: fmt.u32()?,
        width: fmt.i32()?,
        height: fmt.i32()?,
        planes: fmt.u16()?,
        bit_count: fmt.u16()?,
        compression: FourCc(fmt.array()?),
        size_image: fmt.u32()?,
        x_pels_per_meter: fmt.i32()?,
        y_pels_per_meter: fmt.i32()?,
        clr_used: fmt.u32()?,
        clr_important: fmt.u32()?,
    };
    Ok(VideoMediaType {
        encoded_width,
        encoded_height,
        reserved_flags,
        bitmap,
        codec_data: fmt.rest().to_vec(),
    })
}

fn header_extension(r: &mut Reader) -> Result<HeaderExtension, Error> {
    let reserved_field1 = Guid(r.array()?);
    let reserved_field2 = r.u16()?;
    let data_size = r.u32()?;
    let mut ext = sized(r, "header extension data", data_size.into())?;
    Ok(HeaderExtension {
        reserved_field1,
        reserved_field2,
        objects: header_objects(&mut ext)?,
    })
}

fn data(r: &mut Reader, packet_size: u32) -> Result<Data, Error> {
    let file_id = Guid(r.array()?);
    let declared = r.u64()?;
    let reserved = r.u16()?;
    let size = packet_size as usize;
    if size == 0 || !r.remaining().is_multiple_of(size) {
        return Err(Error::LengthMismatch {
            what: "data packets",
            offset: r.offset(),
            declared: u64::from(packet_size),
            available: r.remaining() as u64,
        });
    }
    let mut packets = Vec::with_capacity(r.remaining() / size);
    while !r.is_empty() {
        let index = packets.len();
        let mut pr = r.sub(size)?;
        packets.push(
            packet(&mut pr, packet_size).map_err(|reason| Error::Packet {
                packet: index,
                reason,
            })?,
        );
    }
    if packets.len() as u64 != declared {
        return Err(Error::PacketCount {
            declared,
            found: packets.len() as u64,
        });
    }
    Ok(Data {
        file_id,
        reserved,
        packets,
    })
}

fn var_field(r: &mut Reader, width: FieldWidth) -> Result<VarField, Truncated> {
    Ok(match width {
        FieldWidth::Absent => VarField::Absent,
        FieldWidth::Byte => VarField::Byte(r.u8()?),
        FieldWidth::Word => VarField::Word(r.u16()?),
        FieldWidth::Dword => VarField::Dword(r.u32()?),
    })
}

/// Reads a field whose absence the specification defines as zero.
fn var_value_or_zero(r: &mut Reader, width: FieldWidth) -> Result<u32, Truncated> {
    Ok(var_field(r, width)?.value().unwrap_or(0))
}

const EC_PRESENT: u8 = 0x80;

fn packet(r: &mut Reader, packet_size: u32) -> Result<Packet, PacketError> {
    let start = r.remaining();
    let mut first = r.u8()?;
    let mut error_correction = None;
    if first & EC_PRESENT != 0 {
        let length_type = (first >> 5) & 3;
        if length_type != 0 {
            return Err(PacketError::ErrorCorrectionLengthType(length_type));
        }
        let opaque = first & 0x10 != 0;
        let data = r.take((first & 0x0f).into())?.to_vec();
        first = r.u8()?;
        error_correction = Some(ErrorCorrection {
            opaque,
            data,
            length_flags_bit: first & EC_PRESENT != 0,
        });
    }
    let length_flags = first;
    let property_flags = r.u8()?;
    let stream_width = property_flags >> 6;
    if stream_width != 1 {
        return Err(PacketError::StreamNumberWidth(stream_width));
    }
    let widths = PayloadFieldWidths {
        replicated_data_length: FieldWidth::from_bits(property_flags),
        offset_into_media_object: FieldWidth::from_bits(property_flags >> 2),
        media_object_number: FieldWidth::from_bits(property_flags >> 4),
    };
    let packet_length = var_field(r, FieldWidth::from_bits(length_flags >> 5))?;
    let sequence = var_field(r, FieldWidth::from_bits(length_flags >> 1))?;
    let padding_length = var_field(r, FieldWidth::from_bits(length_flags >> 3))?;
    if let Some(declared) = packet_length.value()
        && declared != packet_size
    {
        return Err(PacketError::PacketLength {
            declared,
            size: packet_size,
        });
    }
    let send_time = SendTime(r.u32()?);
    let duration = PacketDuration(r.u16()?);
    let padding = padding_length.value().unwrap_or(0);
    let size_mismatch = |r: &Reader| PacketError::SizeMismatch {
        used: start - r.remaining(),
        padding,
        size: packet_size,
    };

    let payloads = if length_flags & 1 != 0 {
        let flags = r.u8()?;
        let payload_length_width = FieldWidth::from_bits(flags >> 6);
        if payload_length_width == FieldWidth::Absent {
            return Err(PacketError::MissingPayloadLength);
        }
        let mut payloads = Vec::with_capacity((flags & 0x3f).into());
        for _ in 0..flags & 0x3f {
            let mut p = payload_head(r, widths)?;
            let len = var_value_or_zero(r, payload_length_width)?;
            p.data = r.take(len as usize)?.to_vec();
            payloads.push(p);
        }
        Payloads::Multiple {
            payload_length_width,
            payloads,
        }
    } else {
        let mut p = payload_head(r, widths)?;
        let len = r
            .remaining()
            .checked_sub(padding as usize)
            .ok_or_else(|| size_mismatch(r))?;
        p.data = r.take(len)?.to_vec();
        Payloads::Single(p)
    };

    if r.remaining() != padding as usize {
        return Err(size_mismatch(r));
    }
    if r.rest().iter().any(|&b| b != 0) {
        return Err(PacketError::NonZeroPadding);
    }
    Ok(Packet {
        error_correction,
        packet_length,
        sequence,
        padding_length,
        widths,
        send_time,
        duration,
        payloads,
    })
}

/// One replicated data byte marks a compressed payload.
const COMPRESSED_REPLICATED_LEN: u32 = 1;

/// Reads a payload up to the start of its data.
fn payload_head(r: &mut Reader, widths: PayloadFieldWidths) -> Result<Payload, Truncated> {
    let stream = r.u8()?;
    let media_object_number = MediaObjectNumber(var_value_or_zero(r, widths.media_object_number)?);
    let offset = var_value_or_zero(r, widths.offset_into_media_object)?;
    let replicated_len = var_value_or_zero(r, widths.replicated_data_length)?;
    let kind = if replicated_len == COMPRESSED_REPLICATED_LEN {
        PayloadKind::Compressed {
            presentation_time: offset,
            time_delta: r.u8()?,
        }
    } else {
        PayloadKind::Fragment {
            offset,
            replicated_data: r.take(replicated_len as usize)?.to_vec(),
        }
    };
    Ok(Payload {
        stream: StreamNumber::new(stream & 0x7f).expect("masked to 7 bits"),
        key_frame: stream & 0x80 != 0,
        media_object_number,
        kind,
        data: Vec::new(),
    })
}
