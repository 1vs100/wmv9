/* Genuine decode oracle: WMV3 frames -> I420 via WMVDECOD.DLL's
 * IMediaObject path, with post-processing off and one thread.
 * Build: x86_64-w64-mingw32-gcc -O2 -Wall -o dmo_decode.exe
 *   dmo_decode.c -lole32 -loleaut32 -ldmoguids -luuid -lmsdmo
 * Run under ~/wmv9-oracle with WINEDLLOVERRIDES=wmvdecod=n; without it
 * Wine loads its own wmvdecod.dll, which decodes through GStreamer's
 * FFmpeg wrapper. The harness refuses to run on that module.
 *   dmo_decode.exe FRAMES SEQHDR WIDTH HEIGHT OUT.yuv
 * FRAMES and SEQHDR come from `cargo run --example extract_video`.
 * OUT.yuv holds the output frames in output order, each WIDTH*HEIGHT*3/2
 * bytes of planar Y, U, V.
 */
#define COBJMACROS
#define INITGUID
#include <windows.h>
#include <ocidl.h>
#include <dmo.h>
#include <dmort.h>
#include <amvideo.h>
#include <uuids.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <limits.h>
#include "dmo_buffer.h"

DEFINE_GUID(CLSID_CWMVDecMediaObject, 0x82d353df, 0x90bd, 0x4382, 0x8b, 0xc2,
            0x3f, 0x61, 0x92, 0xb7, 0x6e, 0x34);
DEFINE_GUID(SUBTYPE_WMV3, 0x33564d57, 0x0000, 0x0010, 0x80, 0x00, 0x00, 0xaa,
            0x00, 0x38, 0x9b, 0x71);

#define FOURCC_WMV3 0x33564D57
#define FOURCC_I420 0x30323449
#define FOURCC_IYUV 0x56555949
#define FOURCC_YV12 0x32315659
#define FRAME_DURATION 333333

#define CHECK(expr)                                                       \
    do {                                                                  \
        HRESULT hr_ = (expr);                                             \
        if (FAILED(hr_)) {                                                \
            fprintf(stderr, "%s -> 0x%08lx\n", #expr, (unsigned long)hr_); \
            exit(1);                                                      \
        }                                                                 \
    } while (0)

static BYTE *read_file(const char *path, long *len)
{
    FILE *f = fopen(path, "rb");
    BYTE *p;
    if (!f) {
        fprintf(stderr, "cannot open %s\n", path);
        exit(1);
    }
    fseek(f, 0, SEEK_END);
    *len = ftell(f);
    fseek(f, 0, SEEK_SET);
    p = malloc(*len ? *len : 1);
    if (fread(p, 1, *len, f) != (size_t)*len) {
        fprintf(stderr, "short read %s\n", path);
        exit(1);
    }
    fclose(f);
    return p;
}

/* Refuses a decoder object implemented by a Wine module. The owner is
 * found from the object's code, so a CLSID registered elsewhere cannot
 * pass on the strength of an unrelated native wmvdecod.dll. Wine writes
 * "Wine builtin DLL" into the DOS stub of its own modules. */
static void require_native_decoder(IMediaObject *mo)
{
    static const char marker[] = "Wine builtin DLL";
    HMODULE owner = NULL;
    const BYTE *base;
    if (!GetModuleHandleExW(GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS |
                                GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
                            (LPCWSTR)mo->lpVtbl->ProcessInput, &owner)) {
        fprintf(stderr, "cannot find the module of the decoder object\n");
        exit(1);
    }
    base = (const BYTE *)owner;
    for (int i = 0; i + (int)sizeof(marker) - 1 <= 0x80; i++) {
        if (!memcmp(base + i, marker, sizeof(marker) - 1)) {
            fprintf(stderr, "the decoder object comes from a Wine built-in "
                            "module; set WINEDLLOVERRIDES=wmvdecod=n\n");
            exit(1);
        }
    }
}

/* Sets a named property through IPropertyBag and reads it back, since a
 * write can return S_OK without taking effect. */
static void set_property(IMediaObject *mo, const WCHAR *name, LONG value)
{
    IPropertyBag *bag;
    VARIANT v;
    CHECK(IMediaObject_QueryInterface(mo, &IID_IPropertyBag, (void **)&bag));
    VariantInit(&v);
    v.vt = VT_I4;
    v.lVal = value;
    CHECK(IPropertyBag_Write(bag, name, &v));
    VariantInit(&v);
    CHECK(IPropertyBag_Read(bag, name, &v, NULL));
    if (v.vt != VT_I4 || v.lVal != value) {
        fprintf(stderr, "property %ls reads back as vt %d value %ld\n", name,
                v.vt, (long)v.lVal);
        exit(1);
    }
    IPropertyBag_Release(bag);
}

static void set_input_type(IMediaObject *mo, const BYTE *priv, long privlen,
                           int w, int h)
{
    DMO_MEDIA_TYPE mt;
    DWORD size = sizeof(VIDEOINFOHEADER) + privlen;
    VIDEOINFOHEADER *vih = calloc(1, size);

    vih->AvgTimePerFrame = FRAME_DURATION;
    vih->bmiHeader.biSize = sizeof(BITMAPINFOHEADER) + privlen;
    vih->bmiHeader.biWidth = w;
    vih->bmiHeader.biHeight = h;
    vih->bmiHeader.biPlanes = 1;
    vih->bmiHeader.biBitCount = 24;
    vih->bmiHeader.biCompression = FOURCC_WMV3;
    memcpy((BYTE *)vih + sizeof(VIDEOINFOHEADER), priv, privlen);

    memset(&mt, 0, sizeof(mt));
    mt.majortype = MEDIATYPE_Video;
    mt.subtype = SUBTYPE_WMV3;
    mt.bTemporalCompression = TRUE;
    mt.formattype = FORMAT_VideoInfo;
    mt.cbFormat = size;
    mt.pbFormat = (BYTE *)vih;
    CHECK(IMediaObject_SetInputType(mo, 0, &mt, 0));
    free(vih);
}

/* Picks a planar 4:2:0 output type; returns its FOURCC. */
static DWORD set_output_type(IMediaObject *mo)
{
    for (DWORD i = 0;; i++) {
        DMO_MEDIA_TYPE mt;
        DWORD fcc;
        if (IMediaObject_GetOutputType(mo, 0, i, &mt) != S_OK)
            break;
        fcc = mt.subtype.Data1;
        if (fcc == FOURCC_I420 || fcc == FOURCC_IYUV || fcc == FOURCC_YV12) {
            CHECK(IMediaObject_SetOutputType(mo, 0, &mt, 0));
            MoFreeMediaType(&mt);
            return fcc;
        }
        MoFreeMediaType(&mt);
    }
    fprintf(stderr, "no planar 4:2:0 output type\n");
    exit(1);
}

/* Writes at most `keep` of the available output frames; returns how many
 * it wrote. */
static int drain(IMediaObject *mo, DWORD outmax, DWORD fcc, int w, int h,
                 FILE *out, int keep)
{
    int n = 0;
    DWORD frame = (DWORD)(w * h * 3 / 2);
    for (;;) {
        DMO_OUTPUT_DATA_BUFFER bdo;
        DWORD status = 0;
        BYTE *p;
        DWORD l;
        HRESULT hr;

        memset(&bdo, 0, sizeof(bdo));
        bdo.pBuffer = newbuf(outmax);
        hr = IMediaObject_ProcessOutput(mo, 0, 1, &bdo, &status);
        bdo.pBuffer->lpVtbl->GetBufferAndLength(bdo.pBuffer, &p, &l);
        if (hr == S_OK && l && n < keep) {
            if (l != frame) {
                fprintf(stderr, "output sample is %lu bytes, expected %lu\n",
                        (unsigned long)l, (unsigned long)frame);
                exit(1);
            }
            if (fcc == FOURCC_YV12) {
                DWORD y = (DWORD)(w * h), c = y / 4;
                fwrite(p, 1, y, out);
                fwrite(p + y + c, 1, c, out);
                fwrite(p + y, 1, c, out);
            } else {
                fwrite(p, 1, l, out);
            }
            n++;
        } else if (FAILED(hr)) {
            fprintf(stderr, "ProcessOutput -> 0x%08lx\n", (unsigned long)hr);
            exit(1);
        }
        bdo.pBuffer->lpVtbl->Release(bdo.pBuffer);
        if (hr != S_OK ||
            !(bdo.dwStatus & DMO_OUTPUT_DATA_BUFFERF_INCOMPLETE))
            break;
    }
    return n;
}

int main(int argc, char **argv)
{
    IMediaObject *mo;
    DWORD outmax, align, fcc;
    long flen, plen, off = 0;
    BYTE *frames, *priv;
    int w, h, in = 0, produced = 0;
    FILE *out;

    if (argc != 6) {
        fprintf(stderr, "usage: dmo_decode FRAMES SEQHDR WIDTH HEIGHT OUT\n");
        return 1;
    }
    frames = read_file(argv[1], &flen);
    priv = read_file(argv[2], &plen);
    w = atoi(argv[3]);
    h = atoi(argv[4]);

    CoInitialize(NULL);
    CHECK(CoCreateInstance(&CLSID_CWMVDecMediaObject, NULL,
                           CLSCTX_INPROC_SERVER, &IID_IMediaObject,
                           (void **)&mo));
    require_native_decoder(mo);
    /* Post-processing would add deblocking and deringing on top of the
     * standard reconstruction, adapted to CPU load when left on. */
    set_property(mo, L"_POSTPROCESSMODE", 0);
    set_property(mo, L"_NUMTHREADSDEC", 1);
    set_input_type(mo, priv, plen, w, h);
    fcc = set_output_type(mo);
    CHECK(IMediaObject_GetOutputSizeInfo(mo, 0, &outmax, &align));
    CHECK(IMediaObject_AllocateStreamingResources(mo));

    out = fopen(argv[5], "wb");
    if (!out) {
        fprintf(stderr, "cannot create %s\n", argv[5]);
        return 1;
    }
    while (off + 8 <= flen) {
        uint32_t len, flags;
        IMediaBuffer *inb;
        BYTE *p;
        HRESULT hr;

        memcpy(&len, frames + off, 4);
        memcpy(&flags, frames + off + 4, 4);
        off += 8;
        if (off + (long)len > flen) {
            fprintf(stderr, "truncated record %d\n", in);
            return 1;
        }
        inb = newbuf(len ? len : 1);
        inb->lpVtbl->GetBufferAndLength(inb, &p, NULL);
        memcpy(p, frames + off, len);
        inb->lpVtbl->SetLength(inb, len);
        off += len;
        hr = IMediaObject_ProcessInput(
            mo, 0, inb,
            DMO_INPUT_DATA_BUFFERF_TIME | DMO_INPUT_DATA_BUFFERF_TIMELENGTH |
                ((flags & 1) ? DMO_INPUT_DATA_BUFFERF_SYNCPOINT : 0),
            (REFERENCE_TIME)in * FRAME_DURATION, FRAME_DURATION);
        inb->lpVtbl->Release(inb);
        if (FAILED(hr)) {
            fprintf(stderr, "frame %d ProcessInput -> 0x%08lx\n", in,
                    (unsigned long)hr);
            return 1;
        }
        in++;
        produced += drain(mo, outmax, fcc, w, h, out, INT_MAX);
    }
    /* The decoder releases a held reference picture only when the next
     * one arrives; Discontinuity and empty input do not release it. Feed
     * the first (key) frame again to push out held pictures, and keep
     * nothing the extra frame itself produces. */
    if (produced < in) {
        uint32_t len;
        IMediaBuffer *inb;
        BYTE *p;
        memcpy(&len, frames, 4);
        inb = newbuf(len ? len : 1);
        inb->lpVtbl->GetBufferAndLength(inb, &p, NULL);
        memcpy(p, frames + 8, len);
        inb->lpVtbl->SetLength(inb, len);
        CHECK(IMediaObject_ProcessInput(
            mo, 0, inb,
            DMO_INPUT_DATA_BUFFERF_SYNCPOINT | DMO_INPUT_DATA_BUFFERF_TIME |
                DMO_INPUT_DATA_BUFFERF_TIMELENGTH,
            (REFERENCE_TIME)in * FRAME_DURATION, FRAME_DURATION));
        inb->lpVtbl->Release(inb);
        produced += drain(mo, outmax, fcc, w, h, out, in - produced);
    }
    fclose(out);
    printf("%d frames in, %d frames out, output %.4s\n", in, produced,
           (char *)&fcc);
    IMediaObject_FreeStreamingResources(mo);
    IMediaObject_Release(mo);
    CoUninitialize();
    return 0;
}
