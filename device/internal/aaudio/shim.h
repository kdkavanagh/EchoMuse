/*
 * shim.h — the C half of the AAudio binding.
 *
 * libaaudio.so is dlopen'd at runtime, not linked: the firmware targets API
 * 22 (AAudio arrived in API 26) and the one binary must also start on the
 * Dot and on the Fire OS Show, which have no libaaudio.so. The NDK's
 * <aaudio/AAudio.h> is included for its types and constants only; every
 * function is called through a pointer resolved with dlsym, under an em_aa_
 * name so nothing references the real symbols.
 *
 * Only the blocking API is used (read/write with a timeout); there are no
 * callbacks, so no AAudio thread ever calls into Go.
 *
 * Error convention, matching internal/opensl: a function that can fail
 * returns char* — NULL on success, or a malloc'd message the Go side turns
 * into an error and frees.
 */
#ifndef EM_AAUDIO_SHIM_H
#define EM_AAUDIO_SHIM_H

#include <dlfcn.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#include <aaudio/AAudio.h>

typedef aaudio_result_t (*em_aa_createBuilder_fn)(AAudioStreamBuilder **);
typedef void (*em_aa_builderSetInt_fn)(AAudioStreamBuilder *, int32_t);
typedef aaudio_result_t (*em_aa_openStream_fn)(AAudioStreamBuilder *, AAudioStream **);
typedef aaudio_result_t (*em_aa_builderDelete_fn)(AAudioStreamBuilder *);
typedef aaudio_result_t (*em_aa_streamOp_fn)(AAudioStream *);
typedef aaudio_result_t (*em_aa_io_fn)(AAudioStream *, void *, int32_t, int64_t);
typedef aaudio_result_t (*em_aa_write_fn)(AAudioStream *, const void *, int32_t, int64_t);
typedef aaudio_result_t (*em_aa_timestamp_fn)(AAudioStream *, clockid_t, int64_t *, int64_t *);
typedef int64_t (*em_aa_get64_fn)(AAudioStream *);
typedef int32_t (*em_aa_get32_fn)(AAudioStream *);
typedef aaudio_result_t (*em_aa_set32_fn)(AAudioStream *, int32_t);
typedef const char *(*em_aa_text_fn)(aaudio_result_t);

/* em_aa_lib is the dlopen'd library and its entry points. */
typedef struct {
	void *dl;
	em_aa_createBuilder_fn createBuilder;
	em_aa_builderSetInt_fn setDirection, setSharingMode, setPerformanceMode, setFormat,
		setSampleRate, setChannelCount, setInputPreset, setUsage, setContentType;
	em_aa_openStream_fn openStream;
	em_aa_builderDelete_fn builderDelete;
	em_aa_streamOp_fn requestStart, requestStop, close;
	em_aa_io_fn read;
	em_aa_write_fn write;
	em_aa_timestamp_fn getTimestamp;
	em_aa_get64_fn getFramesRead, getFramesWritten;
	em_aa_get32_fn getXRunCount, getState, getSampleRate, getChannelCount, getFormat,
		getBufferSizeInFrames, getBufferCapacityInFrames, getFramesPerBurst, getPerformanceMode;
	em_aa_set32_fn setBufferSizeInFrames;
	em_aa_text_fn resultText;
} em_aa_lib;

static char *em_aa_dup(const char *s) {
	size_t n = strlen(s) + 1;
	char *p = (char *)malloc(n);
	if (p) memcpy(p, s, n);
	return p;
}

#define EM_AA_SYM(field, name)                                                        \
	do {                                                                          \
		*(void **)(&l->field) = dlsym(l->dl, name);                           \
		if (!l->field) {                                                      \
			char msg[160];                                                \
			snprintf(msg, sizeof(msg), "dlsym %s: not found", name);      \
			dlclose(l->dl);                                               \
			l->dl = NULL;                                                 \
			return em_aa_dup(msg);                                        \
		}                                                                     \
	} while (0)

/* em_aa_load dlopens path and resolves every entry point the binding uses. */
static char *em_aa_load(const char *path, em_aa_lib *l) {
	memset(l, 0, sizeof(*l));
	l->dl = dlopen(path, RTLD_NOW | RTLD_LOCAL);
	if (!l->dl) {
		const char *e = dlerror();
		char msg[256];
		snprintf(msg, sizeof(msg), "dlopen: %s", e ? e : "unknown error");
		return em_aa_dup(msg);
	}
	EM_AA_SYM(createBuilder, "AAudio_createStreamBuilder");
	EM_AA_SYM(setDirection, "AAudioStreamBuilder_setDirection");
	EM_AA_SYM(setSharingMode, "AAudioStreamBuilder_setSharingMode");
	EM_AA_SYM(setPerformanceMode, "AAudioStreamBuilder_setPerformanceMode");
	EM_AA_SYM(setFormat, "AAudioStreamBuilder_setFormat");
	EM_AA_SYM(setSampleRate, "AAudioStreamBuilder_setSampleRate");
	EM_AA_SYM(setChannelCount, "AAudioStreamBuilder_setChannelCount");
	EM_AA_SYM(setInputPreset, "AAudioStreamBuilder_setInputPreset");
	EM_AA_SYM(setUsage, "AAudioStreamBuilder_setUsage");
	EM_AA_SYM(setContentType, "AAudioStreamBuilder_setContentType");
	EM_AA_SYM(openStream, "AAudioStreamBuilder_openStream");
	EM_AA_SYM(builderDelete, "AAudioStreamBuilder_delete");
	EM_AA_SYM(requestStart, "AAudioStream_requestStart");
	EM_AA_SYM(requestStop, "AAudioStream_requestStop");
	EM_AA_SYM(close, "AAudioStream_close");
	EM_AA_SYM(read, "AAudioStream_read");
	EM_AA_SYM(write, "AAudioStream_write");
	EM_AA_SYM(getTimestamp, "AAudioStream_getTimestamp");
	EM_AA_SYM(getFramesRead, "AAudioStream_getFramesRead");
	EM_AA_SYM(getFramesWritten, "AAudioStream_getFramesWritten");
	EM_AA_SYM(getXRunCount, "AAudioStream_getXRunCount");
	EM_AA_SYM(getState, "AAudioStream_getState");
	EM_AA_SYM(getSampleRate, "AAudioStream_getSampleRate");
	EM_AA_SYM(getChannelCount, "AAudioStream_getChannelCount");
	EM_AA_SYM(getFormat, "AAudioStream_getFormat");
	EM_AA_SYM(getBufferSizeInFrames, "AAudioStream_getBufferSizeInFrames");
	EM_AA_SYM(getBufferCapacityInFrames, "AAudioStream_getBufferCapacityInFrames");
	EM_AA_SYM(getFramesPerBurst, "AAudioStream_getFramesPerBurst");
	EM_AA_SYM(getPerformanceMode, "AAudioStream_getPerformanceMode");
	EM_AA_SYM(setBufferSizeInFrames, "AAudioStream_setBufferSizeInFrames");
	EM_AA_SYM(resultText, "AAudio_convertResultToText");
	return NULL;
}

static char *em_aa_err(const em_aa_lib *l, const char *what, aaudio_result_t r) {
	char msg[192];
	snprintf(msg, sizeof(msg), "%s: %d %s", what, (int)r, l->resultText ? l->resultText(r) : "");
	return em_aa_dup(msg);
}

/* em_aa_open opens a shared, PCM I16, PERFORMANCE_MODE_NONE stream: input
 * with the given preset, or output with the given usage (content type left
 * at AAudio's default). bufferFrames > 0 asks an output stream to keep at
 * most that many frames queued. The stream is not started. */
static char *em_aa_open(const em_aa_lib *l, int input, int32_t rate, int32_t channels,
                        int32_t presetOrUsage, int32_t bufferFrames, AAudioStream **out) {
	AAudioStreamBuilder *b = NULL;
	aaudio_result_t r = l->createBuilder(&b);
	if (r != AAUDIO_OK) return em_aa_err(l, "AAudio_createStreamBuilder", r);
	l->setDirection(b, input ? AAUDIO_DIRECTION_INPUT : AAUDIO_DIRECTION_OUTPUT);
	l->setSharingMode(b, AAUDIO_SHARING_MODE_SHARED);
	l->setPerformanceMode(b, AAUDIO_PERFORMANCE_MODE_NONE);
	l->setFormat(b, AAUDIO_FORMAT_PCM_I16);
	l->setSampleRate(b, rate);
	l->setChannelCount(b, channels);
	if (input) {
		l->setInputPreset(b, presetOrUsage);
	} else {
		l->setUsage(b, presetOrUsage);
	}
	AAudioStream *s = NULL;
	r = l->openStream(b, &s);
	l->builderDelete(b);
	if (r != AAUDIO_OK) return em_aa_err(l, "AAudioStreamBuilder_openStream", r);
	if (l->getFormat(s) != AAUDIO_FORMAT_PCM_I16 || l->getSampleRate(s) != rate ||
	    l->getChannelCount(s) != channels) {
		char msg[160];
		snprintf(msg, sizeof(msg), "stream opened as format %d, %d Hz, %d ch; want I16, %d Hz, %d ch",
		         (int)l->getFormat(s), (int)l->getSampleRate(s), (int)l->getChannelCount(s),
		         (int)rate, (int)channels);
		l->close(s);
		return em_aa_dup(msg);
	}
	if (!input && bufferFrames > 0) {
		r = l->setBufferSizeInFrames(s, bufferFrames);
		if (r < 0) {
			char *e = em_aa_err(l, "AAudioStream_setBufferSizeInFrames", r);
			l->close(s);
			return e;
		}
	}
	*out = s;
	return NULL;
}

static char *em_aa_start(const em_aa_lib *l, AAudioStream *s) {
	aaudio_result_t r = l->requestStart(s);
	return r == AAUDIO_OK ? NULL : em_aa_err(l, "AAudioStream_requestStart", r);
}

/* em_aa_stop asks the stream to stop; safe from any thread, and a blocked
 * read or write then returns. */
static void em_aa_stop(const em_aa_lib *l, AAudioStream *s) { l->requestStop(s); }

/* em_aa_close releases the stream. No read or write may be in progress. */
static void em_aa_close(const em_aa_lib *l, AAudioStream *s) { l->close(s); }

static int32_t em_aa_read(const em_aa_lib *l, AAudioStream *s, void *buf, int32_t frames, int64_t timeoutNs) {
	return l->read(s, buf, frames, timeoutNs);
}

static int32_t em_aa_write(const em_aa_lib *l, AAudioStream *s, const void *buf, int32_t frames, int64_t timeoutNs) {
	return l->write(s, buf, frames, timeoutNs);
}

/* em_aa_timestamp reads the stream's latest (frame position, CLOCK_MONOTONIC
 * ns) pair; it returns non-zero when AAudio has none to give. */
static int32_t em_aa_timestamp(const em_aa_lib *l, AAudioStream *s, int64_t *pos, int64_t *ns) {
	return l->getTimestamp(s, CLOCK_MONOTONIC, pos, ns);
}

static int64_t em_aa_frames_read(const em_aa_lib *l, AAudioStream *s) { return l->getFramesRead(s); }
static int64_t em_aa_frames_written(const em_aa_lib *l, AAudioStream *s) { return l->getFramesWritten(s); }
static int32_t em_aa_xruns(const em_aa_lib *l, AAudioStream *s) { return l->getXRunCount(s); }
static int32_t em_aa_state(const em_aa_lib *l, AAudioStream *s) { return l->getState(s); }
static int32_t em_aa_buffer_size(const em_aa_lib *l, AAudioStream *s) { return l->getBufferSizeInFrames(s); }
static int32_t em_aa_buffer_capacity(const em_aa_lib *l, AAudioStream *s) { return l->getBufferCapacityInFrames(s); }
static int32_t em_aa_burst(const em_aa_lib *l, AAudioStream *s) { return l->getFramesPerBurst(s); }
static int32_t em_aa_perf_mode(const em_aa_lib *l, AAudioStream *s) { return l->getPerformanceMode(s); }

static const char *em_aa_text(const em_aa_lib *l, int32_t r) { return l->resultText(r); }

#endif
