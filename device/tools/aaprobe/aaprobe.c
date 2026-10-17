/*
 * aaprobe: AAudio capture/playback timestamp probe for Android 11 (LineageOS
 * 18.1 on the Echo Show 8, `crown`). slprobe's AAudio counterpart: the same
 * PREFIX.raw and a PREFIX.ts in slprobe's format with AAudio's own timestamps
 * appended, so device/tools/slprobe/decode.py reads it, and capture and
 * render can be placed on one clock.
 *
 * Capture: an AAudio input stream (shared mode, the -P performance mode, the
 * -p input preset, I16 at -r Hz and -c channels; 16 kHz mono by default) read
 * by its own thread in blocking reads of -f frames (1280, 80 ms). When a
 * buffer is full the thread stamps CLOCK_MONOTONIC and asks
 * AAudioStream_getFramesRead, AAudioStream_getTimestamp(CLOCK_MONOTONIC) and
 * AAudioStream_getXRunCount. PREFIX.raw gets the samples exactly as read
 * (S16LE, interleaved if -c > 1; bit 0 carries the AFE metadata); a partial
 * buffer at the stop is dropped.
 *
 * The configuration each stream was granted is printed after open and kept in
 * the .ts: AAudio may change the rate (the legacy path takes AudioRecord's),
 * and any resampling destroys the bit-0 metadata. A granted rate equal to the
 * requested one does not prove that no resampler ran below AAudio; the
 * decoder's count of valid metadata frames does.
 *
 * Playback (optional, -i): an AAudio output stream (-s usage, media by
 * default) at the stimulus's own rate and channel count plays a WAV (PCM16)
 * or raw S16LE file from a third thread, -D ms after capture started. It
 * primes the buffer with non-blocking writes, starts the stream, then writes
 * blocking, -b ms per write; each write is stamped like a read, with
 * AAudioStream_getFramesWritten and the output timestamp. Stream frame f plays
 * at timeNanos + (f - framePosition) / rate, on CLOCK_MONOTONIC like the
 * capture timestamps.
 *
 * Android 11's legacy (AudioRecord/AudioTrack) path ignores read and write
 * timeouts, so the main thread supervises: it reports capture stalls, flushes
 * the .ts every 20 ms, prints a status line per second, and ends the run (-d,
 * SIGINT, SIGTERM, a failed read) by stopping the streams, which releases a
 * blocked call.
 *
 * PREFIX.ts is text, one record per line:
 *   # key=value ...      header and notes
 *   T0 <ns>              capture AAudioStream_requestStart returned
 *   R <seq> <ns> <total> <framesRead> <res> <pos> <tns> <xruns>
 *                        buffer <seq> read, frames [seq*f, (seq+1)*f): <ns>
 *                        CLOCK_MONOTONIC after the read returned, <total>
 *                        frames read so far, <framesRead> getFramesRead,
 *                        <res> the getTimestamp result (0 = AAUDIO_OK) with
 *                        its <pos> framePosition and <tns> timeNanos (-1 -1
 *                        unless 0), <xruns> getXRunCount
 *   P <frame> <frames> <ns> <sframe> <framesWritten> <res> <pos> <tns> <xruns>
 *                        stimulus frames [frame, +frames) written as stream
 *                        frames [sframe, +frames); <ns> after the write
 *                        returned, the rest as for R, of the output stream
 *   PS <ns> / PE <ns>    output started / stimulus exhausted (output asked to
 *                        stop, which plays out what is buffered)
 *   X <what> [...] <ns>  stall, resumed; rerror/perror <result>: a read or
 *                        write (or the output start) failed
 *   # m ...              the -m summary
 * The first three columns of R and P are slprobe's.
 *
 * -m prints every capture timestamp during the first -x seconds and then
 * summarizes that window for both streams: how often getTimestamp succeeded,
 * the rate implied by the distinct (framePosition, timeNanos) pairs and by
 * the call returns, the completion lag of each read (its return less the
 * timestamp-derived capture time of its last frame) or the lead of each write
 * (the timestamp-derived playout time of its last frame less its return), and
 * the capture frame at which stimulus frame 0 plays.
 *
 * Build: device/tools/build_tools.sh (NDK 21.4, armv7a-linux-androideabi30,
 * -laaudio). Decode: device/tools/slprobe/decode.py.
 */
#include <aaudio/AAudio.h>
#include <errno.h>
#include <getopt.h>
#include <math.h>
#include <pthread.h>
#include <signal.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

/* Read/write timeout. The MMAP path honours it (a timed-out call returns
 * fewer frames); the legacy path blocks until done or stopped. */
#define IO_TIMEOUT_NS 250000000LL

static int64_t now_ns(void) {
	struct timespec ts;
	clock_gettime(CLOCK_MONOTONIC, &ts);
	return (int64_t)ts.tv_sec * 1000000000LL + ts.tv_nsec;
}

static volatile sig_atomic_t g_stop;
static void on_signal(int sig) { (void)sig; g_stop = 1; }

static void *xrealloc(void *p, size_t n) {
	p = realloc(p, n);
	if (!p) {
		fprintf(stderr, "out of memory\n");
		exit(1);
	}
	return p;
}

/* ─── names ───────────────────────────────────────────────────────────────── */

struct name { const char *s; int v; };

static const struct name presets[] = {
	{"generic", AAUDIO_INPUT_PRESET_GENERIC}, {"mic", AAUDIO_INPUT_PRESET_GENERIC},
	{"camcorder", AAUDIO_INPUT_PRESET_CAMCORDER},
	{"vr", AAUDIO_INPUT_PRESET_VOICE_RECOGNITION},
	{"voice_recognition", AAUDIO_INPUT_PRESET_VOICE_RECOGNITION},
	{"vc", AAUDIO_INPUT_PRESET_VOICE_COMMUNICATION},
	{"voice_communication", AAUDIO_INPUT_PRESET_VOICE_COMMUNICATION},
	{"unprocessed", AAUDIO_INPUT_PRESET_UNPROCESSED},
	{"performance", AAUDIO_INPUT_PRESET_VOICE_PERFORMANCE},
	{NULL, 0},
};

static const struct name perf_modes[] = {
	{"none", AAUDIO_PERFORMANCE_MODE_NONE},
	{"powersave", AAUDIO_PERFORMANCE_MODE_POWER_SAVING},
	{"lowlat", AAUDIO_PERFORMANCE_MODE_LOW_LATENCY},
	{NULL, 0},
};

static const struct name sharing_modes[] = {
	{"shared", AAUDIO_SHARING_MODE_SHARED}, {"exclusive", AAUDIO_SHARING_MODE_EXCLUSIVE},
	{NULL, 0},
};

static const struct name formats[] = {
	{"i16", AAUDIO_FORMAT_PCM_I16}, {"float", AAUDIO_FORMAT_PCM_FLOAT},
	{"unspecified", AAUDIO_FORMAT_UNSPECIFIED}, {"invalid", AAUDIO_FORMAT_INVALID},
	{NULL, 0},
};

static const struct name usages[] = {
	{"media", AAUDIO_USAGE_MEDIA},
	{"vc", AAUDIO_USAGE_VOICE_COMMUNICATION},
	{"voice_communication", AAUDIO_USAGE_VOICE_COMMUNICATION},
	{"signalling", AAUDIO_USAGE_VOICE_COMMUNICATION_SIGNALLING},
	{"alarm", AAUDIO_USAGE_ALARM},
	{"notification", AAUDIO_USAGE_NOTIFICATION},
	{"ringtone", AAUDIO_USAGE_NOTIFICATION_RINGTONE},
	{"event", AAUDIO_USAGE_NOTIFICATION_EVENT},
	{"accessibility", AAUDIO_USAGE_ASSISTANCE_ACCESSIBILITY},
	{"navigation", AAUDIO_USAGE_ASSISTANCE_NAVIGATION_GUIDANCE},
	{"sonification", AAUDIO_USAGE_ASSISTANCE_SONIFICATION},
	{"game", AAUDIO_USAGE_GAME},
	{"assistant", AAUDIO_USAGE_ASSISTANT},
	{NULL, 0},
};

static const struct name contents[] = {
	{"speech", AAUDIO_CONTENT_TYPE_SPEECH}, {"music", AAUDIO_CONTENT_TYPE_MUSIC},
	{"movie", AAUDIO_CONTENT_TYPE_MOVIE}, {"sonification", AAUDIO_CONTENT_TYPE_SONIFICATION},
	{NULL, 0},
};

/* name_value maps s through t, or reads it as a number; -1 if neither. */
static int name_value(const struct name *t, const char *s) {
	for (; t->s; t++)
		if (!strcmp(s, t->s)) return t->v;
	char *end;
	long v = strtol(s, &end, 0);
	if (!*s || *end || v < 0 || v > 0xffff) return -1;
	return (int)v;
}

static const char *value_name(const struct name *t, int v) {
	for (; t->s; t++)
		if (t->v == v) return t->s;
	return "?";
}

/* ─── options ─────────────────────────────────────────────────────────────── */

struct opts {
	int preset;          /* AAUDIO_INPUT_PRESET_*, or -1 to leave AAudio's default */
	int rate, ch;        /* capture request; rate 0 = the stream's own */
	int frames;          /* frames per blocking read */
	int perf;            /* AAUDIO_PERFORMANCE_MODE_*, both streams */
	int session;         /* allocate a session id, both streams */
	double seconds;      /* 0 = until SIGINT/SIGTERM */
	const char *prefix;  /* output path prefix */
	const char *stim;    /* stimulus file or NULL */
	int usage, content;  /* output; content -1 = AAudio's default */
	int delay_ms;        /* playback start after capture start */
	int raw_rate, raw_ch;/* raw stimulus format */
	int play_ms;         /* frames per blocking write, ms */
	int loop;            /* loop the stimulus */
	int quiet;
	int monitor;         /* -m */
	double window;       /* -x: -m window from T0, 0 = whole run */
};

static void usage(const char *argv0) {
	fprintf(stderr,
		"usage: %s [options]\n"
		"  -p PRESET   input preset: vr (default), generic|mic, camcorder, vc,\n"
		"              unprocessed, performance, none (not set: AAudio's default),\n"
		"              or a number\n"
		"  -r HZ       capture rate (16000), 0 = the stream's own\n"
		"  -c CH       capture channels (1)\n"
		"  -f FRAMES   frames per blocking read (1280)\n"
		"  -P MODE     performance mode, both streams: none (default), lowlat, powersave\n"
		"  -A          allocate session ids (AAUDIO_SESSION_ID_ALLOCATE), both streams;\n"
		"              AAudio then never takes the MMAP path\n"
		"  -d SECONDS  duration, 0 = until SIGINT/SIGTERM (10)\n"
		"  -o PREFIX   output prefix: PREFIX.raw, PREFIX.ts (/data/local/tmp/aaprobe);\n"
		"              none = no files, status lines only\n"
		"  -i FILE     play FILE: WAV PCM16, or raw S16LE (see -R, -C)\n"
		"  -s USAGE[/CONTENT]  output usage: media (default), vc, signalling, alarm,\n"
		"              notification, ringtone, event, accessibility, navigation,\n"
		"              sonification, game, assistant, or a number; content: speech,\n"
		"              music, movie, sonification, or a number (default: AAudio's)\n"
		"  -D MS       playback start after capture start (2000)\n"
		"  -R HZ       raw stimulus rate (48000)\n"
		"  -C CH       raw stimulus channels (2)\n"
		"  -b MS       frames per blocking write, in ms of stimulus (20)\n"
		"  -L          loop the stimulus until the end of the run\n"
		"  -q          no per-second status\n"
		"  -m          print every capture timestamp for the -x window, then\n"
		"              summarize both streams' timestamps over it\n"
		"  -x SECONDS  -m window from capture start, 0 = whole run (10)\n",
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

/* ─── stamps ──────────────────────────────────────────────────────────────── */

/* One read or write: when it returned, the stream index of its last frame,
 * and what AAudio reported right after. */
struct stamp {
	int64_t ns;        /* CLOCK_MONOTONIC after the call returned */
	int64_t last;      /* stream frame index of the call's last frame */
	int64_t api;       /* getFramesRead (input) or getFramesWritten (output) */
	int64_t pos, tns;  /* getTimestamp(CLOCK_MONOTONIC); -1 unless res is AAUDIO_OK */
	int32_t res, xruns;
};

static struct stamp take_stamp(AAudioStream *s, int input, int64_t ns, int64_t last) {
	struct stamp st = {.ns = ns, .last = last, .pos = -1, .tns = -1};
	st.api = input ? AAudioStream_getFramesRead(s) : AAudioStream_getFramesWritten(s);
	st.res = AAudioStream_getTimestamp(s, CLOCK_MONOTONIC, &st.pos, &st.tns);
	if (st.res != AAUDIO_OK) st.pos = st.tns = -1;
	st.xruns = AAudioStream_getXRunCount(s);
	return st;
}

/* The appended .ts columns: <api> <res> <pos> <tns> <xruns>. */
static void put_stamp(FILE *f, const struct stamp *s) {
	fprintf(f, " %lld %d %lld %lld %d\n", (long long)s->api, s->res, (long long)s->pos,
		(long long)s->tns, s->xruns);
}

/* lead_ns: the timestamp-derived time of the stamp's last frame less the
 * call's return. Capture: minus the completion lag; render: the write lead. */
static double lead_ns(const struct stamp *s, int rate) {
	return (double)(s->tns - s->ns) + (double)(s->last - s->pos) * 1e9 / rate;
}

struct stamps { struct stamp *v; size_t n, cap; };

static void stamps_add(struct stamps *l, const struct stamp *s) {
	if (l->n == l->cap) {
		l->cap = l->cap ? 2 * l->cap : 256;
		l->v = xrealloc(l->v, l->cap * sizeof(*l->v));
	}
	l->v[l->n++] = *s;
}

/* ─── shared state ────────────────────────────────────────────────────────── */

static struct {
	pthread_mutex_t mu;     /* the .ts, the -m stamps and the status fields */
	FILE *ts;
	int64_t t0;
	int monitor;
	int64_t win_end;        /* -m window end, CLOCK_MONOTONIC */
	struct stamps cap, ren; /* -m: stamps inside the window */
	volatile int quit;

	/* capture; the counters are written by the capture thread */
	AAudioStream *in;
	FILE *raw;
	int in_rate, in_ch, frames;
	int16_t *buf;
	uint32_t reads;
	int64_t last_ns;        /* last full buffer, for stall detection */
	struct stamp last;      /* its stamp, for the status line */
	double sec_sq;
	uint32_t sec_n, sec_zero, sec_reads;
	int cap_failed;

	/* playback */
	AAudioStream *out;
	struct stim stim;
	int pframes, ploop;
	int64_t play_at;
	int pstate;             /* 0 waiting, 1 playing, 2 stimulus done, 3 failed */
} g = {.mu = PTHREAD_MUTEX_INITIALIZER};

static void print_read(uint32_t seq, const struct stamp *s) {
	double at = (s->ns - g.t0) / 1e9;
	if (s->res == AAUDIO_OK)
		printf("[%8.3fs] read %u framesRead %lld ts pos %lld at %.6f s lag %.3f ms xrun %d\n",
			at, seq, (long long)s->api, (long long)s->pos, (s->tns - g.t0) / 1e9,
			-lead_ns(s, g.in_rate) / 1e6, s->xruns);
	else
		printf("[%8.3fs] read %u framesRead %lld ts %d %s xrun %d\n", at, seq, (long long)s->api,
			s->res, AAudio_convertResultToText(s->res), s->xruns);
}

static void *capture(void *arg) {
	(void)arg;
	const size_t n = (size_t)g.frames * (size_t)g.in_ch;
	int32_t got = 0;
	while (!g.quit) {
		aaudio_result_t r = AAudioStream_read(g.in, g.buf + (size_t)got * (size_t)g.in_ch,
			g.frames - got, IO_TIMEOUT_NS);
		int64_t t = now_ns();
		if (g.quit) break;
		if (r < 0) {
			pthread_mutex_lock(&g.mu);
			fprintf(g.ts, "X rerror %d %lld\n", r, (long long)t);
			g.cap_failed = 1;
			pthread_mutex_unlock(&g.mu);
			printf("aaprobe: read failed: %d %s\n", r, AAudio_convertResultToText(r));
			break;
		}
		got += r;
		if (got < g.frames) continue;
		got = 0;

		uint32_t seq = g.reads;
		struct stamp s = take_stamp(g.in, 1, t, (int64_t)(seq + 1) * g.frames - 1);
		fwrite(g.buf, 2, n, g.raw);
		fflush(g.raw);
		double sq = 0;
		uint32_t zero = 0;
		for (size_t i = 0; i < n; i++) {
			double v = (double)(g.buf[i] & ~1); /* bit 0 carries AFE metadata */
			sq += v * v;
			if (g.buf[i] == 0) zero++;
		}
		int in_win = g.monitor && t <= g.win_end;

		pthread_mutex_lock(&g.mu);
		fprintf(g.ts, "R %u %lld %lld", seq, (long long)t, (long long)(s.last + 1));
		put_stamp(g.ts, &s);
		if (in_win) stamps_add(&g.cap, &s);
		g.reads = seq + 1;
		g.last_ns = t;
		g.last = s;
		g.sec_sq += sq;
		g.sec_n += (uint32_t)n;
		g.sec_zero += zero;
		g.sec_reads++;
		pthread_mutex_unlock(&g.mu);
		if (in_win) print_read(seq, &s);
	}
	return NULL;
}

static void player_failed(const char *what, aaudio_result_t r, int64_t t) {
	pthread_mutex_lock(&g.mu);
	fprintf(g.ts, "X perror %d %lld\n", r, (long long)t);
	g.pstate = 3;
	pthread_mutex_unlock(&g.mu);
	printf("aaprobe: output %s failed: %d %s\n", what, r, AAudio_convertResultToText(r));
}

static int player_start(void) {
	aaudio_result_t r = AAudioStream_requestStart(g.out);
	int64_t t = now_ns();
	if (r != AAUDIO_OK) {
		player_failed("requestStart", r, t);
		return -1;
	}
	pthread_mutex_lock(&g.mu);
	fprintf(g.ts, "PS %lld\n", (long long)t);
	g.pstate = 1;
	pthread_mutex_unlock(&g.mu);
	return 0;
}

static void *player(void *arg) {
	(void)arg;
	while (!g.quit && now_ns() < g.play_at) usleep(5000);
	const size_t ch = (size_t)g.stim.ch;
	size_t next = 0;      /* next stimulus frame */
	int64_t written = 0;  /* stream frames written */
	int started = 0;
	while (!g.quit) {
		if (next >= g.stim.frames) {
			if (!g.ploop) break;
			next = 0;
		}
		int32_t n = g.pframes;
		if (next + (size_t)n > g.stim.frames) n = (int32_t)(g.stim.frames - next);
		const int16_t *src = g.stim.pcm + next * ch;
		int32_t done = 0;
		while (done < n && !g.quit) {
			/* Non-blocking until the buffer is full, then start. */
			aaudio_result_t r = AAudioStream_write(g.out, src + (size_t)done * ch, n - done,
				started ? IO_TIMEOUT_NS : 0);
			if (r < 0) {
				player_failed("write", r, now_ns());
				return NULL;
			}
			done += r;
			if (!started && done < n) {
				if (player_start()) return NULL;
				started = 1;
			}
		}
		int64_t t = now_ns();
		if (g.quit) return NULL;
		struct stamp s = take_stamp(g.out, 0, t, written + n - 1);
		pthread_mutex_lock(&g.mu);
		fprintf(g.ts, "P %zu %d %lld %lld", next, n, (long long)t, (long long)written);
		put_stamp(g.ts, &s);
		if (g.monitor && t <= g.win_end) stamps_add(&g.ren, &s);
		pthread_mutex_unlock(&g.mu);
		written += n;
		next += (size_t)n;
	}
	if (g.quit) return NULL;
	/* The whole stimulus fitted in the buffer. */
	if (!started && player_start()) return NULL;
	aaudio_result_t r = AAudioStream_requestStop(g.out);
	int64_t t = now_ns();
	pthread_mutex_lock(&g.mu);
	fprintf(g.ts, "PE %lld\n", (long long)t);
	if (r != AAUDIO_OK) fprintf(g.ts, "# output requestStop %d %s\n", r, AAudio_convertResultToText(r));
	g.pstate = 2;
	pthread_mutex_unlock(&g.mu);
	return NULL;
}

/* ─── -m summary ──────────────────────────────────────────────────────────── */

/* note prints one summary line and keeps it in the .ts. g.mu held. */
static void note(const char *fmt, ...) {
	char line[512];
	va_list ap;
	va_start(ap, fmt);
	vsnprintf(line, sizeof(line), fmt, ap);
	va_end(ap);
	printf("aaprobe: -m %s\n", line);
	fprintf(g.ts, "# m %s\n", line);
}

static int cmp_double(const void *a, const void *b) {
	double x = *(const double *)a, y = *(const double *)b;
	return (x > y) - (x < y);
}

/* Least-squares slope of y on x. */
struct fit { double sx, sy, sxx, sxy; size_t n; };

static void fit_add(struct fit *f, double x, double y) {
	f->sx += x;
	f->sy += y;
	f->sxx += x * x;
	f->sxy += x * y;
	f->n++;
}

static double fit_slope(const struct fit *f) {
	double d = (double)f->n * f->sxx - f->sx * f->sx;
	return f->n >= 2 && d > 0 ? ((double)f->n * f->sxy - f->sx * f->sy) / d : NAN;
}

/* summarize_dir reports one stream's stamps in the window: getTimestamp
 * success, the rates implied by the distinct timestamps and by the call
 * returns, and the completion lag (capture) or write lead (render). The
 * return fit, like the lag, covers only calls with a timestamp: that leaves
 * out the writes that primed the output before it started. */
static void summarize_dir(const char *dir, const struct stamps *l, int rate, int input) {
	const char *calls = input ? "reads" : "writes", *call = input ? "read" : "write";
	if (!l->n) {
		note("%s: no %s in the window", dir, calls);
		return;
	}
	struct fit ft = {0}, fr = {0};
	size_t ok = 0, fail = 0, mism = 0, distinct = 0, err_at = 0;
	int32_t err = 0;
	double *off = xrealloc(NULL, l->n * sizeof(*off));
	const struct stamp *first = NULL, *prev = NULL;
	for (size_t i = 0; i < l->n; i++) {
		const struct stamp *s = &l->v[i];
		if (s->api != s->last + 1) mism++;
		if (s->res != AAUDIO_OK) {
			if (!fail++) {
				err = s->res;
				err_at = i;
			}
			continue;
		}
		if (!first) first = s;
		fit_add(&fr, (s->ns - first->ns) / 1e9, (double)(s->last - first->last));
		off[ok++] = (input ? -1 : 1) * lead_ns(s, rate) / 1e6;
		if (prev && s->pos == prev->pos && s->tns == prev->tns) continue;
		prev = s;
		fit_add(&ft, (s->tns - first->tns) / 1e9, (double)(s->pos - first->pos));
		distinct++;
	}
	char errs[128] = "";
	if (fail)
		snprintf(errs, sizeof(errs), " (first %d %s, %s %zu)", err, AAudio_convertResultToText(err),
			call, err_at);
	note("%s: %zu %s, getTimestamp ok %zu failed %zu%s; %s off the probe's count %zu", dir, l->n,
		calls, ok, fail, errs, input ? "framesRead" : "framesWritten", mism);
	double rt = fit_slope(&ft), rr = fit_slope(&fr);
	note("%s: %zu distinct timestamps over %.2f s: %.3f Hz (%+.1f ppm of %d); %s returns: "
		"%.3f Hz (%+.1f ppm)", dir, distinct, prev ? (prev->tns - first->tns) / 1e9 : 0.0, rt,
		(rt / rate - 1) * 1e6, rate, call, rr, (rr / rate - 1) * 1e6);
	if (ok) {
		qsort(off, ok, sizeof(*off), cmp_double);
		note("%s: %s ms: min %.3f median %.3f max %.3f over %zu %s", dir,
			input ? "completion lag" : "write lead", off[0],
			(off[(ok - 1) / 2] + off[ok / 2]) / 2, off[ok - 1], ok, calls);
	}
	free(off);
}

/* summarize reports the -m window for both streams and places stimulus frame
 * 0 on the capture timeline. g.mu held. */
static void summarize(int64_t t, int out_rate) {
	int64_t end = t < g.win_end ? t : g.win_end;
	note("window T0+0 to T0+%.2f s", (end - g.t0) / 1e9);
	summarize_dir("capture", &g.cap, g.in_rate, 1);
	if (!g.out) return;
	summarize_dir("render", &g.ren, out_rate, 0);
	const struct stamp *r = NULL, *c = NULL;
	for (size_t i = 0; i < g.ren.n && !r; i++)
		if (g.ren.v[i].res == AAUDIO_OK) r = &g.ren.v[i];
	if (!r) return;
	/* Stream frame 0 is stimulus frame 0. */
	double p0 = (double)r->tns - (double)r->pos * 1e9 / out_rate;
	for (size_t i = 0; i < g.cap.n; i++) {
		const struct stamp *s = &g.cap.v[i];
		if (s->res == AAUDIO_OK && (!c || fabs((double)s->tns - p0) < fabs((double)c->tns - p0))) c = s;
	}
	if (!c) return;
	double f = (double)c->pos + (p0 - (double)c->tns) * g.in_rate / 1e9;
	note("stimulus frame 0 plays at T0+%.4f s by the render timestamps: capture frame %.0f "
		"(%.4f s into the .raw) by the capture timestamps", (p0 - (double)g.t0) / 1e9, f, f / g.in_rate);
}

/* ─── main ────────────────────────────────────────────────────────────────── */

/* open_stream opens a shared-mode I16 stream; a preset or content below 0
 * leaves AAudio's default. */
static AAudioStream *open_stream(aaudio_direction_t dir, int rate, int ch, const struct opts *o) {
	const char *what = dir == AAUDIO_DIRECTION_INPUT ? "input" : "output";
	AAudioStreamBuilder *b;
	aaudio_result_t r = AAudio_createStreamBuilder(&b);
	if (r != AAUDIO_OK) {
		fprintf(stderr, "AAudio_createStreamBuilder: %d %s\n", r, AAudio_convertResultToText(r));
		return NULL;
	}
	AAudioStreamBuilder_setDirection(b, dir);
	AAudioStreamBuilder_setSharingMode(b, AAUDIO_SHARING_MODE_SHARED);
	AAudioStreamBuilder_setPerformanceMode(b, o->perf);
	AAudioStreamBuilder_setFormat(b, AAUDIO_FORMAT_PCM_I16);
	if (rate > 0) AAudioStreamBuilder_setSampleRate(b, rate);
	AAudioStreamBuilder_setChannelCount(b, ch);
	if (o->session) AAudioStreamBuilder_setSessionId(b, AAUDIO_SESSION_ID_ALLOCATE);
	if (dir == AAUDIO_DIRECTION_INPUT) {
		if (o->preset >= 0) AAudioStreamBuilder_setInputPreset(b, o->preset);
	} else {
		AAudioStreamBuilder_setUsage(b, o->usage);
		if (o->content >= 0) AAudioStreamBuilder_setContentType(b, o->content);
	}
	AAudioStream *s = NULL;
	r = AAudioStreamBuilder_openStream(b, &s);
	AAudioStreamBuilder_delete(b);
	if (r != AAUDIO_OK) {
		fprintf(stderr, "open %s stream: %d %s\n", what, r, AAudio_convertResultToText(r));
		return NULL;
	}
	return s;
}

/* describe prints the configuration a stream was granted and records it on
 * one .ts line, keys prefixed k. */
static void describe(FILE *ts, AAudioStream *s, const char *dir, const char *k) {
	int input = AAudioStream_getDirection(s) == AAUDIO_DIRECTION_INPUT;
	int rate = AAudioStream_getSampleRate(s), ch = AAudioStream_getChannelCount(s);
	int fmt = AAudioStream_getFormat(s), cap = AAudioStream_getBufferCapacityInFrames(s);
	int burst = AAudioStream_getFramesPerBurst(s), size = AAudioStream_getBufferSizeInFrames(s);
	int session = AAudioStream_getSessionId(s), perf = AAudioStream_getPerformanceMode(s);
	int sharing = AAudioStream_getSharingMode(s), dev = AAudioStream_getDeviceId(s);
	char use[96], usek[96];
	if (input) {
		int p = AAudioStream_getInputPreset(s);
		snprintf(use, sizeof(use), "preset %s (%d)", value_name(presets, p), p);
		snprintf(usek, sizeof(usek), "%spreset=%d", k, p);
	} else {
		int u = AAudioStream_getUsage(s), c = AAudioStream_getContentType(s);
		snprintf(use, sizeof(use), "usage %s (%d), content %s (%d)", value_name(usages, u), u,
			value_name(contents, c), c);
		snprintf(usek, sizeof(usek), "%susage=%d %scontent=%d", k, u, k, c);
	}
	printf("aaprobe: %s: %d Hz, %d ch, format %s (%d), capacity %d, burst %d, buffer %d, %s, "
		"session %d, perf %s (%d), sharing %s (%d), device %d\n",
		dir, rate, ch, value_name(formats, fmt), fmt, cap, burst, size, use, session,
		value_name(perf_modes, perf), perf, value_name(sharing_modes, sharing), sharing, dev);
	fprintf(ts, "# %s %srate=%d %schannels=%d %sformat=%d %scapacity=%d %sburst=%d %sbuffer=%d %s "
		"%ssession=%d %sperf=%d %ssharing=%d %sdevice=%d\n",
		dir, k, rate, k, ch, k, fmt, k, cap, k, burst, k, size, usek, k, session, k, perf, k, sharing,
		k, dev);
}

int main(int argc, char **argv) {
	struct opts o = {
		.preset = AAUDIO_INPUT_PRESET_VOICE_RECOGNITION, .rate = 16000, .ch = 1, .frames = 1280,
		.perf = AAUDIO_PERFORMANCE_MODE_NONE, .seconds = 10, .prefix = "/data/local/tmp/aaprobe",
		.usage = AAUDIO_USAGE_MEDIA, .content = -1, .delay_ms = 2000, .raw_rate = 48000,
		.raw_ch = 2, .play_ms = 20, .window = 10,
	};
	int c, bad = 0;
	while ((c = getopt(argc, argv, "p:r:c:f:P:Ad:o:i:s:D:R:C:b:Lqmx:h")) != -1) {
		switch (c) {
		case 'p':
			o.preset = strcmp(optarg, "none") ? name_value(presets, optarg) : -1;
			if (o.preset < 0 && strcmp(optarg, "none")) bad = 1;
			break;
		case 'r': o.rate = atoi(optarg); break;
		case 'c': o.ch = atoi(optarg); break;
		case 'f': o.frames = atoi(optarg); break;
		case 'P':
			if ((o.perf = name_value(perf_modes, optarg)) < 0) bad = 1;
			break;
		case 'A': o.session = 1; break;
		case 'd': o.seconds = atof(optarg); break;
		case 'o': o.prefix = optarg; break;
		case 'i': o.stim = optarg; break;
		case 's': {
			char *slash = strchr(optarg, '/');
			if (slash) {
				*slash = 0;
				if ((o.content = name_value(contents, slash + 1)) < 0) bad = 1;
			}
			if ((o.usage = name_value(usages, optarg)) < 0) bad = 1;
			break;
		}
		case 'D': o.delay_ms = atoi(optarg); break;
		case 'R': o.raw_rate = atoi(optarg); break;
		case 'C': o.raw_ch = atoi(optarg); break;
		case 'b': o.play_ms = atoi(optarg); break;
		case 'L': o.loop = 1; break;
		case 'q': o.quiet = 1; break;
		case 'm': o.monitor = 1; break;
		case 'x': o.window = atof(optarg); break;
		default: usage(argv[0]); return 2;
		}
	}
	if (bad || o.rate < 0 || o.ch <= 0 || o.frames <= 0 || o.play_ms <= 0 || o.seconds < 0 ||
	    o.window < 0 || o.delay_ms < 0) {
		usage(argv[0]);
		return 2;
	}

	signal(SIGINT, on_signal);
	signal(SIGTERM, on_signal);
	signal(SIGPIPE, SIG_IGN);
	setvbuf(stdout, NULL, _IOLBF, 0); /* status reaches a pipe (logcat, adb) live */

	if (o.stim && load_stim(o.stim, o.raw_rate, o.raw_ch, &g.stim)) return 1;
	if (o.stim && (!g.stim.frames || g.stim.rate <= 0)) {
		fprintf(stderr, "%s: no frames, or rate %d\n", o.stim, g.stim.rate);
		return 1;
	}

	/* -o none: status lines only, for contexts that may not write files. */
	int nofiles = !strcmp(o.prefix, "none");
	char path[512];
	snprintf(path, sizeof(path), "%s.ts", o.prefix);
	g.ts = fopen(nofiles ? "/dev/null" : path, "w");
	if (!g.ts) { fprintf(stderr, "open %s: %s\n", path, strerror(errno)); return 1; }
	snprintf(path, sizeof(path), "%s.raw", o.prefix);
	g.raw = fopen(nofiles ? "/dev/null" : path, "wb");
	if (!g.raw) { fprintf(stderr, "open %s: %s\n", path, strerror(errno)); return 1; }

	char label[128] = "?";
	{
		FILE *ctx = fopen("/proc/self/attr/current", "r");
		if (ctx) {
			if (!fgets(label, sizeof(label), ctx)) strcpy(label, "?");
			fclose(ctx);
			label[strcspn(label, "\n")] = 0;
		}
		printf("aaprobe: pid %d uid %d context %s\n", getpid(), getuid(), label);
	}

	/* Capture stream. Reads go straight into an I16 buffer: anything else
	 * would already be a conversion of the metadata-carrying samples. */
	g.in = open_stream(AAUDIO_DIRECTION_INPUT, o.rate, o.ch, &o);
	if (!g.in) return 1;
	g.in_rate = AAudioStream_getSampleRate(g.in);
	g.in_ch = AAudioStream_getChannelCount(g.in);
	g.frames = o.frames;
	int in_fmt = AAudioStream_getFormat(g.in), in_preset = AAudioStream_getInputPreset(g.in);
	fprintf(g.ts, "# aaprobe api=aaudio pid=%d uid=%d preset=%d rate=%d channels=%d frames=%d "
		"seconds=%g capture=1 req_preset=%d req_rate=%d req_channels=%d req_perf=%d "
		"session_alloc=%d\n",
		getpid(), getuid(), in_preset, g.in_rate, g.in_ch, o.frames, o.seconds, o.preset, o.rate,
		o.ch, o.perf, o.session);
	fprintf(g.ts, "# context=%s\n", label);
	describe(g.ts, g.in, "input", "in_");
	if (in_fmt != AAUDIO_FORMAT_PCM_I16 || g.in_rate <= 0 || g.in_ch <= 0) {
		fprintf(stderr, "aaprobe: input opened at %d Hz, %d ch, format %s (%d); need i16\n", g.in_rate,
			g.in_ch, value_name(formats, in_fmt), in_fmt);
		return 1;
	}
	if (o.rate && g.in_rate != o.rate)
		printf("aaprobe: WARNING input rate %d Hz, asked %d\n", g.in_rate, o.rate);
	if (g.in_ch != o.ch)
		printf("aaprobe: WARNING input %d channels, asked %d\n", g.in_ch, o.ch);
	if (o.preset >= 0 && in_preset != o.preset)
		printf("aaprobe: WARNING input preset %d, asked %d\n", in_preset, o.preset);
	g.buf = malloc((size_t)o.frames * (size_t)g.in_ch * 2);
	if (!g.buf) { fprintf(stderr, "out of memory\n"); return 1; }

	/* Playback stream, opened now and started from its thread. */
	int out_rate = 0;
	if (o.stim) {
		fprintf(g.ts, "# stimulus=%s stim_rate=%d stim_ch=%d stim_frames=%zu usage=%d content=%d "
			"delay_ms=%d play_ms=%d loop=%d\n",
			o.stim, g.stim.rate, g.stim.ch, g.stim.frames, o.usage, o.content, o.delay_ms,
			o.play_ms, o.loop);
		g.out = open_stream(AAUDIO_DIRECTION_OUTPUT, g.stim.rate, g.stim.ch, &o);
		if (!g.out) return 1;
		describe(g.ts, g.out, "output", "out_");
		out_rate = AAudioStream_getSampleRate(g.out);
		int out_ch = AAudioStream_getChannelCount(g.out), out_fmt = AAudioStream_getFormat(g.out);
		if (out_rate != g.stim.rate || out_ch != g.stim.ch || out_fmt != AAUDIO_FORMAT_PCM_I16) {
			fprintf(stderr, "aaprobe: output opened at %d Hz, %d ch, format %d; the stimulus is "
				"%d Hz, %d ch, i16\n", out_rate, out_ch, out_fmt, g.stim.rate, g.stim.ch);
			return 1;
		}
		g.pframes = g.stim.rate * o.play_ms / 1000;
		if (g.pframes <= 0) g.pframes = 1;
		g.ploop = o.loop;
	}
	fflush(g.ts);

	aaudio_result_t r = AAudioStream_requestStart(g.in);
	if (r != AAUDIO_OK) {
		fprintf(stderr, "input requestStart: %d %s\n", r, AAudio_convertResultToText(r));
		return 1;
	}
	g.t0 = now_ns();
	fprintf(g.ts, "T0 %lld\n", (long long)g.t0);
	g.last_ns = g.t0;
	g.monitor = o.monitor;
	g.win_end = o.window > 0 ? g.t0 + (int64_t)(o.window * 1e9) : INT64_MAX;
	g.play_at = g.t0 + (int64_t)o.delay_ms * 1000000LL;

	pthread_t cap_th, play_th;
	if (pthread_create(&cap_th, NULL, capture, NULL)) {
		fprintf(stderr, "pthread_create (capture) failed\n");
		return 1;
	}
	if (g.out && pthread_create(&play_th, NULL, player, NULL)) {
		fprintf(stderr, "pthread_create (player) failed\n");
		return 1;
	}

	int64_t end_at = o.seconds > 0 ? g.t0 + (int64_t)(o.seconds * 1e9) : INT64_MAX;
	/* A stall is a second without a buffer, or three buffer lengths. */
	int64_t stall_ns = (int64_t)3 * o.frames * 1000000000LL / g.in_rate;
	if (stall_ns < 1000000000LL) stall_ns = 1000000000LL;
	int64_t next_status = g.t0 + 1000000000LL;
	int stalled = 0, summarized = 0;
	static const char *const pstates[] = {"", " play:on", " play:done", " play:failed"};

	while (!g_stop) {
		int64_t t = now_ns();
		if (t >= end_at) break;
		pthread_mutex_lock(&g.mu);
		int failed = g.cap_failed;
		if (stalled && g.reads && g.last_ns > t - stall_ns / 2) {
			fprintf(g.ts, "X resumed %lld\n", (long long)g.last_ns);
			printf("aaprobe: capture resumed\n");
			stalled = 0;
		}
		if (!stalled && t - g.last_ns > stall_ns) {
			fprintf(g.ts, "X stall %lld\n", (long long)t);
			printf("aaprobe: STALL, no capture buffer for %lld ms\n", (long long)((t - g.last_ns) / 1000000));
			stalled = 1;
		}
		if (g.monitor && !summarized && t > g.win_end) {
			summarize(t, out_rate);
			summarized = 1;
		}
		if (t >= next_status) {
			if (!o.quiet) {
				double rms = g.sec_n ? sqrt(g.sec_sq / g.sec_n) : 0;
				double db = rms > 0 ? 20 * log10(rms / 32768.0) : -999;
				char tsb[48] = "ts -";
				if (g.reads && g.last.res == AAUDIO_OK)
					snprintf(tsb, sizeof(tsb), "lag %6.2f ms", -lead_ns(&g.last, g.in_rate) / 1e6);
				else if (g.reads)
					snprintf(tsb, sizeof(tsb), "ts %d", g.last.res);
				printf("[%6.1fs] reads %5u (+%3u) rms %6.1f dBFS zero %5.1f%% xrun %d %s%s%s\n",
					(t - g.t0) / 1e9, g.reads, g.sec_reads, db,
					g.sec_n ? 100.0 * g.sec_zero / g.sec_n : 0.0, g.last.xruns, tsb,
					g.out ? pstates[g.pstate] : "", stalled ? " STALLED" : "");
			}
			g.sec_sq = 0;
			g.sec_n = g.sec_zero = g.sec_reads = 0;
			next_status += 1000000000LL;
		}
		fflush(g.ts);
		pthread_mutex_unlock(&g.mu);
		if (failed) break;
		usleep(20000);
	}

	/* Stopping releases a read or write blocked on the legacy path. */
	g.quit = 1;
	AAudioStream_requestStop(g.in);
	if (g.out) AAudioStream_requestStop(g.out);
	pthread_join(cap_th, NULL);
	if (g.out) pthread_join(play_th, NULL);
	int64_t tend = now_ns();
	int32_t xruns = AAudioStream_getXRunCount(g.in);
	if (g.monitor && !summarized) summarize(tend, out_rate);
	fprintf(g.ts, "# end %lld reads=%u xruns=%d signal=%d\n", (long long)tend, g.reads, xruns,
		(int)g_stop);
	if (g.out) AAudioStream_close(g.out);
	AAudioStream_close(g.in);
	fclose(g.ts);
	fclose(g.raw);
	printf("aaprobe: %u reads (%.2f s of audio) in %.2f s, %d xruns\n", g.reads,
		(double)g.reads * o.frames / g.in_rate, (tend - g.t0) / 1e9, xruns);
	return 0;
}
