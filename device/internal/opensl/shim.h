/*
 * shim.h — the C half of the OpenSL ES binding.
 *
 * Same two reasons this exists as internal/wakeword/ort/shim.h:
 *
 *  1. Every OpenSL ES object (SLObjectItf, SLEngineItf, SLRecordItf, ...) is a
 *     struct of function pointers, and cgo cannot call a function pointer held
 *     in a C struct. Every call needs a C-side wrapper.
 *
 *  2. libOpenSLES.so is dlopen'd at runtime, not linked. Nothing in the build
 *     references an OpenSL ES symbol directly, so the firmware links and boots
 *     on a device (or a Go host build) where the library was never resolved at
 *     link time — em_sl_open simply fails and the caller falls back to the
 *     tinyalsa backends. Unlike the ORT shim, this package needs no vendored
 *     header: the NDK toolchain this repo cross-compiles with already ships
 *     the real Khronos/AOSP OpenSL ES headers (SLES/OpenSLES*.h) under its own
 *     sysroot, so `#include`ing them costs nothing extra to vendor and can't
 *     drift from what the device actually implements.
 *
 * Unlike ORT's OrtApi, OpenSL ES's interface IDs (SL_IID_ENGINE and friends)
 * are exported as DATA symbols, not looked up through a version-negotiated
 * "get me the API" entry point — the headers declare them
 * `extern SL_API const SLInterfaceID SL_IID_ENGINE;` for a caller that links
 * against libOpenSLES.so normally. Referencing that extern directly here would
 * force the Go linker to resolve it at BUILD time, defeating the whole point
 * of dlopen. So every interface ID this file needs is resolved with dlsym
 * instead (em_resolve_iid) and the header is included only for its type
 * definitions and #define constants (SL_DATAFORMAT_PCM, SL_BOOLEAN_TRUE, the
 * struct layouts, ...), never for a symbol that has to be linked.
 *
 * Error convention, matching the ORT shim: every function that can fail
 * returns char* — NULL on success, or a malloc'd message the Go side turns
 * into an error and frees.
 */
#ifndef EM_OPENSL_SHIM_H
#define EM_OPENSL_SHIM_H

#include <dlfcn.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include <SLES/OpenSLES.h>
#include <SLES/OpenSLES_Android.h>
#include <SLES/OpenSLES_AndroidConfiguration.h>

/* Forward declarations of the Go callbacks this file calls into (defined
 * with //export in opensl.go; cgo emits matching prototypes into
 * _cgo_export.h, but the buffer-queue callbacks below need them before that
 * header is available, so they're declared here by hand — same signature,
 * which is all C's redeclaration rules require). */
extern void em_go_recorder_cb(long long ctx);
extern void em_go_player_cb(long long ctx);

static char *em_dup(const char *s) {
	size_t n = strlen(s) + 1;
	char *p = (char *)malloc(n);
	if (p) memcpy(p, s, n);
	return p;
}

static char *em_slerr(const char *what, SLresult r) {
	char msg[160];
	snprintf(msg, sizeof(msg), "%s failed: SLresult 0x%x", what, (unsigned)r);
	return em_dup(msg);
}

/* em_resolve_iid dlsyms an SLInterfaceID constant by its exported symbol
 * name. The symbol IS the SLInterfaceID value (a pointer into libOpenSLES's
 * own static data), so dlsym gives the address of that pointer and this
 * dereferences it once — the same shape as reading any other exported global
 * via dlsym, just typed for this API. */
static char *em_resolve_iid(void *dl, const char *name, SLInterfaceID *out) {
	void *sym = dlsym(dl, name);
	if (!sym) {
		const char *e = dlerror();
		char msg[128];
		snprintf(msg, sizeof(msg), "dlsym %s: %s", name, e ? e : "symbol not found");
		return em_dup(msg);
	}
	*out = *(const SLInterfaceID *)sym;
	return NULL;
}

/* em_preset maps EchoMuse's small selector onto the real
 * SL_ANDROID_RECORDING_PRESET_* macro, so the mapping is resolved by the
 * compiler against the real header rather than a hex constant guessed on the
 * Go side. See docs/native-afe-migration.md's input_source table: GENERIC is
 * AUDIO_SOURCE_MIC (ASP pipeline 2, passthrough — the control), VOICE_
 * RECOGNITION and VOICE_COMMUNICATION are pipelines 0 and 1. */
static SLuint32 em_preset(int selector) {
	switch (selector) {
	case 1: return SL_ANDROID_RECORDING_PRESET_VOICE_RECOGNITION;
	case 2: return SL_ANDROID_RECORDING_PRESET_VOICE_COMMUNICATION;
	default: return SL_ANDROID_RECORDING_PRESET_GENERIC;
	}
}

/* ─── engine ──────────────────────────────────────────────────────────────── */

typedef struct {
	void *dl;
	SLObjectItf obj;
	SLEngineItf itf;
	SLObjectItf outmix;

	/* Resolved once at open and handed to every Recorder/Player this engine
	 * creates, so opening N of them costs N-1 fewer dlsym round trips. */
	SLInterfaceID iid_record;
	SLInterfaceID iid_absq;
	SLInterfaceID iid_androidconfig;
	SLInterfaceID iid_play;
} em_engine;

typedef SLresult (*em_slCreateEngine_fn)(SLObjectItf *pEngine, SLuint32 numOptions,
                                          const SLEngineOption *pEngineOptions,
                                          SLuint32 numInterfaces,
                                          const SLInterfaceID *pInterfaceIds,
                                          const SLboolean *pInterfaceRequired);

/* em_sl_open dlopens libOpenSLES.so, creates the engine, and realizes a
 * single shared OutputMix — every Player this engine creates routes into it.
 * One engine per process is not merely convenient: some Android versions
 * enforce it. */
static char *em_sl_open(const char *path, em_engine *out) {
	memset(out, 0, sizeof(*out));

	void *dl = dlopen(path, RTLD_NOW | RTLD_LOCAL);
	if (!dl) {
		const char *e = dlerror();
		return em_dup(e ? e : "dlopen failed");
	}

	em_slCreateEngine_fn create = (em_slCreateEngine_fn)dlsym(dl, "slCreateEngine");
	if (!create) {
		const char *e = dlerror();
		char *msg = em_dup(e ? e : "slCreateEngine not found");
		dlclose(dl);
		return msg;
	}

	SLObjectItf engineObj = NULL;
	SLresult r = create(&engineObj, 0, NULL, 0, NULL, NULL);
	if (r != SL_RESULT_SUCCESS) {
		dlclose(dl);
		return em_slerr("slCreateEngine", r);
	}
	r = (*engineObj)->Realize(engineObj, SL_BOOLEAN_FALSE);
	if (r != SL_RESULT_SUCCESS) {
		(*engineObj)->Destroy(engineObj);
		dlclose(dl);
		return em_slerr("engine Realize", r);
	}

	SLInterfaceID iid_engine;
	char *err;
	if ((err = em_resolve_iid(dl, "SL_IID_ENGINE", &iid_engine)) ||
	    (err = em_resolve_iid(dl, "SL_IID_RECORD", &out->iid_record)) ||
	    (err = em_resolve_iid(dl, "SL_IID_ANDROIDSIMPLEBUFFERQUEUE", &out->iid_absq)) ||
	    (err = em_resolve_iid(dl, "SL_IID_ANDROIDCONFIGURATION", &out->iid_androidconfig)) ||
	    (err = em_resolve_iid(dl, "SL_IID_PLAY", &out->iid_play))) {
		(*engineObj)->Destroy(engineObj);
		dlclose(dl);
		return err;
	}

	SLEngineItf engine = NULL;
	r = (*engineObj)->GetInterface(engineObj, iid_engine, &engine);
	if (r != SL_RESULT_SUCCESS) {
		(*engineObj)->Destroy(engineObj);
		dlclose(dl);
		return em_slerr("GetInterface ENGINE", r);
	}

	SLObjectItf outmix = NULL;
	r = (*engine)->CreateOutputMix(engine, &outmix, 0, NULL, NULL);
	if (r != SL_RESULT_SUCCESS) {
		(*engineObj)->Destroy(engineObj);
		dlclose(dl);
		return em_slerr("CreateOutputMix", r);
	}
	r = (*outmix)->Realize(outmix, SL_BOOLEAN_FALSE);
	if (r != SL_RESULT_SUCCESS) {
		(*outmix)->Destroy(outmix);
		(*engineObj)->Destroy(engineObj);
		dlclose(dl);
		return em_slerr("OutputMix Realize", r);
	}

	out->dl = dl;
	out->obj = engineObj;
	out->itf = engine;
	out->outmix = outmix;
	return NULL;
}

static void em_engine_close(em_engine *e) {
	if (e->outmix) (*e->outmix)->Destroy(e->outmix);
	if (e->obj) (*e->obj)->Destroy(e->obj);
	if (e->dl) dlclose(e->dl);
	memset(e, 0, sizeof(*e));
}

/* ─── recorder ────────────────────────────────────────────────────────────── */

typedef struct {
	SLObjectItf obj;
	SLRecordItf record;
	SLAndroidSimpleBufferQueueItf queue;
	void **bufs;
	int nbuf;
	int bufBytes;
} em_recorder;

static void em_recorder_cb(SLAndroidSimpleBufferQueueItf caller, void *pContext) {
	(void)caller;
	em_go_recorder_cb((long long)(intptr_t)pContext);
}

/* em_recorder_open creates a mono 16-bit AudioRecorder against the engine's
 * default audio input, configured with the recording preset that selects the
 * ASP pipeline (docs/native-afe-migration.md). ctx is an opaque handle the Go
 * side uses to find the right Recorder when em_go_recorder_cb fires — it may
 * run on an OpenSL ES-owned thread Go never created.
 *
 * The preset MUST be set via SLAndroidConfigurationItf before Realize: it is
 * documented as a pre-realization-only configuration item, and setting it
 * after would silently not apply. */
static char *em_recorder_open(em_engine *e, int presetSel, int rateHz, int bufBytes,
                               int nbuf, long long ctx, em_recorder *out) {
	memset(out, 0, sizeof(*out));
	if (!e->itf) return em_dup("engine not open");

	SLDataLocator_IODevice locIn = {
		SL_DATALOCATOR_IODEVICE, SL_IODEVICE_AUDIOINPUT, SL_DEFAULTDEVICEID_AUDIOINPUT, NULL
	};
	SLDataSource src = { &locIn, NULL };

	SLDataLocator_AndroidSimpleBufferQueue locOut = {
		SL_DATALOCATOR_ANDROIDSIMPLEBUFFERQUEUE, (SLuint32)nbuf
	};
	/* samplesPerSec is spec'd in milliHertz, not Hertz — hence *1000. Using
	 * the arithmetic form rather than the SL_SAMPLINGRATE_* convenience
	 * macros keeps this file's only rate dependency the well-documented
	 * units, not a second set of vendor constants to get right. */
	SLDataFormat_PCM fmt = {
		SL_DATAFORMAT_PCM, 1, (SLuint32)rateHz * 1000,
		SL_PCMSAMPLEFORMAT_FIXED_16, SL_PCMSAMPLEFORMAT_FIXED_16,
		SL_SPEAKER_FRONT_CENTER, SL_BYTEORDER_LITTLEENDIAN
	};
	SLDataSink snk = { &locOut, &fmt };

	const SLInterfaceID iids[2] = { e->iid_absq, e->iid_androidconfig };
	const SLboolean req[2] = { SL_BOOLEAN_TRUE, SL_BOOLEAN_TRUE };

	SLObjectItf obj = NULL;
	SLresult r = (*e->itf)->CreateAudioRecorder(e->itf, &obj, &src, &snk, 2, iids, req);
	if (r != SL_RESULT_SUCCESS) return em_slerr("CreateAudioRecorder", r);

	SLAndroidConfigurationItf config = NULL;
	r = (*obj)->GetInterface(obj, e->iid_androidconfig, &config);
	if (r != SL_RESULT_SUCCESS) {
		(*obj)->Destroy(obj);
		return em_slerr("GetInterface ANDROIDCONFIGURATION (recorder)", r);
	}
	SLuint32 preset = em_preset(presetSel);
	r = (*config)->SetConfiguration(config, SL_ANDROID_KEY_RECORDING_PRESET, &preset, sizeof(preset));
	if (r != SL_RESULT_SUCCESS) {
		(*obj)->Destroy(obj);
		return em_slerr("SetConfiguration RECORDING_PRESET", r);
	}

	r = (*obj)->Realize(obj, SL_BOOLEAN_FALSE);
	if (r != SL_RESULT_SUCCESS) {
		(*obj)->Destroy(obj);
		return em_slerr("recorder Realize", r);
	}

	SLRecordItf record = NULL;
	r = (*obj)->GetInterface(obj, e->iid_record, &record);
	if (r != SL_RESULT_SUCCESS) {
		(*obj)->Destroy(obj);
		return em_slerr("GetInterface RECORD", r);
	}
	SLAndroidSimpleBufferQueueItf queue = NULL;
	r = (*obj)->GetInterface(obj, e->iid_absq, &queue);
	if (r != SL_RESULT_SUCCESS) {
		(*obj)->Destroy(obj);
		return em_slerr("GetInterface ANDROIDSIMPLEBUFFERQUEUE (recorder)", r);
	}
	r = (*queue)->RegisterCallback(queue, em_recorder_cb, (void *)(intptr_t)ctx);
	if (r != SL_RESULT_SUCCESS) {
		(*obj)->Destroy(obj);
		return em_slerr("RegisterCallback (recorder)", r);
	}

	void **bufs = (void **)calloc((size_t)nbuf, sizeof(void *));
	if (!bufs) {
		(*obj)->Destroy(obj);
		return em_dup("out of memory allocating recorder buffers");
	}
	for (int i = 0; i < nbuf; i++) {
		bufs[i] = malloc((size_t)bufBytes);
		if (!bufs[i]) {
			for (int j = 0; j < i; j++) free(bufs[j]);
			free(bufs);
			(*obj)->Destroy(obj);
			return em_dup("out of memory allocating a recorder buffer");
		}
	}

	out->obj = obj;
	out->record = record;
	out->queue = queue;
	out->bufs = bufs;
	out->nbuf = nbuf;
	out->bufBytes = bufBytes;
	return NULL;
}

/* em_recorder_prime enqueues every buffer once, in slot order 0..nbuf-1 — the
 * Go side mirrors this exact order into its own bookkeeping (see Recorder in
 * opensl.go) so it can tell which slot a completion callback refers to
 * without OpenSL ES saying so directly. Call once, after open and before
 * em_recorder_start: an unprimed queue has nothing for the recorder to fill. */
static char *em_recorder_prime(em_recorder *r) {
	for (int i = 0; i < r->nbuf; i++) {
		SLresult res = (*r->queue)->Enqueue(r->queue, r->bufs[i], (SLuint32)r->bufBytes);
		if (res != SL_RESULT_SUCCESS) return em_slerr("Enqueue (recorder prime)", res);
	}
	return NULL;
}

static char *em_recorder_start(em_recorder *r) {
	SLresult res = (*r->record)->SetRecordState(r->record, SL_RECORDSTATE_RECORDING);
	if (res != SL_RESULT_SUCCESS) return em_slerr("SetRecordState RECORDING", res);
	return NULL;
}

static char *em_recorder_stop(em_recorder *r) {
	SLresult res = (*r->record)->SetRecordState(r->record, SL_RECORDSTATE_STOPPED);
	if (res != SL_RESULT_SUCCESS) return em_slerr("SetRecordState STOPPED", res);
	return NULL;
}

static void *em_recorder_bufptr(em_recorder *r, int idx) { return r->bufs[idx]; }

static char *em_recorder_enqueue(em_recorder *r, int idx) {
	SLresult res = (*r->queue)->Enqueue(r->queue, r->bufs[idx], (SLuint32)r->bufBytes);
	if (res != SL_RESULT_SUCCESS) return em_slerr("Enqueue (recorder)", res);
	return NULL;
}

static void em_recorder_close(em_recorder *r) {
	if (!r->obj) return;
	(*r->obj)->Destroy(r->obj);
	for (int i = 0; i < r->nbuf; i++) free(r->bufs[i]);
	free(r->bufs);
	memset(r, 0, sizeof(*r));
}

/* ─── player ──────────────────────────────────────────────────────────────── */

typedef struct {
	SLObjectItf obj;
	SLPlayItf play;
	SLAndroidSimpleBufferQueueItf queue;
	void **bufs;
	int nbuf;
	int bufBytes;
} em_player;

static void em_player_cb(SLAndroidSimpleBufferQueueItf caller, void *pContext) {
	(void)caller;
	em_go_player_cb((long long)(intptr_t)pContext);
}

/* em_player_open creates a mono 16-bit AudioPlayer feeding the engine's
 * shared OutputMix. Playback is set running immediately with an empty queue:
 * an OpenSL ES player that is merely PAUSED never fires buffer-queue
 * completion callbacks, so — unlike PcmSpeaker's silence-filled ALSA
 * session — there is no way to "start the stream on silence" here. The
 * caller (slspeaker) supplies silence itself between periods if it wants the
 * far-end AEC reference to stay continuous. */
static char *em_player_open(em_engine *e, int rateHz, int bufBytes, int nbuf,
                             long long ctx, em_player *out) {
	memset(out, 0, sizeof(*out));
	if (!e->itf || !e->outmix) return em_dup("engine not open");

	SLDataLocator_AndroidSimpleBufferQueue locIn = {
		SL_DATALOCATOR_ANDROIDSIMPLEBUFFERQUEUE, (SLuint32)nbuf
	};
	SLDataFormat_PCM fmt = {
		SL_DATAFORMAT_PCM, 1, (SLuint32)rateHz * 1000,
		SL_PCMSAMPLEFORMAT_FIXED_16, SL_PCMSAMPLEFORMAT_FIXED_16,
		SL_SPEAKER_FRONT_CENTER, SL_BYTEORDER_LITTLEENDIAN
	};
	SLDataSource src = { &locIn, &fmt };

	SLDataLocator_OutputMix locOut = { SL_DATALOCATOR_OUTPUTMIX, e->outmix };
	SLDataSink snk = { &locOut, NULL };

	const SLInterfaceID iids[1] = { e->iid_absq };
	const SLboolean req[1] = { SL_BOOLEAN_TRUE };

	SLObjectItf obj = NULL;
	SLresult r = (*e->itf)->CreateAudioPlayer(e->itf, &obj, &src, &snk, 1, iids, req);
	if (r != SL_RESULT_SUCCESS) return em_slerr("CreateAudioPlayer", r);

	r = (*obj)->Realize(obj, SL_BOOLEAN_FALSE);
	if (r != SL_RESULT_SUCCESS) {
		(*obj)->Destroy(obj);
		return em_slerr("player Realize", r);
	}

	SLPlayItf play = NULL;
	r = (*obj)->GetInterface(obj, e->iid_play, &play);
	if (r != SL_RESULT_SUCCESS) {
		(*obj)->Destroy(obj);
		return em_slerr("GetInterface PLAY", r);
	}
	SLAndroidSimpleBufferQueueItf queue = NULL;
	r = (*obj)->GetInterface(obj, e->iid_absq, &queue);
	if (r != SL_RESULT_SUCCESS) {
		(*obj)->Destroy(obj);
		return em_slerr("GetInterface ANDROIDSIMPLEBUFFERQUEUE (player)", r);
	}
	r = (*queue)->RegisterCallback(queue, em_player_cb, (void *)(intptr_t)ctx);
	if (r != SL_RESULT_SUCCESS) {
		(*obj)->Destroy(obj);
		return em_slerr("RegisterCallback (player)", r);
	}

	void **bufs = (void **)calloc((size_t)nbuf, sizeof(void *));
	if (!bufs) {
		(*obj)->Destroy(obj);
		return em_dup("out of memory allocating player buffers");
	}
	for (int i = 0; i < nbuf; i++) {
		bufs[i] = malloc((size_t)bufBytes);
		if (!bufs[i]) {
			for (int j = 0; j < i; j++) free(bufs[j]);
			free(bufs);
			(*obj)->Destroy(obj);
			return em_dup("out of memory allocating a player buffer");
		}
	}

	r = (*play)->SetPlayState(play, SL_PLAYSTATE_PLAYING);
	if (r != SL_RESULT_SUCCESS) {
		for (int i = 0; i < nbuf; i++) free(bufs[i]);
		free(bufs);
		(*obj)->Destroy(obj);
		return em_slerr("SetPlayState PLAYING", r);
	}

	out->obj = obj;
	out->play = play;
	out->queue = queue;
	out->bufs = bufs;
	out->nbuf = nbuf;
	out->bufBytes = bufBytes;
	return NULL;
}

static void *em_player_bufptr(em_player *p, int idx) { return p->bufs[idx]; }

static char *em_player_enqueue(em_player *p, int idx, int n) {
	SLresult res = (*p->queue)->Enqueue(p->queue, p->bufs[idx], (SLuint32)n);
	if (res != SL_RESULT_SUCCESS) return em_slerr("Enqueue (player)", res);
	return NULL;
}

/* em_player_clear drops every buffer queued but not yet played — the OpenSL
 * ES equivalent of Flush()/FlushMusic() (barge-in / user stop). A buffer
 * already handed to the HAL and mid-flight is unaffected, same as tinyalsa's
 * "periods already in the ALSA ring still play". */
static char *em_player_clear(em_player *p) {
	SLresult res = (*p->queue)->Clear(p->queue);
	if (res != SL_RESULT_SUCCESS) return em_slerr("Clear (player)", res);
	return NULL;
}

static void em_player_close(em_player *p) {
	if (!p->obj) return;
	(*p->play)->SetPlayState(p->play, SL_PLAYSTATE_STOPPED);
	(*p->obj)->Destroy(p->obj);
	for (int i = 0; i < p->nbuf; i++) free(p->bufs[i]);
	free(p->bufs);
	memset(p, 0, sizeof(*p));
}

#endif /* EM_OPENSL_SHIM_H */
