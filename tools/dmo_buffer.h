/* Minimal IMediaBuffer for the DMO harnesses. Include after <dmo.h>. */
#ifndef DMO_BUFFER_H
#define DMO_BUFFER_H

#include <stdlib.h>

typedef struct {
    IMediaBufferVtbl *lpVtbl;
    LONG ref;
    BYTE *buf;
    DWORD maxlen;
    DWORD len;
} Buf;

static HRESULT STDMETHODCALLTYPE B_QI(IMediaBuffer *s, REFIID riid, void **o)
{
    if (IsEqualIID(riid, &IID_IMediaBuffer) ||
        IsEqualIID(riid, &IID_IUnknown)) {
        *o = s;
        s->lpVtbl->AddRef(s);
        return S_OK;
    }
    *o = NULL;
    return E_NOINTERFACE;
}
static ULONG STDMETHODCALLTYPE B_Add(IMediaBuffer *s)
{
    return (ULONG)InterlockedIncrement(&((Buf *)s)->ref);
}
static ULONG STDMETHODCALLTYPE B_Rel(IMediaBuffer *s)
{
    Buf *b = (Buf *)s;
    LONG r = InterlockedDecrement(&b->ref);
    if (!r) {
        free(b->buf);
        free(b);
    }
    return (ULONG)r;
}
static HRESULT STDMETHODCALLTYPE B_Get(IMediaBuffer *s, BYTE **b, DWORD *l)
{
    Buf *x = (Buf *)s;
    if (b)
        *b = x->buf;
    if (l)
        *l = x->len;
    return S_OK;
}
static HRESULT STDMETHODCALLTYPE B_Set(IMediaBuffer *s, DWORD l)
{
    Buf *x = (Buf *)s;
    if (l > x->maxlen)
        return E_INVALIDARG;
    x->len = l;
    return S_OK;
}
static HRESULT STDMETHODCALLTYPE B_Max(IMediaBuffer *s, DWORD *l)
{
    *l = ((Buf *)s)->maxlen;
    return S_OK;
}
static IMediaBufferVtbl bvt = { B_QI, B_Add, B_Rel, B_Set, B_Max, B_Get };

static IMediaBuffer *newbuf(DWORD n)
{
    Buf *b = calloc(1, sizeof(Buf));
    b->lpVtbl = &bvt;
    b->ref = 1;
    b->buf = malloc(n);
    b->maxlen = n;
    return (IMediaBuffer *)b;
}

#endif
