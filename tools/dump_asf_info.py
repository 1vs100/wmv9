"""Print what an ASF file says about its origin: codec list, metadata
attributes (WMFSDKVersion, WM/ToolName, IsVBR and kin) and the decoded
WMV3 sequence header (STRUCT_C) of the video stream.

usage: dump_asf_info.py FILE..."""
import struct
import sys
import uuid


def guid(s):
    return uuid.UUID(s).bytes_le


HEADER = guid("75B22630-668E-11CF-A6D9-00AA0062CE6C")
EXT_CONTENT = guid("D2D0A440-E307-11D2-97F0-00A0C95EA850")
CODEC_LIST = guid("86D15240-311D-11D0-A3A4-00A0C90348F6")
STREAM_PROPS = guid("B7DC0791-A9B7-11CF-8EE6-00C00C205365")
VIDEO_MEDIA = guid("BC19EFC0-5B4D-11CF-A8FD-00805F5C442B")
HEADER_EXT = guid("5FBF03B5-A92E-11CF-8EE3-00C00C205365")
METADATA = guid("C5F8CBEA-5BAF-4877-8467-AA8C44FA4CCA")
METADATA_LIB = guid("44231C94-9498-49D1-A141-1D134E457054")

STRUCT_C = [
    ("profile", 2), ("res_y411", 1), ("res_sprite", 1), ("frmrtq_postproc", 3),
    ("bitrtq_postproc", 5), ("loop_filter", 1), ("res_x8", 1), ("multires", 1),
    ("res_fasttx", 1), ("fastuvmc", 1), ("extended_mv", 1), ("dquant", 2),
    ("vstransform", 1), ("res_transtab", 1), ("overlap", 1), ("syncmarker", 1),
    ("rangered", 1), ("max_b_frames", 3), ("quantizer", 2), ("finterpflag", 1),
    ("res_rtm_flag", 1),
]


def utf16(b):
    return b.decode("utf-16le").rstrip("\x00")


def value(kind, b):
    """ASF attribute value by data type (0 string, 1 bytes, 2 bool,
    3 dword, 4 qword, 5 word, 6 GUID)."""
    if kind == 0:
        return utf16(b)
    if kind == 1:
        return b.hex()
    if kind == 2:
        return bool(int.from_bytes(b, "little"))
    if kind in (3, 4, 5):
        return int.from_bytes(b, "little")
    return str(uuid.UUID(bytes_le=b))


def struct_c(b):
    v = int.from_bytes(b[:4], "big")
    out, pos = [], 0
    for name, width in STRUCT_C:
        out.append(f"{name}={(v >> (32 - pos - width)) & ((1 << width) - 1)}")
        pos += width
    return " ".join(out)


def objects(d, off, end):
    while off < end:
        size = struct.unpack_from("<Q", d, off + 16)[0]
        yield d[off:off + 16], off, d[off + 24:off + size]
        off += size


def info(d):
    if d[:16] != HEADER:
        raise ValueError("not an ASF file")
    header_size = struct.unpack_from("<Q", d, 16)[0]
    out = {}

    def scan(start, end):
        for gid, off, body in objects(d, start, end):
            if gid == EXT_CONTENT:
                p = 2
                for _ in range(struct.unpack_from("<H", body, 0)[0]):
                    n = struct.unpack_from("<H", body, p)[0]
                    name = utf16(body[p + 2:p + 2 + n])
                    p += 2 + n
                    kind, n = struct.unpack_from("<HH", body, p)
                    out[name] = value(kind, body[p + 4:p + 4 + n])
                    p += 4 + n
            elif gid in (METADATA, METADATA_LIB):
                p = 2
                for _ in range(struct.unpack_from("<H", body, 0)[0]):
                    _, stream, n, kind, size = struct.unpack_from("<HHHHI", body, p)
                    p += 12
                    name = utf16(body[p:p + n])
                    p += n
                    out[f"stream{stream}:{name}"] = value(kind, body[p:p + size])
                    p += size
            elif gid == CODEC_LIST:
                p = 20
                for k in range(struct.unpack_from("<I", body, 16)[0]):
                    p += 2
                    n = struct.unpack_from("<H", body, p)[0] * 2
                    name = utf16(body[p + 2:p + 2 + n])
                    p += 2 + n
                    n = struct.unpack_from("<H", body, p)[0] * 2
                    desc = utf16(body[p + 2:p + 2 + n])
                    p += 2 + n
                    p += 2 + struct.unpack_from("<H", body, p)[0]
                    out[f"codec{k}"] = f"{name} | {desc}"
            elif gid == STREAM_PROPS and body[:16] == VIDEO_MEDIA:
                n = struct.unpack_from("<I", body, 40)[0]
                ts = body[54:54 + n]
                width, height = struct.unpack_from("<II", ts, 0)
                bmi = ts[11:]
                out["video"] = f"{width}x{height} {bmi[16:20].decode('ascii', 'replace')}"
                priv = bmi[40:]
                out["seqhdr"] = priv.hex()
                if bmi[16:20] == b"WMV3" and len(priv) >= 4:
                    out["struct_c"] = struct_c(priv)
            elif gid == HEADER_EXT:
                scan(off + 24 + 22, off + 24 + len(body))

    scan(30, header_size)
    return out


def main():
    if len(sys.argv) < 2:
        sys.exit(__doc__)
    for path in sys.argv[1:]:
        print(f"== {path}")
        with open(path, "rb") as f:
            for k, v in info(f.read()).items():
                print(f"  {k}: {ascii(v) if isinstance(v, str) else v}")


if __name__ == "__main__":
    main()
