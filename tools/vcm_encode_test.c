/* VfW smoke test for wmv9vcm.dll (vidc.WMV3) under 32-bit Wine.
 * Opens the compressor, encodes N synthetic YUY2 frames, writes raw
 * frames as [u32 size][bytes] records to argv[1]. Build 32-bit only:
 *   i686-w64-mingw32-gcc -m32 -O2 -Wall -o vcm_encode_test.exe \
 *     vcm_encode_test.c -lmsvfw32 -lwinmm
 */
#include <windows.h>
#include <vfw.h>
#include <stdio.h>
#include <stdlib.h>

static void fill_yuy2(BYTE *p, int w, int h, int frame)
{
    int x, y;
    int bx = (frame * 8) % (w - 32);
    for (y = 0; y < h; y++) {
        for (x = 0; x < w; x += 2) {
            BYTE yy = (BYTE)((x + y + frame * 2) & 0xff);
            if (y > h / 4 && y < 3 * h / 4 && x > bx && x < bx + 32)
                yy = 200;
            *p++ = yy;
            *p++ = 128;
            *p++ = (BYTE)(255 - yy);
            *p++ = 128;
        }
    }
}

int main(int argc, char **argv)
{
    const int w = 320, h = 240, nframes = 30;
    HIC hic;
    BITMAPINFOHEADER inbih = { 0 }, *outbih = NULL;
    DWORD outsize;
    BYTE *inbuf, *outbuf;
    FILE *f;
    int i;

    if (argc < 2) {
        fprintf(stderr, "usage: %s out.bin\n", argv[0]);
        return 2;
    }
    hic = ICOpen(ICTYPE_VIDEO, mmioFOURCC('W', 'M', 'V', '3'),
                 ICMODE_COMPRESS);
    if (!hic) {
        fprintf(stderr, "ICOpen vidc.WMV3 failed\n");
        return 1;
    }
    inbih.biSize = sizeof(inbih);
    inbih.biWidth = w;
    inbih.biHeight = h;
    inbih.biPlanes = 1;
    inbih.biBitCount = 16;
    inbih.biCompression = mmioFOURCC('Y', 'U', 'Y', '2');
    inbih.biSizeImage = (DWORD)(w * h * 2);
    if (ICCompressQuery(hic, &inbih, NULL) != ICERR_OK) {
        fprintf(stderr, "ICCompressQuery rejected YUY2 input\n");
        ICClose(hic);
        return 1;
    }
    outsize = ICCompressGetFormatSize(hic, &inbih);
    outbih = (BITMAPINFOHEADER *)malloc(outsize);
    if (!outbih) {
        ICClose(hic);
        return 1;
    }
    if (ICCompressGetFormat(hic, &inbih, outbih) != ICERR_OK) {
        fprintf(stderr, "ICCompressGetFormat failed\n");
        ICClose(hic);
        return 1;
    }
    printf("out fcc=%c%c%c%c %dx%d\n",
           (char)(outbih->biCompression & 0xff),
           (char)((outbih->biCompression >> 8) & 0xff),
           (char)((outbih->biCompression >> 16) & 0xff),
           (char)((outbih->biCompression >> 24) & 0xff),
           (int)outbih->biWidth, (int)outbih->biHeight);
    if (ICCompressBegin(hic, &inbih, outbih) != ICERR_OK) {
        fprintf(stderr, "ICCompressBegin failed\n");
        ICClose(hic);
        return 1;
    }
    inbuf = (BYTE *)malloc(inbih.biSizeImage);
    outbuf = (BYTE *)malloc(outbih->biSizeImage > 0 ?
                            outbih->biSizeImage : (DWORD)(w * h * 2));
    if (!inbuf || !outbuf) {
        ICClose(hic);
        return 1;
    }
    f = fopen(argv[1], "wb");
    if (!f) {
        perror("fopen");
        ICClose(hic);
        return 1;
    }
    for (i = 0; i < nframes; i++) {
        DWORD ckid = 0, flags = 0;
        LRESULT rc;
        fill_yuy2(inbuf, w, h, i);
        rc = ICCompress(hic, 0, outbih, outbuf, &inbih, inbuf,
                        &ckid, &flags, i, 0, 7500, NULL, NULL);
        if (rc != ICERR_OK) {
            fprintf(stderr, "frame %d ICCompress rc=%ld\n", i, (long)rc);
            break;
        }
        {
            DWORD sz = outbih->biSizeImage;
            fwrite(&sz, 4, 1, f);
            fwrite(outbuf, 1, sz, f);
            printf("frame %3d size %7lu flags 0x%lx%s\n", i,
                   (unsigned long)sz, (unsigned long)flags,
                   (flags & AVIIF_KEYFRAME) ? " key" : "");
        }
    }
    fclose(f);
    ICCompressEnd(hic);
    ICClose(hic);
    return 0;
}
