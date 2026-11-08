"""Generate a table crate's ffmpeg.rs from FFmpeg's table sources.

Usage: python3 -I tools/port_ffmpeg_tables.py SET FFMPEG_SRC_DIR OUT_RS

SET is vc1 (crates/vc1-tables), wma (crates/wma-tables) or wmapro
(crates/wma-tables/src/pro.rs).

Only purely numeric const arrays are ported; arrays with symbolic entries
are listed on stderr. The one preprocessor conditional around table data,
`#if B_FRACTION_DEN == 840` in vc1data.c, is resolved for the value FFmpeg
builds with (256). The output is LGPL-2.1-or-later like its source.
"""
import re
import subprocess
import sys

# Per set: the source files and, where a file holds far more than the set
# needs, the arrays to take from it.
SETS = {
    "vc1": [
        ("libavcodec/vc1_vlc_data.h", None),
        ("libavcodec/vc1acdata.h", None),
        ("libavcodec/vc1data.c", None),
        ("libavcodec/msmpeg4_vc1_data.c", None),
        ("libavcodec/wmv2data.c", None),
    ],
    "wma": [
        ("libavcodec/wmadata.h", None),
        ("libavcodec/wma_freqs.c", None),
        (
            "libavcodec/aactab.c",
            {"ff_aac_scalefactor_code", "ff_aac_scalefactor_bits"},
        ),
    ],
    "wmapro": [
        ("libavcodec/wmaprodata.h", None),
    ],
}

# Float arrays of the sources no crate uses: the codebook of WMA's LSP
# exponent coding, which the encoder does not write.
EXCLUDED = {"ff_wma_lsp_codebook"}

C_TYPES = {
    "uint8_t": "u8",
    "int8_t": "i8",
    "uint16_t": "u16",
    "int16_t": "i16",
    "uint32_t": "u32",
    "int32_t": "i32",
    "int": "i32",
    "float": "f32",
}

DECL = re.compile(
    r"^(?:static\s+)?const\s+(\w+)\s+(\w+)\s*((?:\[[^\]]*\]\s*)+)=\s*\{",
    re.M,
)
NUMBER = re.compile(r"-?(?:0[xX][0-9a-fA-F]+|\d+)")
FLOAT = re.compile(r"-?\d+\.\d*(?:[eE][-+]?\d+)?[fF]?")


B_FRACTION_DEN = 256
BFRACTION_IF = re.compile(
    r"#if B_FRACTION_DEN == 840(.*?)#else(.*?)#endif", re.S
)


def resolve_conditionals(src):
    def pick(m):
        return m.group(1) if B_FRACTION_DEN == 840 else m.group(2)

    return BFRACTION_IF.sub(pick, src)


def strip_comments(src):
    src = re.sub(r"/\*.*?\*/", " ", src, flags=re.S)
    return re.sub(r"//[^\n]*", " ", src)


def body_at(src, start):
    depth = 0
    for i in range(start, len(src)):
        if src[i] == "{":
            depth += 1
        elif src[i] == "}":
            depth -= 1
            if depth == 0:
                return src[start : i + 1]
    raise ValueError("unbalanced braces")


def parse_nested(body):
    """Turns a C initializer into nested lists of ints, or None if any
    element is symbolic."""
    tokens = re.findall(r"[{}]|[^{},\s]+", body)
    stack = [[]]
    for t in tokens:
        if t == "{":
            stack.append([])
        elif t == "}":
            done = stack.pop()
            stack[-1].append(done)
        elif NUMBER.fullmatch(t):
            stack[-1].append(int(t, 0))
        elif FLOAT.fullmatch(t):
            stack[-1].append(float(t.rstrip("fF")))
        else:
            return None
    return stack[0][0]


def shape(v):
    if isinstance(v, list):
        inner = [shape(x) for x in v]
        if any(i != inner[0] for i in inner):
            return None
        return [len(v)] + (inner[0] or [])
    return []


def pad(v, dims):
    """Zero-fills short rows, as C does for partial initializers."""
    if not dims:
        return v
    v = list(v) + [0 if len(dims) == 1 else [] for _ in range(dims[0] - len(v))]
    return [pad(x, dims[1:]) for x in v]


UNSIGNED_BITS = {"u8": 8, "u16": 16, "u32": 32}


def wrap_unsigned(v, rtype):
    """Applies C's modular conversion, e.g. -1 stored in a uint8_t is 255."""
    if isinstance(v, list):
        return [wrap_unsigned(x, rtype) for x in v]
    bits = UNSIGNED_BITS.get(rtype)
    return v % (1 << bits) if bits else v


def rust_type(base, dims):
    t = base
    for d in reversed(dims):
        t = f"[{t}; {d}]"
    return t


def rust_scalar(x, rtype):
    """Integer-valued entries of a float table need a decimal point."""
    if rtype == "f32":
        return repr(float(x))
    return str(x)


def rust_value(v, depth=0, rtype=None):
    if not isinstance(v, list):
        return rust_scalar(v, rtype)
    if v and not isinstance(v[0], list):
        items = [rust_scalar(x, rtype) for x in v]
        lines, line = [], ""
        for it in items:
            if len(line) + len(it) + 2 > 92:
                lines.append(line.rstrip())
                line = ""
            line += it + ", "
        lines.append(line.rstrip())
        ind = "    " * (depth + 1)
        return "[\n" + "".join(ind + ln + "\n" for ln in lines) + "    " * depth + "]"
    ind = "    " * (depth + 1)
    inner = ",\n".join(ind + rust_value(x, depth + 1, rtype) for x in v)
    return "[\n" + inner + ",\n" + "    " * depth + "]"


def resolve_dims(declared, value, defines):
    dims = []
    for d in re.findall(r"\[([^\]]*)\]", declared):
        d = d.strip()
        if not d:
            dims.append(None)
        elif d.isdigit():
            dims.append(int(d))
        elif d in defines:
            dims.append(defines[d])
        else:
            dims.append(None)
    actual = shape_max(value)
    return [a if d is None else d for d, a in zip(dims, actual)]


def shape_max(v):
    if not isinstance(v, list):
        return []
    subs = [shape_max(x) for x in v]
    deepest = max((len(s) for s in subs), default=0)
    out = [len(v)]
    for i in range(deepest):
        out.append(max(s[i] for s in subs if len(s) > i))
    return out


def main():
    set_name, root, out_path = sys.argv[1], sys.argv[2], sys.argv[3]
    sources = SETS[set_name]
    commit = subprocess.run(
        ["git", "-C", root, "describe", "--tags", "--always"],
        capture_output=True,
        text=True,
        check=True,
    ).stdout.strip()
    defines = {"AC_MODES": 8, "WMV1_SCANTABLE_COUNT": 4}
    out = [
        "// Generated by tools/port_ffmpeg_tables.py from FFmpeg " + commit + ".",
        "// Do not edit. Licensed LGPL-2.1-or-later like its FFmpeg sources:",
    ]
    for s, _ in sources:
        out.append("//   " + s)
    out.append("")
    tables = {}
    for rel, wanted in sources:
        src = strip_comments(resolve_conditionals(open(f"{root}/{rel}").read()))
        for m in DECL.finditer(src):
            ctype, name, dims = m.group(1), m.group(2), m.group(3)
            if (
                ctype not in C_TYPES
                or name in EXCLUDED
                or (wanted is not None and name not in wanted)
            ):
                continue
            value = parse_nested(body_at(src, m.end() - 1))
            if value is None:
                print(f"skipped symbolic array {name} in {rel}", file=sys.stderr)
                continue
            full = resolve_dims(dims, value, defines)
            value = wrap_unsigned(pad(value, full), C_TYPES[ctype])
            rname = re.sub(r"^ff_", "", name).upper()
            if rname in tables:
                raise SystemExit(f"{name} defined twice")
            rtype = C_TYPES[ctype]
            tables[rname] = (
                f"pub static {rname}: {rust_type(rtype, full)} = "
                f"{rust_value(value, rtype=rtype)};\n"
            )
    out.extend(tables.values())
    with open(out_path, "w") as f:
        f.write("\n".join(out).rstrip() + "\n")


if __name__ == "__main__":
    main()
