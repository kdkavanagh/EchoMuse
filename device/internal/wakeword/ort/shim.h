/*
 * shim.h — C half of the ONNX Runtime binding.
 *
 * ORT's C API is a table of function pointers, which cgo cannot call, so each
 * call has a wrapper here. The library is dlopen'd from a hash-named speech
 * asset (SPEC §16.5), never linked.
 *
 * Every function returns NULL on success or a malloc'd message that the Go
 * side converts and frees. OrtStatus never escapes this file.
 */
#ifndef EM_ORT_SHIM_H
#define EM_ORT_SHIM_H

#include <dlfcn.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "onnxruntime_c_api.h"

typedef const OrtApiBase *(*em_getapibase_fn)(void);

typedef struct {
	void *dl;
	const OrtApi *api;
	OrtEnv *env;
} em_runtime;

typedef struct {
	const OrtApi *api;
	OrtSession *sess;
	OrtMemoryInfo *mem;
	char *in_name;
	char *out_name;
	int64_t in_len;    /* last input dimension; -1 when dynamic */
	int64_t out_count; /* last output dimension; -1 when dynamic */
	int xnnpack;       /* whether the XNNPACK provider attached */
} em_model;

static char *em_dup(const char *s) {
	size_t n = strlen(s) + 1;
	char *p = (char *)malloc(n);
	if (p) memcpy(p, s, n);
	return p;
}

static char *em_err(const OrtApi *api, OrtStatus *st) {
	if (!st) return NULL;
	char *msg = em_dup(api->GetErrorMessage(st));
	api->ReleaseStatus(st);
	return msg ? msg : em_dup("onnxruntime: error (out of memory copying message)");
}

/* em_ort_open dlopens the runtime and creates its environment. RTLD_LOCAL:
 * the library statically links libc++, which must not leak into the process
 * symbol namespace. */
static char *em_ort_open(const char *path, em_runtime *out) {
	memset(out, 0, sizeof(*out));
	void *dl = dlopen(path, RTLD_NOW | RTLD_LOCAL);
	if (!dl) {
		const char *e = dlerror();
		return em_dup(e ? e : "dlopen failed");
	}
	em_getapibase_fn base_fn = (em_getapibase_fn)dlsym(dl, "OrtGetApiBase");
	if (!base_fn) {
		dlclose(dl);
		return em_dup("OrtGetApiBase not found");
	}
	const OrtApiBase *base = base_fn();
	const OrtApi *api = base ? base->GetApi(ORT_API_VERSION) : NULL;
	if (!api) {
		char msg[160];
		snprintf(msg, sizeof(msg), "onnxruntime %s does not provide C API version %d",
		         base ? base->GetVersionString() : "?", ORT_API_VERSION);
		dlclose(dl);
		return em_dup(msg);
	}
	OrtEnv *env = NULL;
	char *err = em_err(api, api->CreateEnv(ORT_LOGGING_LEVEL_ERROR, "echomuse", &env));
	if (err) {
		dlclose(dl);
		return err;
	}
	out->dl = dl;
	out->api = api;
	out->env = env;
	return NULL;
}

static const char *em_ort_version(em_runtime *r) {
	em_getapibase_fn base_fn = (em_getapibase_fn)dlsym(r->dl, "OrtGetApiBase");
	return base_fn ? base_fn()->GetVersionString() : "";
}

/* em_io_dim reads a rank-2 float tensor's last dimension. */
static char *em_io_dim(const OrtApi *api, OrtTypeInfo *ti, int64_t *dim) {
	const OrtTensorTypeAndShapeInfo *tsi = NULL;
	char *err = em_err(api, api->CastTypeInfoToTensorInfo(ti, &tsi));
	if (err) return err;
	if (!tsi) return em_dup("graph input/output is not a tensor");
	ONNXTensorElementDataType et;
	size_t rank = 0;
	if ((err = em_err(api, api->GetTensorElementType(tsi, &et))) ||
	    (err = em_err(api, api->GetDimensionsCount(tsi, &rank))))
		return err;
	if (et != ONNX_TENSOR_ELEMENT_DATA_TYPE_FLOAT) return em_dup("graph input/output is not float32");
	if (rank != 2) return em_dup("graph input/output is not rank 2");
	int64_t dims[2];
	if ((err = em_err(api, api->GetDimensions(tsi, dims, 2)))) return err;
	*dim = dims[1];
	return NULL;
}

static void em_model_free(em_model *m);

/* em_model_load creates one session with the SPEC §16.5 options: one
 * intra-op thread, spinning off, sequential execution, ORT_ENABLE_ALL and the
 * XNNPACK provider, which the Android build compiles in but exposes only
 * through the string-keyed provider API. require_xnnpack=0 admits a runtime
 * built without it (host tests only). */
static char *em_model_load(em_runtime *r, const char *path, int require_xnnpack, em_model *out) {
	const OrtApi *api = r->api;
	memset(out, 0, sizeof(*out));
	out->api = api;

	OrtSessionOptions *o = NULL;
	char *err = em_err(api, api->CreateSessionOptions(&o));
	if (err) return err;
	if ((err = em_err(api, api->SetIntraOpNumThreads(o, 1))) ||
	    (err = em_err(api, api->SetSessionExecutionMode(o, ORT_SEQUENTIAL))) ||
	    (err = em_err(api, api->SetSessionGraphOptimizationLevel(o, ORT_ENABLE_ALL))) ||
	    (err = em_err(api, api->AddSessionConfigEntry(o, "session.intra_op.allow_spinning", "0"))) ||
	    (err = em_err(api, api->AddSessionConfigEntry(o, "session.inter_op.allow_spinning", "0")))) {
		api->ReleaseSessionOptions(o);
		return err;
	}
	const char *keys[] = {"intra_op_num_threads"};
	const char *vals[] = {"1"};
	OrtStatus *st = api->SessionOptionsAppendExecutionProvider(o, "XNNPACK", keys, vals, 1);
	if (st) {
		if (require_xnnpack) {
			err = em_err(api, st);
			api->ReleaseSessionOptions(o);
			return err;
		}
		api->ReleaseStatus(st);
	} else {
		out->xnnpack = 1;
	}

	OrtSession *sess = NULL;
	err = em_err(api, api->CreateSession(r->env, path, o, &sess));
	api->ReleaseSessionOptions(o);
	if (err) return err;
	out->sess = sess;

	size_t n_in = 0, n_out = 0;
	if ((err = em_err(api, api->SessionGetInputCount(sess, &n_in))) ||
	    (err = em_err(api, api->SessionGetOutputCount(sess, &n_out))))
		goto fail;
	if (n_in != 1 || n_out != 1) {
		err = em_dup("graph must have exactly one input and one output");
		goto fail;
	}

	OrtTypeInfo *ti = NULL;
	if ((err = em_err(api, api->SessionGetInputTypeInfo(sess, 0, &ti)))) goto fail;
	err = em_io_dim(api, ti, &out->in_len);
	api->ReleaseTypeInfo(ti);
	if (err) goto fail;
	if ((err = em_err(api, api->SessionGetOutputTypeInfo(sess, 0, &ti)))) goto fail;
	err = em_io_dim(api, ti, &out->out_count);
	api->ReleaseTypeInfo(ti);
	if (err) goto fail;

	OrtAllocator *al = NULL;
	if ((err = em_err(api, api->GetAllocatorWithDefaultOptions(&al))) ||
	    (err = em_err(api, api->SessionGetInputName(sess, 0, al, &out->in_name))) ||
	    (err = em_err(api, api->SessionGetOutputName(sess, 0, al, &out->out_name))) ||
	    (err = em_err(api, api->CreateCpuMemoryInfo(OrtArenaAllocator, OrtMemTypeDefault, &out->mem))))
		goto fail;
	return NULL;

fail:
	em_model_free(out);
	return err;
}

/* em_model_run executes one inference. `data` is Go memory read for the
 * duration of the call; the output is copied into the Go buffer `dst` of
 * capacity `cap`, so no C allocation outlives the call. */
static char *em_model_run(em_model *m, const float *data, int64_t n_in,
                          float *dst, size_t cap, size_t *out_n) {
	const OrtApi *api = m->api;
	*out_n = 0;
	int64_t shape[2] = {1, n_in};
	OrtValue *in = NULL, *out = NULL;
	char *err = em_err(api, api->CreateTensorWithDataAsOrtValue(
	                            m->mem, (void *)data, (size_t)n_in * sizeof(float), shape, 2,
	                            ONNX_TENSOR_ELEMENT_DATA_TYPE_FLOAT, &in));
	if (err) return err;
	err = em_err(api, api->Run(m->sess, NULL, (const char *const *)&m->in_name,
	                           (const OrtValue *const *)&in, 1,
	                           (const char *const *)&m->out_name, 1, &out));
	api->ReleaseValue(in);
	if (err) return err;

	OrtTensorTypeAndShapeInfo *ti = NULL;
	size_t count = 0;
	float *src = NULL;
	if (!(err = em_err(api, api->GetTensorTypeAndShape(out, &ti)))) {
		err = em_err(api, api->GetTensorShapeElementCount(ti, &count));
		api->ReleaseTensorTypeAndShapeInfo(ti);
	}
	if (!err) err = em_err(api, api->GetTensorMutableData(out, (void **)&src));
	if (!err && count > cap) err = em_dup("graph output larger than expected");
	if (!err) {
		memcpy(dst, src, count * sizeof(float));
		*out_n = count;
	}
	api->ReleaseValue(out);
	return err;
}

static void em_model_free(em_model *m) {
	const OrtApi *api = m->api;
	if (!api) return;
	if (m->sess) api->ReleaseSession(m->sess);
	if (m->mem) api->ReleaseMemoryInfo(m->mem);
	OrtAllocator *al = NULL;
	OrtStatus *st = api->GetAllocatorWithDefaultOptions(&al);
	if (st) {
		api->ReleaseStatus(st);
	} else {
		if (m->in_name) api->ReleaseStatus(api->AllocatorFree(al, m->in_name));
		if (m->out_name) api->ReleaseStatus(api->AllocatorFree(al, m->out_name));
	}
	memset(m, 0, sizeof(*m));
}

#endif /* EM_ORT_SHIM_H */
