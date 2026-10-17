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
 *     references an OpenSL ES symbol directly, so the one firmware binary
 *     links and starts on the Dot, whose headless image has no OpenSL ES
 *     engine to speak of, and only em_sl_open fails there. This package needs
 *     no vendored header: the NDK toolchain the firmware cross-compiles with
 *     ships the real Khronos/AOSP headers (SLES/OpenSLES*.h) in its sysroot.
 *
 * OpenSL ES's interface IDs (SL_IID_ENGINE and friends) are exported as DATA
 * symbols, declared `extern SL_API const SLInterfaceID SL_IID_ENGINE;` for a
 * caller that links against libOpenSLES.so. Referencing one directly here
 * would make the linker resolve it at build time, defeating the dlopen. So
 * every interface ID this file needs is resolved with dlsym (em_resolve_iid),
 * and the headers are included only for types and #define constants.
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

/* The Go callbacks this file calls into (//export in opensl.go). cgo emits
 * matching prototypes into _cgo_export.h, which the buffer-queue callbacks
 * below cannot include, so they are declared here by hand with the same
 * signature. */
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

/* em_resolve_iid dlsyms an SLInterfaceID constant by its exported name. The
 * symbol IS the SLInterfaceID value (a pointer into libOpenSLES's own static
 * data), so dlsym gives the address of that pointer and this dereferences it
 * once. */
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

/* em_alloc_bufs allocates nbuf buffers of bufBytes each, or none. */
static void **em_alloc_bufs(int nbuf, int bufBytes) {
	void **bufs = (void **)calloc((size_t)nbuf, sizeof(void *));
	if (!bufs) return NULL;
	for (int i = 0; i < nbuf; i++) {
		bufs[i] = calloc(1, (size_t)bufBytes);
		if (!bufs[i]) {
			for (int j = 0; j < i; j++) free(bufs[j]);
			free(bufs);
			return NULL;
		}
	}
	return bufs;
}

static void em_free_bufs(void **bufs, int nbuf) {
	if (!bufs) return;
	for (int i = 0; i < nbuf; i++) free(bufs[i]);
	free(bufs);
}

/* ─── engine ──────────────────────────────────────────────────────────────── */

typedef struct {
	void *dl;
	SLObjectItf obj;
	SLEngineItf itf;
	SLObjectItf outmix;

	/* Resolved once at open and shared by every Recorder/Player. */
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

/* em_sl_open dlopens libOpenSLES.so, creates the engine, and realizes the
 * single OutputMix every Player routes into. The engine lives for the
 * process: some Android versions allow only one per process. */
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

	SLInterfaceID iid_engine;
	char *err;
	if ((err = em_resolve_iid(dl, "SL_IID_ENGINE", &iid_engine)) ||
	    (err = em_resolve_iid(dl, "SL_IID_RECORD", &out->iid_record)) ||
	    (err = em_resolve_iid(dl, "SL_IID_ANDROIDSIMPLEBUFFERQUEUE", &out->iid_absq)) ||
	    (err = em_resolve_iid(dl, "SL_IID_ANDROIDCONFIGURATION", &out->iid_androidconfig)) ||
	    (err = em_resolve_iid(dl, "SL_IID_PLAY", &out->iid_play))) {
		dlclose(dl);
		return err;
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

static void em_recorder_close(em_recorder *r) {
	if (!r->obj) return;
	/* Destroy waits for a callback in progress to return (Android's
	 * CallbackProtector), and none starts after it. */
	(*r->obj)->Destroy(r->obj);
	em_free_bufs(r->bufs, r->nbuf);
	memset(r, 0, sizeof(*r));
}

/* em_recorder_open creates a mono S16 AudioRecorder on the default input
 * with SL_ANDROID_RECORDING_PRESET_VOICE_RECOGNITION: AUDIO_SOURCE_
 * VOICE_RECOGNITION, which the Show's HAL runs through ASP pipeline 0, the
 * ASR pipeline with AEC and v3.3 metadata in bit 0
 * (docs/echo-show-crown.md §4.3). The preset is a pre-Realize configuration
 * item: set after Realize, it would not apply.
 *
 * Every buffer is then enqueued once, in slot order 0..nbuf-1, which the Go
 * side mirrors to know which slot each completion names, and recording
 * starts. ctx is the opaque handle em_go_recorder_cb receives, on a thread
 * OpenSL ES owns. */
static char *em_recorder_open(em_engine *e, int rateHz, int bufBytes, int nbuf,
                               long long ctx, em_recorder *out) {
	memset(out, 0, sizeof(*out));
	if (!e->itf) return em_dup("engine not open");

	SLDataLocator_IODevice locIn = {
		SL_DATALOCATOR_IODEVICE, SL_IODEVICE_AUDIOINPUT, SL_DEFAULTDEVICEID_AUDIOINPUT, NULL
	};
	SLDataSource src = { &locIn, NULL };

	SLDataLocator_AndroidSimpleBufferQueue locOut = {
		SL_DATALOCATOR_ANDROIDSIMPLEBUFFERQUEUE, (SLuint32)nbuf
	};
	/* samplesPerSec is in milliHertz. */
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
	out->obj = obj;
	out->nbuf = nbuf;
	out->bufBytes = bufBytes;

	SLAndroidConfigurationItf config = NULL;
	r = (*obj)->GetInterface(obj, e->iid_androidconfig, &config);
	if (r != SL_RESULT_SUCCESS) {
		em_recorder_close(out);
		return em_slerr("GetInterface ANDROIDCONFIGURATION (recorder)", r);
	}
	SLuint32 preset = SL_ANDROID_RECORDING_PRESET_VOICE_RECOGNITION;
	r = (*config)->SetConfiguration(config, SL_ANDROID_KEY_RECORDING_PRESET, &preset, sizeof(preset));
	if (r != SL_RESULT_SUCCESS) {
		em_recorder_close(out);
		return em_slerr("SetConfiguration RECORDING_PRESET", r);
	}

	r = (*obj)->Realize(obj, SL_BOOLEAN_FALSE);
	if (r != SL_RESULT_SUCCESS) {
		em_recorder_close(out);
		return em_slerr("recorder Realize", r);
	}
	r = (*obj)->GetInterface(obj, e->iid_record, &out->record);
	if (r != SL_RESULT_SUCCESS) {
		em_recorder_close(out);
		return em_slerr("GetInterface RECORD", r);
	}
	r = (*obj)->GetInterface(obj, e->iid_absq, &out->queue);
	if (r != SL_RESULT_SUCCESS) {
		em_recorder_close(out);
		return em_slerr("GetInterface ANDROIDSIMPLEBUFFERQUEUE (recorder)", r);
	}
	r = (*out->queue)->RegisterCallback(out->queue, em_recorder_cb, (void *)(intptr_t)ctx);
	if (r != SL_RESULT_SUCCESS) {
		em_recorder_close(out);
		return em_slerr("RegisterCallback (recorder)", r);
	}

	out->bufs = em_alloc_bufs(nbuf, bufBytes);
	if (!out->bufs) {
		em_recorder_close(out);
		return em_dup("out of memory allocating recorder buffers");
	}
	for (int i = 0; i < nbuf; i++) {
		r = (*out->queue)->Enqueue(out->queue, out->bufs[i], (SLuint32)bufBytes);
		if (r != SL_RESULT_SUCCESS) {
			em_recorder_close(out);
			return em_slerr("Enqueue (recorder prime)", r);
		}
	}
	r = (*out->record)->SetRecordState(out->record, SL_RECORDSTATE_RECORDING);
	if (r != SL_RESULT_SUCCESS) {
		em_recorder_close(out);
		return em_slerr("SetRecordState RECORDING", r);
	}
	return NULL;
}

static void *em_recorder_bufptr(em_recorder *r, int idx) { return r->bufs[idx]; }

static char *em_recorder_enqueue(em_recorder *r, int idx) {
	SLresult res = (*r->queue)->Enqueue(r->queue, r->bufs[idx], (SLuint32)r->bufBytes);
	if (res != SL_RESULT_SUCCESS) return em_slerr("Enqueue (recorder)", res);
	return NULL;
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

static void em_player_close(em_player *p) {
	if (!p->obj) return;
	if (p->play) (*p->play)->SetPlayState(p->play, SL_PLAYSTATE_STOPPED);
	/* As for the recorder: Destroy waits out a callback in progress. */
	(*p->obj)->Destroy(p->obj);
	em_free_bufs(p->bufs, p->nbuf);
	memset(p, 0, sizeof(*p));
}

/* em_player_open creates a mono S16 AudioPlayer feeding the engine's
 * OutputMix on SL_ANDROID_STREAM_MEDIA, Android's STREAM_MUSIC: the stream
 * whose index is the Show's volume authority (port plan decision 8), so the
 * volume keys, the kiosk and EchoMuse act on the same gain. The stream type
 * is a pre-Realize configuration item. Whatever plays through AudioFlinger
 * is ASP's echo reference (docs/echo-show-crown.md §4.1).
 *
 * Playback is set running with an empty queue: a PAUSED player never fires
 * buffer-queue callbacks, and the render mixer writes continuously
 * (silence included), so the queue only runs dry when the writer stalls. */
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

	const SLInterfaceID iids[2] = { e->iid_absq, e->iid_androidconfig };
	const SLboolean req[2] = { SL_BOOLEAN_TRUE, SL_BOOLEAN_TRUE };

	SLObjectItf obj = NULL;
	SLresult r = (*e->itf)->CreateAudioPlayer(e->itf, &obj, &src, &snk, 2, iids, req);
	if (r != SL_RESULT_SUCCESS) return em_slerr("CreateAudioPlayer", r);
	out->obj = obj;
	out->nbuf = nbuf;
	out->bufBytes = bufBytes;

	SLAndroidConfigurationItf config = NULL;
	r = (*obj)->GetInterface(obj, e->iid_androidconfig, &config);
	if (r != SL_RESULT_SUCCESS) {
		em_player_close(out);
		return em_slerr("GetInterface ANDROIDCONFIGURATION (player)", r);
	}
	SLint32 stream = SL_ANDROID_STREAM_MEDIA;
	r = (*config)->SetConfiguration(config, SL_ANDROID_KEY_STREAM_TYPE, &stream, sizeof(stream));
	if (r != SL_RESULT_SUCCESS) {
		em_player_close(out);
		return em_slerr("SetConfiguration STREAM_TYPE", r);
	}

	r = (*obj)->Realize(obj, SL_BOOLEAN_FALSE);
	if (r != SL_RESULT_SUCCESS) {
		em_player_close(out);
		return em_slerr("player Realize", r);
	}
	r = (*obj)->GetInterface(obj, e->iid_play, &out->play);
	if (r != SL_RESULT_SUCCESS) {
		em_player_close(out);
		return em_slerr("GetInterface PLAY", r);
	}
	r = (*obj)->GetInterface(obj, e->iid_absq, &out->queue);
	if (r != SL_RESULT_SUCCESS) {
		em_player_close(out);
		return em_slerr("GetInterface ANDROIDSIMPLEBUFFERQUEUE (player)", r);
	}
	r = (*out->queue)->RegisterCallback(out->queue, em_player_cb, (void *)(intptr_t)ctx);
	if (r != SL_RESULT_SUCCESS) {
		em_player_close(out);
		return em_slerr("RegisterCallback (player)", r);
	}

	out->bufs = em_alloc_bufs(nbuf, bufBytes);
	if (!out->bufs) {
		em_player_close(out);
		return em_dup("out of memory allocating player buffers");
	}
	r = (*out->play)->SetPlayState(out->play, SL_PLAYSTATE_PLAYING);
	if (r != SL_RESULT_SUCCESS) {
		em_player_close(out);
		return em_slerr("SetPlayState PLAYING", r);
	}
	return NULL;
}

static void *em_player_bufptr(em_player *p, int idx) { return p->bufs[idx]; }

static char *em_player_enqueue(em_player *p, int idx) {
	SLresult res = (*p->queue)->Enqueue(p->queue, p->bufs[idx], (SLuint32)p->bufBytes);
	if (res != SL_RESULT_SUCCESS) return em_slerr("Enqueue (player)", res);
	return NULL;
}

#endif /* EM_OPENSL_SHIM_H */
