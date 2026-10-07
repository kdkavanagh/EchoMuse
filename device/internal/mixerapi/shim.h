/*
 * shim.h — the C half of the libmixerAPI binding.
 *
 * Unlike internal/opensl/shim.h, libmixerAPI.so is a plain C ABI, not a
 * vtable of interfaces: every symbol is an ordinary exported function. The
 * only reason this file exists at all is the same reason ort/shim.h and
 * opensl/shim.h do — cgo cannot call a Go-side function pointer obtained
 * from dlsym directly; a C wrapper has to hold the typed pointer and make
 * the call. There is no engine/output-mix setup step here (libmixerAPI has
 * none) and no callback registration (Phase 1 does not use
 * MixerSetupAsyncCallbacks — see mixerapi.go's package doc for why).
 *
 * The library is dlopen'd at RUNTIME, not linked, for the same reason as
 * opensl: a Fire OS 5 device, which has no libmixerAPI.so, still starts the
 * binary — cmd/server.go's backend selection falls back to OpenSL when
 * Open here fails.
 *
 * Error convention, matching the other two shims: every function that can
 * fail returns char* — NULL on success, or a malloc'd message the Go side
 * turns into an error and frees.
 */
#ifndef EM_MIXERAPI_SHIM_H
#define EM_MIXERAPI_SHIM_H

#include <dlfcn.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static char *em_dup(const char *s) {
	size_t n = strlen(s) + 1;
	char *p = (char *)malloc(n);
	if (p) memcpy(p, s, n);
	return p;
}

/* Function pointer types for every symbol Phase 1 calls, pinned against a
 * disassembly of the device's libmixerAPI.so and confirmed by throwaway spikes on device
 * G090LF0965260F1J (docs/fireos6-port.md §2.1; mixerapi.go's package doc
 * records the confirming evidence for each). [INFERENCE, confirmed by
 * behaviour] */
typedef void    *(*em_open_rec_fn)(const char *type);
typedef void    *(*em_get_buf_rec_timed_fn)(void *h, int *status, unsigned *size, uint64_t *ts);
typedef int      (*em_release_buf_rec_fn)(void *h);
typedef void    *(*em_open_play_fn)(unsigned rate, unsigned ch, unsigned bits, uintptr_t mode);
typedef void    *(*em_get_buf_play_fn)(void *h, int *status, unsigned *size);
typedef int      (*em_release_buf_play_fn)(void *h, unsigned bytes);
typedef unsigned (*em_underflow_fn)(void *h);
typedef int      (*em_close_fn)(void *h);
typedef unsigned (*em_bufsize_fn)(void *h);

typedef struct {
	void *dl;
	em_open_rec_fn          openRec;
	em_get_buf_rec_timed_fn getBufRecTimed;
	em_release_buf_rec_fn   releaseBufRec;
	em_open_play_fn         openPlay;
	em_get_buf_play_fn      getBufPlay;
	em_release_buf_play_fn  releaseBufPlay;
	em_underflow_fn         underflowMs;
	em_close_fn             close;
	em_bufsize_fn           bufSize;
} em_lib;

/* em_resolve_fn dlsyms a function pointer by name, the same shape as
 * opensl/shim.h's em_resolve_iid: NULL on success, a malloc'd message on
 * failure. void** sidesteps the (unenforced, universally relied-upon)
 * assumption that function and data pointers are interchangeable on this
 * target — the same assumption every other dlsym-for-a-function call in
 * this tree already makes. */
static char *em_resolve_fn(void *dl, const char *name, void **out) {
	*out = dlsym(dl, name);
	if (*out) return NULL;
	const char *e = dlerror();
	char msg[128];
	snprintf(msg, sizeof(msg), "dlsym %s: %s", name, e ? e : "symbol not found");
	return em_dup(msg);
}

/* em_mixer_open dlopens libmixerAPI.so and resolves every symbol Phase 1
 * calls. All nine must resolve: this build carries the whole surface
 * (confirmed via the symbol table in docs/fireos6-port.md's evidence), so a
 * partial resolve means the wrong library, not a version to degrade
 * against. */
static char *em_mixer_open(const char *path, em_lib *out) {
	memset(out, 0, sizeof(*out));
	void *dl = dlopen(path, RTLD_NOW);
	if (!dl) {
		const char *e = dlerror();
		return em_dup(e ? e : "dlopen failed");
	}
	char *err;
	if ((err = em_resolve_fn(dl, "MixerOpenRec", (void **)&out->openRec)) ||
	    (err = em_resolve_fn(dl, "MixerGetBufRecTimed", (void **)&out->getBufRecTimed)) ||
	    (err = em_resolve_fn(dl, "MixerReleaseBufRec", (void **)&out->releaseBufRec)) ||
	    (err = em_resolve_fn(dl, "MixerOpenPlay", (void **)&out->openPlay)) ||
	    (err = em_resolve_fn(dl, "MixerGetBufPlay", (void **)&out->getBufPlay)) ||
	    (err = em_resolve_fn(dl, "MixerReleaseBufPlay", (void **)&out->releaseBufPlay)) ||
	    (err = em_resolve_fn(dl, "MixerGetUnderflowMs", (void **)&out->underflowMs)) ||
	    (err = em_resolve_fn(dl, "MixerClose", (void **)&out->close)) ||
	    (err = em_resolve_fn(dl, "MixerGetBufSize", (void **)&out->bufSize))) {
		dlclose(dl);
		memset(out, 0, sizeof(*out));
		return err;
	}
	out->dl = dl;
	return NULL;
}

static void *em_open_rec(em_lib *l, const char *name) { return l->openRec(name); }

static void *em_get_buf_rec_timed(em_lib *l, void *h, int *status, unsigned *size, uint64_t *ts) {
	return l->getBufRecTimed(h, status, size, ts);
}

static int em_release_buf_rec(em_lib *l, void *h) { return l->releaseBufRec(h); }

static void *em_open_play(em_lib *l, unsigned rate, unsigned ch, unsigned bits, uintptr_t mode) {
	return l->openPlay(rate, ch, bits, mode);
}

static void *em_get_buf_play(em_lib *l, void *h, int *status, unsigned *size) {
	return l->getBufPlay(h, status, size);
}

static int em_release_buf_play(em_lib *l, void *h, unsigned bytes) { return l->releaseBufPlay(h, bytes); }
static unsigned em_underflow_ms(em_lib *l, void *h) { return l->underflowMs(h); }
static int em_close(em_lib *l, void *h) { return l->close(h); }
static unsigned em_buf_size(em_lib *l, void *h) { return l->bufSize(h); }

#endif /* EM_MIXERAPI_SHIM_H */
