/* Genuine decode oracle: WMA (Standard and Pro) media objects -> PCM via
 * WMADMOD.DLL's IMediaObject path.
 * Build: x86_64-w64-mingw32-gcc -O2 -Wall -o wma_decode.exe
 *   wma_decode.c -lole32 -loleaut32 -ldmoguids -luuid -lmsdmo
 * Run under ~/wmv9-oracle with WINEDLLOVERRIDES=wmadmod=n; without it
 * Wine loads its own wmadmod.dll, which decodes through GStreamer's FFmpeg
 * wrapper. The harness refuses to run on that module.
 *   wma_decode.exe OBJECTS FORMAT OUT.pcm
 * OBJECTS and FORMAT come from `cargo run --example extract_audio`.
 * OUT.pcm holds the output samples, interleaved, in the output format the
 * harness prints: 16-bit PCM when the decoder offers it.
 */
#define COBJMACROS
#define INITGUID
#include <windows.h>
#include <mmreg.h>
#include <dmo.h>
#include <dmort.h>
#include <uuids.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "dmo_buffer.h"

DEFINE_GUID(CLSID_CWMADecMediaObject, 0x2eeb4adf, 0x4578, 0x4d10, 0xbc, 0xa7,
            0xbb, 0x95, 0x5f, 0x56, 0x32, 0x0a);

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

/* Refuses a decoder object implemented by a Wine module: Wine writes
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
                            "module; set WINEDLLOVERRIDES=wmadmod=n\n");
            exit(1);
        }
    }
}

static void set_input_type(IMediaObject *mo, BYTE *wfx, long len)
{
    DMO_MEDIA_TYPE mt;
    WORD tag;
    memcpy(&tag, wfx, 2);
    memset(&mt, 0, sizeof(mt));
    mt.majortype = MEDIATYPE_Audio;
    /* Audio subtypes are the format tag in the base GUID. */
    mt.subtype = MEDIASUBTYPE_PCM;
    mt.subtype.Data1 = tag;
    mt.bFixedSizeSamples = TRUE;
    mt.formattype = FORMAT_WaveFormatEx;
    mt.cbFormat = (ULONG)len;
    mt.pbFormat = wfx;
    CHECK(IMediaObject_SetInputType(mo, 0, &mt, 0));
}

/* Picks 16-bit PCM output if offered, else the first type; prints it. */
static WORD set_output_type(IMediaObject *mo)
{
    DMO_MEDIA_TYPE mt, first;
    int have_first = 0;
    for (DWORD i = 0;; i++) {
        WAVEFORMATEX *w;
        if (IMediaObject_GetOutputType(mo, 0, i, &mt) != S_OK)
            break;
        w = (WAVEFORMATEX *)mt.pbFormat;
        printf("output type %lu: tag %#x, %u channels, %lu Hz, %u bits\n",
               (unsigned long)i, w->wFormatTag, w->nChannels,
               (unsigned long)w->nSamplesPerSec, w->wBitsPerSample);
        if (w->wBitsPerSample == 16 &&
            (w->wFormatTag == WAVE_FORMAT_PCM ||
             w->wFormatTag == WAVE_FORMAT_EXTENSIBLE)) {
            WORD block = w->nBlockAlign;
            CHECK(IMediaObject_SetOutputType(mo, 0, &mt, 0));
            MoFreeMediaType(&mt);
            if (have_first)
                MoFreeMediaType(&first);
            return block;
        }
        if (!have_first) {
            first = mt;
            have_first = 1;
        } else {
            MoFreeMediaType(&mt);
        }
    }
    if (!have_first) {
        fprintf(stderr, "no output type\n");
        exit(1);
    }
    CHECK(IMediaObject_SetOutputType(mo, 0, &first, 0));
    {
        WORD block = ((WAVEFORMATEX *)first.pbFormat)->nBlockAlign;
        MoFreeMediaType(&first);
        return block;
    }
}

/* Writes all available output; returns the bytes written. */
static long drain(IMediaObject *mo, DWORD outmax, FILE *out)
{
    long total = 0;
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
            fwrite(p, 1, l, out);
            total += l;
        } else if (FAILED(hr)) {
            fprintf(stderr, "ProcessOutput -> 0x%08lx\n", (unsigned long)hr);
            exit(1);
        }
        bdo.pBuffer->lpVtbl->Release(bdo.pBuffer);
        if (hr != S_OK ||
            !(bdo.dwStatus & DMO_OUTPUT_DATA_BUFFERF_INCOMPLETE))
            break;
    }
    return total;
}

int main(int argc, char **argv)
{
    IMediaObject *mo;
    DWORD outmax, align;
    long olen, flen, off = 0, bytes = 0;
    BYTE *objects, *wfx;
    WORD block;
    int in = 0;
    FILE *out;

    if (argc != 4) {
        fprintf(stderr, "usage: wma_decode OBJECTS FORMAT OUT\n");
        return 1;
    }
    objects = read_file(argv[1], &olen);
    wfx = read_file(argv[2], &flen);

    CoInitialize(NULL);
    CHECK(CoCreateInstance(&CLSID_CWMADecMediaObject, NULL,
                           CLSCTX_INPROC_SERVER, &IID_IMediaObject,
                           (void **)&mo));
    require_native_decoder(mo);
    set_input_type(mo, wfx, flen);
    block = set_output_type(mo);
    CHECK(IMediaObject_GetOutputSizeInfo(mo, 0, &outmax, &align));
    /* Room for several decoded packets at once. */
    if (outmax < (1 << 20))
        outmax = 1 << 20;
    CHECK(IMediaObject_AllocateStreamingResources(mo));

    out = fopen(argv[3], "wb");
    if (!out) {
        fprintf(stderr, "cannot create %s\n", argv[3]);
        return 1;
    }
    while (off + 8 <= olen) {
        uint32_t len, ms;
        IMediaBuffer *inb;
        BYTE *p;
        HRESULT hr;

        memcpy(&len, objects + off, 4);
        memcpy(&ms, objects + off + 4, 4);
        off += 8;
        if (off + (long)len > olen) {
            fprintf(stderr, "truncated record %d\n", in);
            return 1;
        }
        inb = newbuf(len ? len : 1);
        inb->lpVtbl->GetBufferAndLength(inb, &p, NULL);
        memcpy(p, objects + off, len);
        inb->lpVtbl->SetLength(inb, len);
        off += len;
        hr = IMediaObject_ProcessInput(mo, 0, inb,
                                       DMO_INPUT_DATA_BUFFERF_SYNCPOINT |
                                           DMO_INPUT_DATA_BUFFERF_TIME,
                                       (REFERENCE_TIME)ms * 10000, 0);
        inb->lpVtbl->Release(inb);
        if (FAILED(hr)) {
            fprintf(stderr, "object %d ProcessInput -> 0x%08lx\n", in,
                    (unsigned long)hr);
            return 1;
        }
        in++;
        bytes += drain(mo, outmax, out);
    }
    /* The end of the stream releases what the decoder holds back. */
    CHECK(IMediaObject_Discontinuity(mo, 0));
    bytes += drain(mo, outmax, out);
    fclose(out);
    printf("%d objects in, %ld bytes out, %ld sample frames\n", in, bytes,
           block ? bytes / block : 0);
    IMediaObject_FreeStreamingResources(mo);
    IMediaObject_Release(mo);
    CoUninitialize();
    return 0;
}
