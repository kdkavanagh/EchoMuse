/*
 * slprobe: OpenSL ES capture/playback probe for Fire OS devices with the
 * Android framework (Echo Show 8, `crown`). It rebuilds the throwaway program
 * of docs/echo-show-crown.md §4.4 so the Phase 0 measurements of
 * docs/echo-show-port-plan.md can be repeated.
 *
 * Capture: SLDataLocator_IODevice -> SLAndroidSimpleBufferQueue, 16-bit mono
 * at the chosen rate (default 16 kHz), with SL_ANDROID_KEY_RECORDING_PRESET
 * set before Realize. Every buffer completion is stamped with CLOCK_MONOTONIC
 * in the callback. Samples go to PREFIX.raw (S16LE mono), stamps to PREFIX.ts.
 *
 * Playback (optional, -i): an OpenSL buffer-queue player on the chosen
 * SL_ANDROID_KEY_STREAM_TYPE plays a WAV (PCM16) or raw S16LE stimulus,
 * starting -D ms after SetRecordState(RECORDING). Every enqueue is stamped.
 *
 * PREFIX.ts is text, one record per line:
 *   # key=value ...            header and notes
 *   T0 <ns>                    SetRecordState(RECORDING) returned
 *   R <seq> <ns>               recorder buffer <seq> completed (seq from 0;
 *                              it holds samples [seq*frames, (seq+1)*frames))
 *   P <frame> <frames> <ns>    player enqueued stimulus frames [frame, +frames)
 *   PS <ns> / PE <ns>          player started / stimulus exhausted
 *   E <event-mask> <ns>        SLRecordItf event (0x10 = HEADSTALLED, overrun)
 *   X <what> [...] <ns>        main thread: stall, resumed, restart (-x, with
 *                              the three SLresults), overflow
 *
 * The callbacks only copy into preallocated rings; the main thread flushes
 * them to disk every 20 ms, prints a status line per second (completions,
 * RMS, digital-zero fraction) and stops on -d, SIGINT or SIGTERM.
 *
 * Build: device/tools/build_tools.sh (NDK 21.4, armv7a-linux-androideabi22,
 * -lOpenSLES). Decode: device/tools/slprobe/decode.py.
 */
#include <errno.h>
#include <getopt.h>
#include <math.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

#include <SLES/OpenSLES.h>
#include <SLES/OpenSLES_Android.h>
#include <SLES/OpenSLES_AndroidConfiguration.h>

#define REC_RING 4096 /* recorder buffers held for the flusher */
#define PLAY_RING 4096 /* player enqueue stamps held for the flusher */
#define EVT_RING 256

static int64_t now_ns(void) {
	struct timespec ts;
	clock_gettime(CLOCK_MONOTONIC, &ts);
	return (int64_t)ts.tv_sec * 1000000000LL + ts.tv_nsec;
}

static volatile sig_atomic_t g_stop;
static void on_signal(int sig) { (void)sig; g_stop = 1; }

/* ─── options ─────────────────────────────────────────────────────────────── */

struct opts {
	int preset;          /* SL_ANDROID_RECORDING_PRESET_*, or -1 to leave unset */
	int rate;            /* capture rate, Hz */
	int frames;          /* frames per capture buffer */
	int nbuf;            /* capture buffers in the queue */
	double seconds;      /* 0 = until SIGINT/SIGTERM */
	const char *prefix;  /* output path prefix */
	const char *stim;    /* stimulus file or NULL */
	int stream;          /* SL_ANDROID_STREAM_* for the player */
	int delay_ms;        /* player start after RECORDING */
	int raw_rate, raw_ch;/* raw stimulus format */
	int play_ms, play_n; /* player buffer length and count */
	int loop;            /* loop the stimulus */
	int no_capture;      /* playback only */
	int quiet;
	int restart_ms;      /* -x: cycle the recorder while stalled, 0 = never */
};

static int parse_preset(const char *s) {
	static const struct { const char *name; int v; } t[] = {
		{"none", -1}, {"generic", 1}, {"mic", 1}, {"camcorder", 2},
		{"vr", 3}, {"voice_recognition", 3}, {"vc", 4}, {"voice_communication", 4},
		{"unprocessed", 5},
	};
	for (size_t i = 0; i < sizeof(t) / sizeof(t[0]); i++)
		if (!strcmp(s, t[i].name)) return t[i].v;
	char *end;
	long v = strtol(s, &end, 0);
	if (*end || v < 0 || v > 0xffff) return -2;
	return (int)v;
}

static int parse_stream(const char *s) {
	static const struct { const char *name; int v; } t[] = {
		{"voice", 0}, {"system", 1}, {"ring", 2}, {"music", 3}, {"media", 3},
		{"alarm", 4}, {"notification", 5},
	};
	for (size_t i = 0; i < sizeof(t) / sizeof(t[0]); i++)
		if (!strcmp(s, t[i].name)) return t[i].v;
	char *end;
	long v = strtol(s, &end, 0);
	if (*end || v < 0 || v > 64) return -1;
	return (int)v;
}

static void usage(const char *argv0) {
	fprintf(stderr,
		"usage: %s [options]\n"
		"  -p PRESET   recording preset: vr (default), generic|mic, camcorder, vc,\n"
		"              unprocessed, none (not set), or a number\n"
		"  -r HZ       capture rate (16000)\n"
		"  -f FRAMES   frames per capture buffer (256)\n"
		"  -n COUNT    capture buffers queued (8)\n"
		"  -d SECONDS  duration, 0 = until SIGINT/SIGTERM (10)\n"
		"  -o PREFIX   output prefix: PREFIX.raw, PREFIX.ts (/data/local/tmp/slprobe);\n"
		"              none = no files, status lines only\n"
		"  -i FILE     play FILE: WAV PCM16, or raw S16LE (see -R, -C)\n"
		"  -s STREAM   player stream: music (default), alarm, voice, system, ring,\n"
		"              notification, or a number\n"
		"  -D MS       player start after RECORDING (2000)\n"
		"  -R HZ       raw stimulus rate (48000)\n"
		"  -C CH       raw stimulus channels (2)\n"
		"  -b MS       player buffer length (20)\n"
		"  -B COUNT    player buffers queued (4)\n"
		"  -L          loop the stimulus until the end of the run\n"
		"  -N          no capture (playback only)\n"
		"  -q          no per-second status\n"
		"  -x MS       while stalled, cycle the recorder (stop, clear, record) every MS\n",
		argv0);
}

/* ─── stimulus ────────────────────────────────────────────────────────────── */

struct stim {
	int16_t *pcm;  /* interleaved */
	size_t frames;
	int ch, rate;
};

static uint32_t le32(const uint8_t *p) { return p[0] | p[1] << 8 | p[2] << 16 | (uint32_t)p[3] << 24; }
static uint16_t le16(const uint8_t *p) { return (uint16_t)(p[0] | p[1] << 8); }

static int load_stim(const char *path, int raw_rate, int raw_ch, struct stim *out) {
	FILE *f = fopen(path, "rb");
	if (!f) { fprintf(stderr, "open %s: %s\n", path, strerror(errno)); return -1; }
	fseek(f, 0, SEEK_END);
	long len = ftell(f);
	fseek(f, 0, SEEK_SET);
	uint8_t *buf = malloc((size_t)len);
	if (!buf || fread(buf, 1, (size_t)len, f) != (size_t)len) {
		fprintf(stderr, "read %s failed\n", path);
		fclose(f);
		free(buf);
		return -1;
	}
	fclose(f);

	const uint8_t *data = buf;
	size_t data_len = (size_t)len;
	int ch = raw_ch, rate = raw_rate;
	if (len >= 12 && !memcmp(buf, "RIFF", 4) && !memcmp(buf + 8, "WAVE", 4)) {
		size_t off = 12;
		int have_fmt = 0;
		data = NULL;
		while (off + 8 <= (size_t)len) {
			uint32_t sz = le32(buf + off + 4);
			const uint8_t *body = buf + off + 8;
			if (!memcmp(buf + off, "fmt ", 4) && sz >= 16) {
				uint16_t fmt = le16(body), bits = le16(body + 14);
				ch = le16(body + 2);
				rate = (int)le32(body + 4);
				if ((fmt != 1 && fmt != 0xfffe) || bits != 16) {
					fprintf(stderr, "%s: only PCM16 WAV is supported\n", path);
					free(buf);
					return -1;
				}
				have_fmt = 1;
			} else if (!memcmp(buf + off, "data", 4)) {
				data = body;
				data_len = sz;
				if (off + 8 + data_len > (size_t)len) data_len = (size_t)len - off - 8;
				break;
			}
			off += 8 + sz + (sz & 1);
		}
		if (!have_fmt || !data) {
			fprintf(stderr, "%s: WAV without fmt/data chunk\n", path);
			free(buf);
			return -1;
		}
	}
	if (ch < 1 || ch > 2) {
		fprintf(stderr, "%s: %d channels; 1 or 2 supported\n", path, ch);
		free(buf);
		return -1;
	}
	out->ch = ch;
	out->rate = rate;
	out->frames = data_len / (size_t)(2 * ch);
	out->pcm = malloc(out->frames * (size_t)(2 * ch) + 1);
	if (!out->pcm) { free(buf); return -1; }
	memcpy(out->pcm, data, out->frames * (size_t)(2 * ch));
	free(buf);
	return 0;
}

/* ─── shared state ────────────────────────────────────────────────────────── */

struct play_stamp { uint32_t frame, frames; int64_t ns; };
struct evt_stamp { uint32_t mask; int64_t ns; };

static struct {
	/* recorder */
	SLAndroidSimpleBufferQueueItf rq;
	int16_t **rbufs;
	int nbuf, frames, buf_bytes;
	int16_t *rring;          /* REC_RING buffers of samples */
	int64_t rts[REC_RING];
	uint32_t rdone;          /* completions; written by the callback only */
	uint32_t rbase;          /* rdone when slot 0 was last enqueued first */
	volatile int rstop;

	/* player */
	SLAndroidSimpleBufferQueueItf pq;
	int16_t **pbufs;
	int pn, pframes;
	struct stim stim;
	size_t pnext;            /* next stimulus frame to enqueue */
	int ploop, pended;
	struct play_stamp pstamps[PLAY_RING];
	uint32_t pdone;
	int64_t pend_ns;

	struct evt_stamp evts[EVT_RING];
	uint32_t edone;
} g;

static void rec_cb(SLAndroidSimpleBufferQueueItf q, void *ctx) {
	(void)ctx;
	int64_t t = now_ns();
	uint32_t k = g.rdone;
	int slot = (int)((k - g.rbase) % (uint32_t)g.nbuf);
	memcpy(g.rring + (size_t)(k % REC_RING) * (size_t)g.frames, g.rbufs[slot], (size_t)g.buf_bytes);
	g.rts[k % REC_RING] = t;
	__atomic_store_n(&g.rdone, k + 1, __ATOMIC_RELEASE);
	if (!g.rstop) (*q)->Enqueue(q, g.rbufs[slot], (SLuint32)g.buf_bytes);
}

static void rec_event_cb(SLRecordItf caller, void *ctx, SLuint32 event) {
	(void)caller;
	(void)ctx;
	uint32_t k = g.edone;
	g.evts[k % EVT_RING] = (struct evt_stamp){event, now_ns()};
	__atomic_store_n(&g.edone, k + 1, __ATOMIC_RELEASE);
}

/* play_fill enqueues the next chunk into slot. Returns 0 when nothing is
 * left to play. */
static int play_fill(int slot) {
	size_t total = g.stim.frames;
	if (g.pnext >= total) {
		if (!g.ploop) return 0;
		g.pnext = 0;
	}
	size_t n = (size_t)g.pframes;
	if (g.pnext + n > total) n = total - g.pnext;
	size_t fb = (size_t)(2 * g.stim.ch);
	memcpy(g.pbufs[slot], g.stim.pcm + g.pnext * (size_t)g.stim.ch, n * fb);
	uint32_t k = g.pdone;
	g.pstamps[k % PLAY_RING] = (struct play_stamp){(uint32_t)g.pnext, (uint32_t)n, now_ns()};
	(*g.pq)->Enqueue(g.pq, g.pbufs[slot], (SLuint32)(n * fb));
	__atomic_store_n(&g.pdone, k + 1, __ATOMIC_RELEASE);
	g.pnext += n;
	return 1;
}

static int g_pslot;
static void play_cb(SLAndroidSimpleBufferQueueItf q, void *ctx) {
	(void)q;
	(void)ctx;
	if (g.pended) return;
	if (!play_fill(g_pslot)) {
		g.pend_ns = now_ns();
		g.pended = 1;
		return;
	}
	g_pslot = (g_pslot + 1) % g.pn;
}

/* ─── main ────────────────────────────────────────────────────────────────── */

#define CHECK(r, what) do { \
	SLresult _r = (r); \
	if (_r != SL_RESULT_SUCCESS) { \
		fprintf(stderr, "%s failed: SLresult 0x%x\n", what, (unsigned)_r); \
		return 1; \
	} \
} while (0)

int main(int argc, char **argv) {
	struct opts o = {
		.preset = 3, .rate = 16000, .frames = 256, .nbuf = 8, .seconds = 10,
		.prefix = "/data/local/tmp/slprobe", .stream = 3, .delay_ms = 2000,
		.raw_rate = 48000, .raw_ch = 2, .play_ms = 20, .play_n = 4,
	};
	int c;
	while ((c = getopt(argc, argv, "p:r:f:n:d:o:i:s:D:R:C:b:B:LNqx:h")) != -1) {
		switch (c) {
		case 'p': o.preset = parse_preset(optarg); break;
		case 'r': o.rate = atoi(optarg); break;
		case 'f': o.frames = atoi(optarg); break;
		case 'n': o.nbuf = atoi(optarg); break;
		case 'd': o.seconds = atof(optarg); break;
		case 'o': o.prefix = optarg; break;
		case 'i': o.stim = optarg; break;
		case 's': o.stream = parse_stream(optarg); break;
		case 'D': o.delay_ms = atoi(optarg); break;
		case 'R': o.raw_rate = atoi(optarg); break;
		case 'C': o.raw_ch = atoi(optarg); break;
		case 'b': o.play_ms = atoi(optarg); break;
		case 'B': o.play_n = atoi(optarg); break;
		case 'L': o.loop = 1; break;
		case 'N': o.no_capture = 1; break;
		case 'q': o.quiet = 1; break;
		case 'x': o.restart_ms = atoi(optarg); break;
		default: usage(argv[0]); return 2;
		}
	}
	if (o.preset == -2 || o.stream < 0 || o.rate <= 0 || o.frames <= 0 || o.nbuf <= 0 ||
	    o.play_ms <= 0 || o.play_n <= 0 || o.seconds < 0 || (o.no_capture && !o.stim)) {
		usage(argv[0]);
		return 2;
	}

	signal(SIGINT, on_signal);
	signal(SIGTERM, on_signal);
	signal(SIGPIPE, SIG_IGN);
	setvbuf(stdout, NULL, _IOLBF, 0); /* status reaches a pipe (logcat, adb) live */

	if (o.stim && load_stim(o.stim, o.raw_rate, o.raw_ch, &g.stim)) return 1;

	/* -o none: status lines only, for contexts that may not write files. */
	int nofiles = !strcmp(o.prefix, "none");
	char path[512];
	snprintf(path, sizeof(path), "%s.ts", o.prefix);
	FILE *ts = fopen(nofiles ? "/dev/null" : path, "w");
	if (!ts) { fprintf(stderr, "open %s: %s\n", path, strerror(errno)); return 1; }
	FILE *raw = NULL;
	if (!o.no_capture) {
		snprintf(path, sizeof(path), "%s.raw", o.prefix);
		raw = fopen(nofiles ? "/dev/null" : path, "wb");
		if (!raw) { fprintf(stderr, "open %s: %s\n", path, strerror(errno)); return 1; }
	}
	fprintf(ts, "# slprobe pid=%d uid=%d preset=%d rate=%d frames=%d nbuf=%d seconds=%g capture=%d\n",
		getpid(), getuid(), o.preset, o.rate, o.frames, o.nbuf, o.seconds, !o.no_capture);
	if (o.stim)
		fprintf(ts, "# stimulus=%s stim_rate=%d stim_ch=%d stim_frames=%zu stream=%d delay_ms=%d play_ms=%d play_n=%d loop=%d\n",
			o.stim, g.stim.rate, g.stim.ch, g.stim.frames, o.stream, o.delay_ms, o.play_ms, o.play_n, o.loop);
	{
		FILE *ctx = fopen("/proc/self/attr/current", "r");
		char label[128] = "?";
		if (ctx) {
			if (!fgets(label, sizeof(label), ctx)) strcpy(label, "?");
			fclose(ctx);
			label[strcspn(label, "\n")] = 0;
		}
		fprintf(ts, "# context=%s\n", label);
		printf("slprobe: pid %d uid %d context %s\n", getpid(), getuid(), label);
	}
	fflush(ts);

	/* Engine and output mix. */
	SLObjectItf engObj;
	SLEngineItf eng;
	CHECK(slCreateEngine(&engObj, 0, NULL, 0, NULL, NULL), "slCreateEngine");
	CHECK((*engObj)->Realize(engObj, SL_BOOLEAN_FALSE), "engine Realize");
	CHECK((*engObj)->GetInterface(engObj, SL_IID_ENGINE, &eng), "GetInterface ENGINE");

	/* Recorder. */
	SLObjectItf recObj = NULL;
	SLRecordItf rec = NULL;
	if (!o.no_capture) {
		SLDataLocator_IODevice locIn = {SL_DATALOCATOR_IODEVICE, SL_IODEVICE_AUDIOINPUT,
			SL_DEFAULTDEVICEID_AUDIOINPUT, NULL};
		SLDataSource src = {&locIn, NULL};
		SLDataLocator_AndroidSimpleBufferQueue locOut = {SL_DATALOCATOR_ANDROIDSIMPLEBUFFERQUEUE,
			(SLuint32)o.nbuf};
		SLDataFormat_PCM fmt = {SL_DATAFORMAT_PCM, 1, (SLuint32)o.rate * 1000,
			SL_PCMSAMPLEFORMAT_FIXED_16, SL_PCMSAMPLEFORMAT_FIXED_16,
			SL_SPEAKER_FRONT_CENTER, SL_BYTEORDER_LITTLEENDIAN};
		SLDataSink snk = {&locOut, &fmt};
		const SLInterfaceID iids[2] = {SL_IID_ANDROIDSIMPLEBUFFERQUEUE, SL_IID_ANDROIDCONFIGURATION};
		const SLboolean req[2] = {SL_BOOLEAN_TRUE, SL_BOOLEAN_TRUE};
		CHECK((*eng)->CreateAudioRecorder(eng, &recObj, &src, &snk, 2, iids, req), "CreateAudioRecorder");
		if (o.preset >= 0) {
			SLAndroidConfigurationItf cfg;
			CHECK((*recObj)->GetInterface(recObj, SL_IID_ANDROIDCONFIGURATION, &cfg),
				"GetInterface ANDROIDCONFIGURATION (recorder)");
			SLuint32 p = (SLuint32)o.preset;
			CHECK((*cfg)->SetConfiguration(cfg, SL_ANDROID_KEY_RECORDING_PRESET, &p, sizeof(p)),
				"SetConfiguration RECORDING_PRESET");
		}
		CHECK((*recObj)->Realize(recObj, SL_BOOLEAN_FALSE), "recorder Realize");
		CHECK((*recObj)->GetInterface(recObj, SL_IID_RECORD, &rec), "GetInterface RECORD");
		CHECK((*recObj)->GetInterface(recObj, SL_IID_ANDROIDSIMPLEBUFFERQUEUE, &g.rq),
			"GetInterface ANDROIDSIMPLEBUFFERQUEUE (recorder)");
		CHECK((*g.rq)->RegisterCallback(g.rq, rec_cb, NULL), "RegisterCallback (recorder)");
		CHECK((*rec)->RegisterCallback(rec, rec_event_cb, NULL), "RegisterCallback (record events)");
		SLresult er = (*rec)->SetCallbackEventsMask(rec, SL_RECORDEVENT_HEADSTALLED);
		if (er != SL_RESULT_SUCCESS)
			fprintf(ts, "# SetCallbackEventsMask(HEADSTALLED) SLresult 0x%x\n", (unsigned)er);

		g.nbuf = o.nbuf;
		g.frames = o.frames;
		g.buf_bytes = o.frames * 2;
		g.rbufs = calloc((size_t)o.nbuf, sizeof(*g.rbufs));
		g.rring = malloc((size_t)REC_RING * (size_t)g.buf_bytes);
		if (!g.rbufs || !g.rring) { fprintf(stderr, "out of memory\n"); return 1; }
		for (int i = 0; i < o.nbuf; i++) {
			g.rbufs[i] = calloc(1, (size_t)g.buf_bytes);
			if (!g.rbufs[i]) { fprintf(stderr, "out of memory\n"); return 1; }
			CHECK((*g.rq)->Enqueue(g.rq, g.rbufs[i], (SLuint32)g.buf_bytes), "Enqueue (recorder prime)");
		}
	}

	/* Player (created now, started after the delay). */
	SLObjectItf mixObj = NULL, playObj = NULL;
	SLPlayItf play = NULL;
	if (o.stim) {
		CHECK((*eng)->CreateOutputMix(eng, &mixObj, 0, NULL, NULL), "CreateOutputMix");
		CHECK((*mixObj)->Realize(mixObj, SL_BOOLEAN_FALSE), "OutputMix Realize");
		SLDataLocator_AndroidSimpleBufferQueue locIn = {SL_DATALOCATOR_ANDROIDSIMPLEBUFFERQUEUE,
			(SLuint32)o.play_n};
		SLDataFormat_PCM fmt = {SL_DATAFORMAT_PCM, (SLuint32)g.stim.ch, (SLuint32)g.stim.rate * 1000,
			SL_PCMSAMPLEFORMAT_FIXED_16, SL_PCMSAMPLEFORMAT_FIXED_16,
			g.stim.ch == 2 ? (SL_SPEAKER_FRONT_LEFT | SL_SPEAKER_FRONT_RIGHT) : SL_SPEAKER_FRONT_CENTER,
			SL_BYTEORDER_LITTLEENDIAN};
		SLDataSource src = {&locIn, &fmt};
		SLDataLocator_OutputMix locOut = {SL_DATALOCATOR_OUTPUTMIX, mixObj};
		SLDataSink snk = {&locOut, NULL};
		const SLInterfaceID iids[2] = {SL_IID_ANDROIDSIMPLEBUFFERQUEUE, SL_IID_ANDROIDCONFIGURATION};
		const SLboolean req[2] = {SL_BOOLEAN_TRUE, SL_BOOLEAN_TRUE};
		CHECK((*eng)->CreateAudioPlayer(eng, &playObj, &src, &snk, 2, iids, req), "CreateAudioPlayer");
		SLAndroidConfigurationItf cfg;
		CHECK((*playObj)->GetInterface(playObj, SL_IID_ANDROIDCONFIGURATION, &cfg),
			"GetInterface ANDROIDCONFIGURATION (player)");
		SLint32 st = (SLint32)o.stream;
		CHECK((*cfg)->SetConfiguration(cfg, SL_ANDROID_KEY_STREAM_TYPE, &st, sizeof(st)),
			"SetConfiguration STREAM_TYPE");
		CHECK((*playObj)->Realize(playObj, SL_BOOLEAN_FALSE), "player Realize");
		CHECK((*playObj)->GetInterface(playObj, SL_IID_PLAY, &play), "GetInterface PLAY");
		CHECK((*playObj)->GetInterface(playObj, SL_IID_ANDROIDSIMPLEBUFFERQUEUE, &g.pq),
			"GetInterface ANDROIDSIMPLEBUFFERQUEUE (player)");
		CHECK((*g.pq)->RegisterCallback(g.pq, play_cb, NULL), "RegisterCallback (player)");
		g.pn = o.play_n;
		g.pframes = g.stim.rate * o.play_ms / 1000;
		g.ploop = o.loop;
		g.pbufs = calloc((size_t)g.pn, sizeof(*g.pbufs));
		if (!g.pbufs) { fprintf(stderr, "out of memory\n"); return 1; }
		for (int i = 0; i < g.pn; i++) {
			g.pbufs[i] = malloc((size_t)g.pframes * (size_t)(2 * g.stim.ch));
			if (!g.pbufs[i]) { fprintf(stderr, "out of memory\n"); return 1; }
		}
	}

	int64_t t0 = now_ns();
	if (rec) {
		CHECK((*rec)->SetRecordState(rec, SL_RECORDSTATE_RECORDING), "SetRecordState RECORDING");
		t0 = now_ns();
		fprintf(ts, "T0 %lld\n", (long long)t0);
	}
	int64_t play_at = o.stim ? t0 + (int64_t)o.delay_ms * 1000000LL : -1;
	int64_t end_at = o.seconds > 0 ? t0 + (int64_t)(o.seconds * 1e9) : INT64_MAX;
	int playing = 0, pend_logged = 0;

	uint32_t rflushed = 0, pflushed = 0, eflushed = 0;
	int64_t last_ns = t0, next_status = t0 + 1000000000LL;
	int stalled = 0;
	int64_t last_restart = t0;
	double sec_sq = 0;
	uint32_t sec_n = 0, sec_zero = 0, sec_bufs = 0;
	uint64_t total_overflow = 0;

	while (!g_stop) {
		int64_t t = now_ns();
		if (t >= end_at) break;
		if (play_at >= 0 && !playing && t >= play_at) {
			fprintf(ts, "PS %lld\n", (long long)now_ns());
			for (int i = 0; i < g.pn; i++) {
				if (!play_fill(g_pslot)) break;
				g_pslot = (g_pslot + 1) % g.pn;
			}
			SLresult r = (*play)->SetPlayState(play, SL_PLAYSTATE_PLAYING);
			if (r != SL_RESULT_SUCCESS) fprintf(ts, "# SetPlayState PLAYING SLresult 0x%x\n", (unsigned)r);
			playing = 1;
		}

		/* Flush recorder completions. */
		uint32_t rd = __atomic_load_n(&g.rdone, __ATOMIC_ACQUIRE);
		if (rd - rflushed > REC_RING) {
			uint32_t lost = rd - rflushed - REC_RING;
			fprintf(ts, "X overflow %u %lld\n", lost, (long long)t);
			total_overflow += lost;
			rflushed = rd - REC_RING;
		}
		for (; rflushed != rd; rflushed++) {
			const int16_t *b = g.rring + (size_t)(rflushed % REC_RING) * (size_t)g.frames;
			fwrite(b, 2, (size_t)g.frames, raw);
			fprintf(ts, "R %u %lld\n", rflushed, (long long)g.rts[rflushed % REC_RING]);
			last_ns = g.rts[rflushed % REC_RING];
			for (int i = 0; i < g.frames; i++) {
				double s = (double)(b[i] & ~1); /* bit 0 carries AFE metadata */
				sec_sq += s * s;
				if (b[i] == 0) sec_zero++;
			}
			sec_n += (uint32_t)g.frames;
			sec_bufs++;
		}
		if (stalled && rd != 0 && last_ns > t - 500000000LL) {
			fprintf(ts, "X resumed %lld\n", (long long)last_ns);
			printf("slprobe: capture resumed\n");
			stalled = 0;
		}
		if (rec && !stalled && t - last_ns > 1000000000LL) {
			fprintf(ts, "X stall %lld\n", (long long)t);
			printf("slprobe: STALL, no capture buffer for %lld ms\n", (long long)((t - last_ns) / 1000000));
			stalled = 1;
		}
		/* -x: cycle the recorder (STOPPED, Clear, re-prime, RECORDING) while
		 * stalled, every restart_ms. Callbacks are idle while stalled. */
		if (rec && stalled && o.restart_ms > 0 && t - last_ns >= (int64_t)o.restart_ms * 1000000LL &&
		    t - last_restart >= (int64_t)o.restart_ms * 1000000LL) {
			SLresult r1 = (*rec)->SetRecordState(rec, SL_RECORDSTATE_STOPPED);
			SLresult r2 = (*g.rq)->Clear(g.rq);
			g.rbase = __atomic_load_n(&g.rdone, __ATOMIC_ACQUIRE);
			for (int i = 0; i < g.nbuf; i++)
				(*g.rq)->Enqueue(g.rq, g.rbufs[i], (SLuint32)g.buf_bytes);
			SLresult r3 = (*rec)->SetRecordState(rec, SL_RECORDSTATE_RECORDING);
			last_restart = now_ns();
			fprintf(ts, "X restart 0x%x/0x%x/0x%x %lld\n", (unsigned)r1, (unsigned)r2, (unsigned)r3,
				(long long)last_restart);
			printf("slprobe: restarted recorder (stop 0x%x clear 0x%x record 0x%x)\n",
				(unsigned)r1, (unsigned)r2, (unsigned)r3);
		}

		uint32_t pd = __atomic_load_n(&g.pdone, __ATOMIC_ACQUIRE);
		for (; pflushed != pd; pflushed++) {
			struct play_stamp *s = &g.pstamps[pflushed % PLAY_RING];
			fprintf(ts, "P %u %u %lld\n", s->frame, s->frames, (long long)s->ns);
		}
		if (g.pended && !pend_logged) {
			fprintf(ts, "PE %lld\n", (long long)g.pend_ns);
			pend_logged = 1;
		}
		uint32_t ed = __atomic_load_n(&g.edone, __ATOMIC_ACQUIRE);
		for (; eflushed != ed; eflushed++) {
			struct evt_stamp *e = &g.evts[eflushed % EVT_RING];
			fprintf(ts, "E 0x%x %lld\n", e->mask, (long long)e->ns);
			printf("slprobe: record event 0x%x\n", e->mask);
		}

		if (t >= next_status) {
			if (!o.quiet) {
				double rms = sec_n ? sqrt(sec_sq / sec_n) : 0;
				double db = rms > 0 ? 20 * log10(rms / 32768.0) : -999;
				printf("[%6.1fs] bufs %4u (+%3u) rms %6.1f dBFS zero %5.1f%%%s%s\n",
					(t - t0) / 1e9, rd, sec_bufs, db,
					sec_n ? 100.0 * sec_zero / sec_n : 0.0,
					playing ? (g.pended ? " play:done" : " play:on") : "",
					stalled ? " STALLED" : "");
				fflush(stdout);
			}
			sec_sq = 0;
			sec_n = sec_zero = sec_bufs = 0;
			next_status += 1000000000LL;
		}
		fflush(ts);
		if (raw) fflush(raw);
		usleep(20000);
	}

	g.rstop = 1;
	if (play) (*play)->SetPlayState(play, SL_PLAYSTATE_STOPPED);
	if (rec) (*rec)->SetRecordState(rec, SL_RECORDSTATE_STOPPED);
	int64_t tend = now_ns();
	uint32_t rd = __atomic_load_n(&g.rdone, __ATOMIC_ACQUIRE);
	for (; rflushed != rd && rd - rflushed <= REC_RING; rflushed++) {
		fwrite(g.rring + (size_t)(rflushed % REC_RING) * (size_t)g.frames, 2, (size_t)g.frames, raw);
		fprintf(ts, "R %u %lld\n", rflushed, (long long)g.rts[rflushed % REC_RING]);
	}
	fprintf(ts, "# end %lld buffers=%u overflow=%llu signal=%d\n", (long long)tend, rd,
		(unsigned long long)total_overflow, (int)g_stop);
	if (playObj) (*playObj)->Destroy(playObj);
	if (mixObj) (*mixObj)->Destroy(mixObj);
	if (recObj) (*recObj)->Destroy(recObj);
	(*engObj)->Destroy(engObj);
	fclose(ts);
	if (raw) fclose(raw);
	double secs = (tend - t0) / 1e9;
	printf("slprobe: %u buffers (%.2f s of audio) in %.2f s, %llu lost to overflow\n", rd,
		o.no_capture ? 0.0 : (double)rd * o.frames / o.rate, secs, (unsigned long long)total_overflow);
	return 0;
}
