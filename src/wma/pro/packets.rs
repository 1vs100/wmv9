//! WMA Pro packets: `block_align` bytes that start with a 4-bit sequence
//! number, 2 reserved bits and the number of bits that end the frame the
//! previous packet started; frames follow, each with its length, and the
//! packet ends with the start of a frame the next packet ends.

use super::config::Config;
use super::syntax::{Bits, pack, unpack};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PacketHeader {
    pub sequence: Sequence,
    pub reserved: u8,
    /// Bits at the start of the packet that end the previous frame.
    pub prev_bits: usize,
}

/// A frame and where it was found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameBits {
    /// From the length prefix through the trailer bit.
    pub bits: Bits,
    /// Packet the frame starts in.
    pub packet: usize,
}

/// A stream taken apart into packet headers and frames.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Split {
    pub headers: Vec<PacketHeader>,
    pub frames: Vec<FrameBits>,
    /// Bits after the last whole frame, which no frame completes.
    pub tail: Bits,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PacketError {
    #[error("packet {packet} has {len} bytes, not block_align {block_align}")]
    Size {
        packet: usize,
        len: usize,
        block_align: usize,
    },
    #[error("packet {packet} ends {prev_bits} bits of a frame, but none is open")]
    NoOpenFrame { packet: usize, prev_bits: usize },
    #[error("streams without frame length prefixes are not read")]
    NoLengthPrefix,
}

fn read_field(bits: &[bool], at: usize, width: u32) -> usize {
    bits[at..at + width as usize]
        .iter()
        .fold(0, |a, &b| a << 1 | usize::from(b))
}

/// Splits packets into frames. A frame whose length prefix does not fit
/// the rest of a packet continues in the next packet's first `prev_bits`.
pub fn split(cfg: &Config, packets: &[&[u8]]) -> Result<Split, PacketError> {
    if !cfg.header.decode_flags.len_prefix() {
        return Err(PacketError::NoLengthPrefix);
    }
    let w = cfg.log2_frame_size;
    let mut headers = Vec::new();
    let mut frames = Vec::new();
    // The frame begun in an earlier packet: its bits so far and that
    // packet.
    let mut open: Option<(Bits, usize)> = None;
    for (index, p) in packets.iter().enumerate() {
        if p.len() != cfg.block_align {
            return Err(PacketError::Size {
                packet: index,
                len: p.len(),
                block_align: cfg.block_align,
            });
        }
        let bits = unpack(p, p.len() * 8);
        let header = PacketHeader {
            sequence: Sequence(read_field(&bits, 0, 4) as u8),
            reserved: read_field(&bits, 4, 2) as u8,
            prev_bits: read_field(&bits, 6, w),
        };
        headers.push(header);
        let mut pos = 6 + w as usize;
        if header.prev_bits > 0 {
            let (mut frame, start) = open.take().ok_or(PacketError::NoOpenFrame {
                packet: index,
                prev_bits: header.prev_bits,
            })?;
            let take = header.prev_bits.min(bits.len() - pos);
            frame.extend(&bits[pos..pos + take]);
            pos += take;
            if take < header.prev_bits || pos == bits.len() && !complete(&frame, w) {
                open = Some((frame, start));
                continue;
            }
            frames.push(FrameBits {
                bits: frame,
                packet: start,
            });
        }
        // Whole frames, then the start of one the next packet ends.
        loop {
            let rest = bits.len() - pos;
            if rest <= w as usize {
                break;
            }
            let len = read_field(&bits, pos, w);
            if len == 0 || len > rest {
                break;
            }
            frames.push(FrameBits {
                bits: bits[pos..pos + len].to_vec(),
                packet: index,
            });
            pos += len;
            if !bits[pos - 1] {
                break;
            }
        }
        open = Some((bits[pos..].to_vec(), index));
    }
    Ok(Split {
        headers,
        frames,
        tail: open.map(|(b, _)| b).unwrap_or_default(),
    })
}

/// Whether `frame` holds all the bits its length prefix gives.
fn complete(frame: &[bool], w: u32) -> bool {
    frame.len() >= w as usize && read_field(frame, 0, w) == frame.len()
}

/// Bytes of a packet from its bits, zero padded to `block_align`.
pub fn packet_bytes(bits: &[bool], block_align: usize) -> Vec<u8> {
    let mut b = pack(bits);
    b.resize(block_align, 0);
    b
}

/// Reserved bits of every packet in the SDK 11 samples.
pub const RESERVED: u8 = 0b10;

/// Frames that start in one packet at most, after which Microsoft's
/// encoder pads the packet. In the samples, all at 44.1 kHz with frames of
/// 2048 samples, packets of silence and of 64 kbit/s streams hold 42
/// frames, 1.95 s; whether the limit counts frames or time is open.
pub const MAX_FRAMES_PER_PACKET: usize = 42;

/// Packet sequence numbers count modulo 16.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sequence(pub u8);

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum LayoutError {
    /// FFmpeg loses a frame that continues past the packet after the one
    /// it starts in, so no frame may be longer than a packet's payload.
    #[error("frame {frame} has {bits} bits; a packet carries at most {max}")]
    FrameTooLong {
        frame: usize,
        bits: usize,
        max: usize,
    },
}

/// Where frames go in packets of `block_align` bytes laid end to end.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    /// Per frame, the packet it starts in.
    pub starts: Vec<usize>,
    pub packets: usize,
}

impl Layout {
    /// The trailer bit of frame `i`: 0 when it is the last to start in its
    /// packet.
    pub fn trailer(&self, i: usize) -> bool {
        self.starts.get(i + 1) == Some(&self.starts[i])
    }
}

/// Lays out frames of `lens` bits: each packet's header, then the end of
/// the frame begun in the packet before, then frames, at most
/// `MAX_FRAMES_PER_PACKET` of them.
pub fn layout(cfg: &Config, lens: &[usize]) -> Result<Layout, LayoutError> {
    let header = 6 + cfg.log2_frame_size as usize;
    let size = cfg.block_align * 8;
    let max = size - header;
    let mut starts = Vec::with_capacity(lens.len());
    let mut packet = 0;
    let mut pos = header;
    let mut started = 0;
    for (frame, &len) in lens.iter().enumerate() {
        if len > max {
            return Err(LayoutError::FrameTooLong {
                frame,
                bits: len,
                max,
            });
        }
        if pos >= size || started == MAX_FRAMES_PER_PACKET {
            packet += 1;
            pos = header;
            started = 0;
        }
        starts.push(packet);
        started += 1;
        if pos + len > size {
            packet += 1;
            pos = header + len - (size - pos);
            started = 0;
        } else {
            pos += len;
        }
    }
    Ok(Layout {
        starts,
        packets: packet + 1,
    })
}

/// Writes frames into packets numbered from `first`, setting each frame's
/// trailer bit from `layout`.
pub fn join(cfg: &Config, frames: &[Bits], layout: &Layout, first: Sequence) -> Vec<Vec<u8>> {
    let w = cfg.log2_frame_size;
    let size = cfg.block_align * 8;
    let mut out = Vec::with_capacity(layout.packets);
    let mut cur: Bits = Vec::with_capacity(size);
    // Bits of the current frame still to place.
    let mut carry: Bits = Vec::new();
    let mut next = 0;
    for p in 0..layout.packets {
        cur.clear();
        let seq = (usize::from(first.0) + p) % 16;
        for i in (0..4).rev() {
            cur.push(seq >> i & 1 == 1);
        }
        cur.push(RESERVED & 2 != 0);
        cur.push(RESERVED & 1 != 0);
        for i in (0..w).rev() {
            cur.push(carry.len() >> i & 1 == 1);
        }
        let take = carry.len().min(size - cur.len());
        cur.extend(carry.drain(..take));
        while carry.is_empty() && next < frames.len() && layout.starts[next] == p {
            let mut f = frames[next].clone();
            *f.last_mut().expect("a frame ends with its trailer bit") = layout.trailer(next);
            let take = f.len().min(size - cur.len());
            cur.extend(&f[..take]);
            carry = f.split_off(take);
            next += 1;
        }
        out.push(packet_bytes(&cur, cfg.block_align));
    }
    out
}
