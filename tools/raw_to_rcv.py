"""Wrap dmo_encode output (frames.raw + seqhdr.bin) in an RCV container
(SMPTE 421M Annex L) so FFmpeg's vc1test demuxer can decode it."""
import struct
import sys


def read_records(path):
    with open(path, "rb") as f:
        data = f.read()
    off = 0
    while off < len(data):
        length, flags = struct.unpack_from("<II", data, off)
        off += 8
        yield data[off:off + length], bool(flags & 1)
        off += length


def main():
    if len(sys.argv) != 7:
        sys.exit("usage: raw_to_rcv.py FRAMES SEQHDR OUT WIDTH HEIGHT FPS")
    raw, hdr, out, width, height, fps = sys.argv[1:7]
    with open(hdr, "rb") as f:
        priv = f.read()
    # WMV3 private data is STRUCT_C plus one zero pad byte.
    if len(priv) != 5 or priv[4] != 0:
        sys.exit("unexpected private data: " + priv.hex())
    seq = priv[:4]
    frames = list(read_records(raw))
    with open(out, "wb") as f:
        f.write(struct.pack("<I", len(frames) | (0xC5 << 24)))
        f.write(struct.pack("<I", 4))
        f.write(seq)
        f.write(struct.pack("<II", int(height), int(width)))
        f.write(struct.pack("<I", 0xC))
        f.write(bytes(8))
        f.write(struct.pack("<I", int(fps)))
        for i, (payload, key) in enumerate(frames):
            size = len(payload) | (0x80000000 if key else 0)
            f.write(struct.pack("<II", size, round(i * 1000 / int(fps))))
            f.write(payload)


if __name__ == "__main__":
    main()
