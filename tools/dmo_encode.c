/* Genuine encode oracle: YUY2 320x240x30 -> WMV3 via the IMediaObject path.
 * Build: x86_64-w64-mingw32-gcc -O2 -Wall -o dmo_encode.exe
 *   dmo_encode.c -lole32 -lpropsys -lmfplat -ldmoguids -luuid -lmsdmo
 * Run under ~/wmv9-oracle with WINEDLLOVERRIDES=wmvencod=n.
 * Writes frames.raw (one length+flags record per output sample) and
 * seqhdr.bin (codec private data).
 */
#define COBJMACROS
#define INITGUID
#include <windows.h>
#include <dmo.h>
#include <dmort.h>
#include <amvideo.h>
#include <uuids.h>
#include <mfapi.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "wmv9keys.h"
#include "dmo_buffer.h"

#define W 320
#define H 240
#define NFRAMES 30
#define FRAME_DURATION 333333
#define BITRATE 500000
#define FOURCC_WMV3 0x33564D57

/* IWMCodecPrivateData (wmcodecdsp.h); not in mingw headers. */
DEFINE_GUID(IID_IWMCodecPrivateData, 0x73f0be8e, 0x57f7, 0x4f01, 0xaa, 0x66,
            0x9f, 0x57, 0x34, 0x0c, 0xfe, 0x0e);

typedef struct IWMCodecPrivateData IWMCodecPrivateData;
typedef struct {
    HRESULT(STDMETHODCALLTYPE *QueryInterface)(IWMCodecPrivateData *,
                                               REFIID, void **);
    ULONG(STDMETHODCALLTYPE *AddRef)(IWMCodecPrivateData *);
    ULONG(STDMETHODCALLTYPE *Release)(IWMCodecPrivateData *);
    HRESULT(STDMETHODCALLTYPE *SetPartialOutputType)(IWMCodecPrivateData *,
                                                     DMO_MEDIA_TYPE *);
    HRESULT(STDMETHODCALLTYPE *GetPrivateData)(IWMCodecPrivateData *,
                                               BYTE *, ULONG *);
} IWMCodecPrivateDataVtbl;
struct IWMCodecPrivateData {
    const IWMCodecPrivateDataVtbl *lpVtbl;
};

/* Y1 = 255 - Y0 on purpose: a worst-case luma pattern. The reference set
 * in ~/wmv9-decomp/ref/ was encoded from it. */
static void fill_yuy2(BYTE *p, int frame)
{
    int x, y;
    int bx = (frame * 8) % (W - 32);
    for (y = 0; y < H; y++)
        for (x = 0; x < W; x += 2) {
            BYTE yy = (BYTE)((x + y + frame * 2) & 0xff);
            if (y > H / 4 && y < 3 * H / 4 && x > bx && x < bx + 32)
                yy = 200;
            *p++ = yy;
            *p++ = 128;
            *p++ = (BYTE)(255 - yy);
            *p++ = 128;
        }
}

#define CHECK(expr)                                                       \
    do {                                                                  \
        HRESULT hr_ = (expr);                                             \
        printf("%s -> 0x%08lx\n", #expr, (unsigned long)hr_);             \
        if (FAILED(hr_))                                                  \
            exit(1);                                                      \
    } while (0)

static void set_input_type(IMediaObject *mo)
{
    DMO_MEDIA_TYPE mt;
    VIDEOINFOHEADER vih;

    memset(&vih, 0, sizeof(vih));
    vih.AvgTimePerFrame = FRAME_DURATION;
    vih.bmiHeader.biSize = sizeof(BITMAPINFOHEADER);
    vih.bmiHeader.biWidth = W;
    vih.bmiHeader.biHeight = H;
    vih.bmiHeader.biPlanes = 1;
    vih.bmiHeader.biBitCount = 16;
    vih.bmiHeader.biCompression = MAKEFOURCC('Y', 'U', 'Y', '2');
    vih.bmiHeader.biSizeImage = W * H * 2;

    memset(&mt, 0, sizeof(mt));
    mt.majortype = MEDIATYPE_Video;
    mt.subtype = MEDIASUBTYPE_YUY2;
    mt.formattype = FORMAT_VideoInfo;
    mt.cbFormat = sizeof(vih);
    mt.pbFormat = (BYTE *)&vih;
    CHECK(IMediaObject_SetInputType(mo, 0, &mt, 0));
}

static void set_fourcc(IMediaObject *mo)
{
    IPropertyStore *ps;
    PROPVARIANT v;

    CHECK(IMediaObject_QueryInterface(mo, &IID_IPropertyStore, (void **)&ps));
    PropVariantInit(&v);
    v.vt = VT_I4; /* VT_UI4 is rejected. */
    v.lVal = FOURCC_WMV3;
    CHECK(IPropertyStore_SetValue(ps, &MFPKEY_FOURCC, &v));
    IPropertyStore_Release(ps);
}

/* The encoder rejects any output type whose format block does not end with
 * the exact codec private data it would generate. Obtain it through
 * IWMCodecPrivateData and append it to the VIDEOINFOHEADER. */
static void set_output_type(IMediaObject *mo, const char *hdrpath)
{
    DMO_MEDIA_TYPE mt, full;
    IWMCodecPrivateData *pd;
    VIDEOINFOHEADER *vih;
    BYTE priv[1024];
    ULONG privlen = sizeof(priv);
    FILE *f;

    memset(&mt, 0, sizeof(mt));
    CHECK(IMediaObject_GetOutputType(mo, 0, 0, &mt));
    if (mt.subtype.Data1 != FOURCC_WMV3 || mt.cbFormat != sizeof(*vih)) {
        printf("unexpected output type 0\n");
        exit(1);
    }
    vih = (VIDEOINFOHEADER *)mt.pbFormat;
    vih->dwBitRate = BITRATE;

    CHECK(IMediaObject_QueryInterface(mo, &IID_IWMCodecPrivateData,
                                      (void **)&pd));
    CHECK(pd->lpVtbl->SetPartialOutputType(pd, &mt));
    CHECK(pd->lpVtbl->GetPrivateData(pd, priv, &privlen));
    pd->lpVtbl->Release(pd);
    printf("private data (%lu):", (unsigned long)privlen);
    for (ULONG i = 0; i < privlen; i++)
        printf(" %02x", priv[i]);
    printf("\n");
    f = fopen(hdrpath, "wb");
    if (f) {
        fwrite(priv, 1, privlen, f);
        fclose(f);
    }

    full = mt;
    full.cbFormat = mt.cbFormat + privlen;
    full.pbFormat = malloc(full.cbFormat);
    memcpy(full.pbFormat, mt.pbFormat, mt.cbFormat);
    memcpy(full.pbFormat + mt.cbFormat, priv, privlen);
    ((VIDEOINFOHEADER *)full.pbFormat)->bmiHeader.biSize =
        sizeof(BITMAPINFOHEADER) + privlen;
    MoFreeMediaType(&mt);

    CHECK(IMediaObject_SetOutputType(mo, 0, &full, 0));
    free(full.pbFormat);
}

/* Record layout: u32 length, u32 flags (bit0 = sync point), payload. */
static void write_record(FILE *f, const BYTE *p, DWORD len, DWORD status)
{
    uint32_t hdr[2];
    hdr[0] = len;
    hdr[1] = (status & DMO_OUTPUT_DATA_BUFFERF_SYNCPOINT) ? 1 : 0;
    fwrite(hdr, sizeof(hdr), 1, f);
    fwrite(p, 1, len, f);
}

static int drain(IMediaObject *mo, DWORD outmax, FILE *f)
{
    int n = 0;
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
        if (hr == S_OK && l) {
            printf("  out %lu bytes%s\n", (unsigned long)l,
                   (bdo.dwStatus & DMO_OUTPUT_DATA_BUFFERF_SYNCPOINT) ? " key"
                                                                       : "");
            write_record(f, p, l, bdo.dwStatus);
            n++;
        } else if (FAILED(hr)) {
            printf("  ProcessOutput -> 0x%08lx\n", (unsigned long)hr);
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
    const char *outpath = argc > 1 ? argv[1] : "frames.raw";
    const char *hdrpath = argc > 2 ? argv[2] : "seqhdr.bin";
    IMediaObject *mo;
    DWORD outmax, align;
    int produced = 0;
    FILE *f;

    CoInitialize(NULL);
    CHECK(CoCreateInstance(&CLSID_CWMV9EncMediaObject, NULL,
                           CLSCTX_INPROC_SERVER, &IID_IMediaObject,
                           (void **)&mo));
    set_input_type(mo);
    set_fourcc(mo);
    set_output_type(mo, hdrpath);
    CHECK(IMediaObject_GetOutputSizeInfo(mo, 0, &outmax, &align));
    printf("output sizeinfo: %lu\n", (unsigned long)outmax);
    CHECK(IMediaObject_AllocateStreamingResources(mo));

    f = fopen(outpath, "wb");
    if (!f) {
        printf("fopen %s failed\n", outpath);
        return 1;
    }
    for (int i = 0; i < NFRAMES; i++) {
        IMediaBuffer *inb = newbuf(W * H * 2);
        BYTE *p;
        HRESULT hr;

        inb->lpVtbl->GetBufferAndLength(inb, &p, NULL);
        fill_yuy2(p, i);
        inb->lpVtbl->SetLength(inb, W * H * 2);
        hr = IMediaObject_ProcessInput(mo, 0, inb,
                                       DMO_INPUT_DATA_BUFFERF_TIME |
                                           DMO_INPUT_DATA_BUFFERF_TIMELENGTH,
                                       (REFERENCE_TIME)i * FRAME_DURATION,
                                       FRAME_DURATION);
        inb->lpVtbl->Release(inb);
        printf("frame %d ProcessInput -> 0x%08lx\n", i, (unsigned long)hr);
        if (FAILED(hr))
            return 1;
        produced += drain(mo, outmax, f);
    }
    CHECK(IMediaObject_Discontinuity(mo, 0));
    produced += drain(mo, outmax, f);
    fclose(f);
    printf("wrote %d samples to %s\n", produced, outpath);
    if (produced != NFRAMES) {
        printf("expected %d samples\n", NFRAMES);
        return 1;
    }
    IMediaObject_FreeStreamingResources(mo);
    IMediaObject_Release(mo);
    CoUninitialize();
    return 0;
}
