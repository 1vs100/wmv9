//! Reassembly of media objects (video frames, audio blocks) from payloads.

use std::collections::BTreeMap;

use super::{
    Asf, FieldWidth, MediaObjectNumber, Millis, Payload, PayloadKind, Payloads, StreamNumber,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaObject {
    pub stream: StreamNumber,
    pub number: MediaObjectNumber,
    pub presentation_time: Millis,
    pub key_frame: bool,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MediaObjectError {
    #[error(
        "packet {packet}: stream {stream:?} payload has {len} bytes of replicated data, need 8"
    )]
    ShortReplicatedData {
        packet: usize,
        stream: StreamNumber,
        len: usize,
    },
    #[error(
        "packet {packet}: stream {stream:?} fragment {number:?} at offset {offset} does not continue the pending object"
    )]
    Discontinuous {
        packet: usize,
        stream: StreamNumber,
        number: MediaObjectNumber,
        offset: u32,
    },
    #[error(
        "packet {packet}: stream {stream:?} object {number:?} is {have} bytes, declared {declared}"
    )]
    SizeMismatch {
        packet: usize,
        stream: StreamNumber,
        number: MediaObjectNumber,
        have: usize,
        declared: u32,
    },
    #[error("stream {stream:?} ends inside object {number:?} ({have} of {declared} bytes)")]
    Incomplete {
        stream: StreamNumber,
        number: MediaObjectNumber,
        have: usize,
        declared: u32,
    },
    #[error("packet {packet}: stream {stream:?} compressed payload sub-payload overruns data")]
    CompressedOverrun { packet: usize, stream: StreamNumber },
    #[error("packet {packet}: stream {stream:?} compressed payload inside a fragmented object")]
    CompressedWhilePending { packet: usize, stream: StreamNumber },
}

struct Pending {
    object: MediaObject,
    size: u32,
}

pub(super) fn media_objects(asf: &Asf) -> Result<Vec<MediaObject>, MediaObjectError> {
    let mut out = Vec::new();
    let mut pending: BTreeMap<StreamNumber, Pending> = BTreeMap::new();
    let Some(data) = asf.data() else {
        return Ok(out);
    };
    for (packet, p) in data.packets.iter().enumerate() {
        let payloads = match &p.payloads {
            Payloads::Single(pl) => std::slice::from_ref(pl),
            Payloads::Multiple { payloads, .. } => payloads.as_slice(),
        };
        for pl in payloads {
            match &pl.kind {
                PayloadKind::Fragment {
                    offset,
                    replicated_data,
                } => fragment(packet, pl, *offset, replicated_data, &mut pending, &mut out)?,
                PayloadKind::Compressed {
                    presentation_time,
                    time_delta,
                } => {
                    if pending.contains_key(&pl.stream) {
                        return Err(MediaObjectError::CompressedWhilePending {
                            packet,
                            stream: pl.stream,
                        });
                    }
                    let c = Compressed {
                        packet,
                        start_time: *presentation_time,
                        time_delta: *time_delta,
                        number_width: p.widths.media_object_number,
                    };
                    c.split(pl, &mut out)?;
                }
            }
        }
    }
    if let Some((stream, p)) = pending.into_iter().next() {
        return Err(MediaObjectError::Incomplete {
            stream,
            number: p.object.number,
            have: p.object.data.len(),
            declared: p.size,
        });
    }
    Ok(out)
}

fn fragment(
    packet: usize,
    pl: &Payload,
    offset: u32,
    replicated: &[u8],
    pending: &mut BTreeMap<StreamNumber, Pending>,
    out: &mut Vec<MediaObject>,
) -> Result<(), MediaObjectError> {
    let (Some(size), Some(time)) = (replicated.get(0..4), replicated.get(4..8)) else {
        return Err(MediaObjectError::ShortReplicatedData {
            packet,
            stream: pl.stream,
            len: replicated.len(),
        });
    };
    let size = u32::from_le_bytes(size.try_into().unwrap());
    let time = u32::from_le_bytes(time.try_into().unwrap());
    if offset == 0 {
        if let Some(prev) = pending.remove(&pl.stream) {
            return Err(MediaObjectError::SizeMismatch {
                packet,
                stream: pl.stream,
                number: prev.object.number,
                have: prev.object.data.len(),
                declared: prev.size,
            });
        }
        pending.insert(
            pl.stream,
            Pending {
                object: MediaObject {
                    stream: pl.stream,
                    number: pl.media_object_number,
                    presentation_time: Millis(time.into()),
                    key_frame: pl.key_frame,
                    // The declared size is untrusted; grow from what is present.
                    data: Vec::with_capacity(pl.data.len()),
                },
                size,
            },
        );
    }
    let cur = pending
        .get_mut(&pl.stream)
        .filter(|p| {
            p.object.number == pl.media_object_number && p.object.data.len() == offset as usize
        })
        .ok_or(MediaObjectError::Discontinuous {
            packet,
            stream: pl.stream,
            number: pl.media_object_number,
            offset,
        })?;
    cur.object.data.extend_from_slice(&pl.data);
    if cur.object.data.len() >= cur.size as usize {
        let done = pending.remove(&pl.stream).expect("entry exists");
        if done.object.data.len() != done.size as usize {
            return Err(MediaObjectError::SizeMismatch {
                packet,
                stream: pl.stream,
                number: done.object.number,
                have: done.object.data.len(),
                declared: done.size,
            });
        }
        out.push(done.object);
    }
    Ok(())
}

/// Each sub-payload of a compressed payload is a whole media object. Object
/// numbers advance by one and wrap at the width of their field.
struct Compressed {
    packet: usize,
    start_time: u32,
    time_delta: u8,
    number_width: FieldWidth,
}

impl Compressed {
    fn split(&self, pl: &Payload, out: &mut Vec<MediaObject>) -> Result<(), MediaObjectError> {
        let mask = match self.number_width {
            FieldWidth::Absent => 0,
            FieldWidth::Byte => 0xff,
            FieldWidth::Word => 0xffff,
            FieldWidth::Dword => u32::MAX,
        };
        let mut time = u64::from(self.start_time);
        let mut number = pl.media_object_number.0;
        let mut rest = pl.data.as_slice();
        while let Some((&len, tail)) = rest.split_first() {
            let len = usize::from(len);
            if len > tail.len() {
                return Err(MediaObjectError::CompressedOverrun {
                    packet: self.packet,
                    stream: pl.stream,
                });
            }
            out.push(MediaObject {
                stream: pl.stream,
                number: MediaObjectNumber(number),
                presentation_time: Millis(time),
                key_frame: pl.key_frame,
                data: tail[..len].to_vec(),
            });
            rest = &tail[len..];
            time += u64::from(self.time_delta);
            number = number.wrapping_add(1) & mask;
        }
        Ok(())
    }
}
