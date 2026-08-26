// v4l2lat - measure H.264 decode->encode latency on Raspberry Pi's bcm2835-codec.
//
// Feeds an Annex-B H.264 file into /dev/video10 (decoder), hands the decoded frames to
// /dev/video11 (encoder), and reports how long each frame takes end to end. The point is
// to find out how much latency a transcode adds to the CarPlay screen stream, and how much
// CPU it costs when the two hardware blocks do the work.
//
// Frames are correlated with the v4l2_buffer timestamp, which M2M drivers copy from the
// OUTPUT queue to the matching CAPTURE buffer, so a submit time set on the decoder input
// comes back out of the encoder attached to the right frame.
//
// Build: gcc -O2 -o v4l2lat v4l2lat.c

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <linux/videodev2.h>
#include <poll.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <time.h>
#include <unistd.h>

#define MAX_BUFS 16
#define MAX_TRACK 4096

static int verbose;

static void die(const char *fmt, ...) {
	va_list ap;
	va_start(ap, fmt);
	fprintf(stderr, "error: ");
	vfprintf(stderr, fmt, ap);
	va_end(ap);
	if (errno)
		fprintf(stderr, ": %s", strerror(errno));
	fprintf(stderr, "\n");
	exit(1);
}

static void vlog(const char *fmt, ...) {
	if (!verbose)
		return;
	va_list ap;
	va_start(ap, fmt);
	vfprintf(stderr, fmt, ap);
	va_end(ap);
	fputc('\n', stderr);
}

static int xioctl(int fd, unsigned long req, void *arg) {
	int r;
	do {
		r = ioctl(fd, req, arg);
	} while (r == -1 && errno == EINTR);
	return r;
}

static uint64_t now_us(void) {
	struct timespec ts;
	clock_gettime(CLOCK_MONOTONIC, &ts);
	return (uint64_t)ts.tv_sec * 1000000 + ts.tv_nsec / 1000;
}

// ---------------------------------------------------------------- buffers

struct buf {
	void *start;
	size_t length;
	int dmafd;
	int queued;
};

struct queue {
	struct buf b[MAX_BUFS];
	int count;
	enum v4l2_buf_type type;
	enum v4l2_memory memory;
};

static void queue_alloc(int fd, struct queue *q, enum v4l2_buf_type type,
                        enum v4l2_memory memory, int count, const char *what) {
	struct v4l2_requestbuffers req = {
		.count = count, .type = type, .memory = memory,
	};
	if (xioctl(fd, VIDIOC_REQBUFS, &req) == -1)
		die("REQBUFS %s", what);

	q->count = req.count;
	q->type = type;
	q->memory = memory;

	for (int i = 0; i < q->count; i++) {
		q->b[i].dmafd = -1;
		q->b[i].queued = 0;

		if (memory != V4L2_MEMORY_MMAP)
			continue;

		// Multiplanar QUERYBUF needs somewhere to put the per-plane details, and the
		// mmap offset lives in the plane rather than in the buffer.
		struct v4l2_plane planes[VIDEO_MAX_PLANES];
		memset(planes, 0, sizeof(planes));

		struct v4l2_buffer buf = {
			.type = type, .memory = memory, .index = i,
			.m.planes = planes, .length = VIDEO_MAX_PLANES,
		};
		if (xioctl(fd, VIDIOC_QUERYBUF, &buf) == -1)
			die("QUERYBUF %s[%d]", what, i);

		q->b[i].length = planes[0].length;
		q->b[i].start = mmap(NULL, planes[0].length, PROT_READ | PROT_WRITE, MAP_SHARED,
		                     fd, planes[0].m.mem_offset);
		if (q->b[i].start == MAP_FAILED)
			die("mmap %s[%d]", what, i);
	}

	vlog("%s: %d buffers", what, q->count);
}

// Export every buffer in a queue as a dmabuf fd, for zero-copy handoff.
static void queue_export(int fd, struct queue *q, const char *what) {
	for (int i = 0; i < q->count; i++) {
		struct v4l2_exportbuffer exp = { .type = q->type, .index = i, .plane = 0, .flags = O_CLOEXEC };
		if (xioctl(fd, VIDIOC_EXPBUF, &exp) == -1)
			die("EXPBUF %s[%d]", what, i);
		q->b[i].dmafd = exp.fd;
	}
	vlog("%s: exported %d dmabuf fds", what, q->count);
}

static void stream_on(int fd, enum v4l2_buf_type type, const char *what) {
	if (xioctl(fd, VIDIOC_STREAMON, &type) == -1)
		die("STREAMON %s", what);
}

static void stream_off(int fd, enum v4l2_buf_type type) {
	xioctl(fd, VIDIOC_STREAMOFF, &type);
}

// ---------------------------------------------------------------- annex-b

struct au {
	size_t offset;
	size_t length;
};

// Split an Annex-B stream into access units. A new unit starts at the first VCL NAL
// (types 1 and 5) once the current one already holds a VCL NAL; parameter sets and SEI
// ahead of that stay attached to the unit they precede.
static struct au *split_access_units(const uint8_t *data, size_t size, int *out_count) {
	size_t cap = 1024;
	struct au *aus = malloc(cap * sizeof(*aus));
	int count = 0;

	size_t start = 0;
	int have_vcl = 0;
	size_t i = 0;

	while (i + 3 < size) {
		int sc = 0;
		if (data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1)
			sc = 3;
		else if (i + 4 < size && data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 0 &&
		         data[i + 3] == 1)
			sc = 4;

		if (!sc) {
			i++;
			continue;
		}

		uint8_t nal = data[i + sc] & 0x1F;
		int is_vcl = (nal == 1 || nal == 5);

		if (is_vcl && have_vcl && i > start) {
			if (count == (int)cap) {
				cap *= 2;
				aus = realloc(aus, cap * sizeof(*aus));
			}
			aus[count].offset = start;
			aus[count].length = i - start;
			count++;
			start = i;
			have_vcl = 0;
		}

		if (is_vcl)
			have_vcl = 1;

		i += sc;
	}

	if (start < size) {
		if (count == (int)cap)
			aus = realloc(aus, (cap + 1) * sizeof(*aus));
		aus[count].offset = start;
		aus[count].length = size - start;
		count++;
	}

	*out_count = count;
	return aus;
}

// ---------------------------------------------------------------- tracking

struct track {
	uint64_t key;
	uint64_t submitted;
	uint64_t decoded;
	uint64_t encoded;
	int used;
};

static struct track tracks[MAX_TRACK];

static struct track *track_get(uint64_t key, int create) {
	size_t slot = key % MAX_TRACK;
	for (size_t probe = 0; probe < MAX_TRACK; probe++) {
		struct track *t = &tracks[(slot + probe) % MAX_TRACK];
		if (t->used && t->key == key)
			return t;
		if (!t->used && create) {
			t->used = 1;
			t->key = key;
			return t;
		}
	}
	return NULL;
}

static uint64_t key_of(const struct timeval *tv) {
	return (uint64_t)tv->tv_sec * 1000000 + tv->tv_usec;
}

static void key_to_tv(uint64_t key, struct timeval *tv) {
	tv->tv_sec = key / 1000000;
	tv->tv_usec = key % 1000000;
}

// ---------------------------------------------------------------- stats

static int cmp_u64(const void *a, const void *b) {
	uint64_t x = *(const uint64_t *)a, y = *(const uint64_t *)b;
	return x < y ? -1 : x > y ? 1 : 0;
}

static void report(const char *label, uint64_t *v, int n, double frame_ms) {
	if (n == 0) {
		printf("  %-22s (no samples)\n", label);
		return;
	}

	qsort(v, n, sizeof(*v), cmp_u64);

	double sum = 0;
	for (int i = 0; i < n; i++)
		sum += v[i];

	double mean = sum / n / 1000.0;
	double p50 = v[n / 2] / 1000.0;
	double p95 = v[(int)(n * 0.95)] / 1000.0;
	double max = v[n - 1] / 1000.0;

	printf("  %-22s mean %6.1f ms   p50 %6.1f   p95 %6.1f   max %6.1f", label, mean, p50,
	       p95, max);
	if (frame_ms > 0)
		printf("   (%.1f frames)", mean / frame_ms);
	printf("\n");
}

static void cpu_times(double *user, double *sys) {
	FILE *f = fopen("/proc/self/stat", "r");
	if (!f) {
		*user = *sys = 0;
		return;
	}

	// Field 14 and 15 are utime and stime, after a comm field that may contain spaces.
	char line[1024];
	if (!fgets(line, sizeof(line), f)) {
		fclose(f);
		*user = *sys = 0;
		return;
	}
	fclose(f);

	char *p = strrchr(line, ')');
	if (!p) {
		*user = *sys = 0;
		return;
	}

	unsigned long ut = 0, st = 0;
	sscanf(p + 2, "%*s %*d %*d %*d %*d %*d %*u %*u %*u %*u %*u %lu %lu", &ut, &st);

	double hz = sysconf(_SC_CLK_TCK);
	*user = ut / hz;
	*sys = st / hz;
}

// ---------------------------------------------------------------- main

// Bring the encoder up for a known frame geometry. Deferred until the decoder has told us
// what it is actually producing, so the two agree without us having to guess.
static void setup_encoder(int enc, struct queue *out, struct queue *cap, int width,
                          int height, int bitrate, int fps, int use_dmabuf) {
	struct v4l2_format f = {
		.type = V4L2_BUF_TYPE_VIDEO_OUTPUT_MPLANE,
		.fmt.pix_mp = {
			.width = width, .height = height,
			.pixelformat = V4L2_PIX_FMT_YUV420,
			.num_planes = 1,
		},
	};
	if (xioctl(enc, VIDIOC_S_FMT, &f) == -1)
		die("encoder S_FMT output");

	struct v4l2_format c = {
		.type = V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE,
		.fmt.pix_mp = {
			.width = width, .height = height,
			.pixelformat = V4L2_PIX_FMT_H264,
			.num_planes = 1,
			.plane_fmt[0].sizeimage = 1024 * 1024,
		},
	};
	if (xioctl(enc, VIDIOC_S_FMT, &c) == -1)
		die("encoder S_FMT capture");

	// CarPlay wants High profile, no B-frames, and the parameter sets sent once at the
	// start of the stream rather than repeated (AIS R29 30.2.8.1).
	struct { unsigned id; int value; const char *name; } ctrls[] = {
		{ V4L2_CID_MPEG_VIDEO_BITRATE, bitrate, "bitrate" },
		{ V4L2_CID_MPEG_VIDEO_H264_PROFILE, V4L2_MPEG_VIDEO_H264_PROFILE_HIGH,
		  "profile=high" },
		{ V4L2_CID_MPEG_VIDEO_H264_I_PERIOD, fps > 0 ? fps * 2 : 60, "i-period" },
		{ V4L2_CID_MPEG_VIDEO_REPEAT_SEQ_HEADER, 0, "repeat-seq-header=0" },
	};
	for (size_t i = 0; i < sizeof(ctrls) / sizeof(*ctrls); i++) {
		struct v4l2_control ctrl = { .id = ctrls[i].id, .value = ctrls[i].value };
		if (xioctl(enc, VIDIOC_S_CTRL, &ctrl) == -1)
			fprintf(stderr, "warning: could not set %s: %s\n", ctrls[i].name,
			        strerror(errno));
	}

	queue_alloc(enc, out, V4L2_BUF_TYPE_VIDEO_OUTPUT_MPLANE,
	            use_dmabuf ? V4L2_MEMORY_DMABUF : V4L2_MEMORY_MMAP, 6, "enc-out");
	queue_alloc(enc, cap, V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE, V4L2_MEMORY_MMAP, 6,
	            "enc-cap");

	stream_on(enc, V4L2_BUF_TYPE_VIDEO_OUTPUT_MPLANE, "enc-out");
	stream_on(enc, V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE, "enc-cap");

	for (int i = 0; i < cap->count; i++) {
		struct v4l2_plane p = {0};
		struct v4l2_buffer b = {
			.type = V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE,
			.memory = V4L2_MEMORY_MMAP, .index = i, .m.planes = &p, .length = 1,
		};
		if (xioctl(enc, VIDIOC_QBUF, &b) == -1)
			die("encoder QBUF capture[%d]", i);
		cap->b[i].queued = 1;
	}
}

// Handle V4L2_EVENT_SOURCE_CHANGE: read back the real decoded geometry and allocate the
// CAPTURE queue against it.
static size_t setup_decoder_capture(int dec, struct queue *cap, int use_dmabuf, int *width,
                                    int *height) {
	struct v4l2_format c = { .type = V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE };
	if (xioctl(dec, VIDIOC_G_FMT, &c) == -1)
		die("decoder G_FMT capture");

	c.fmt.pix_mp.pixelformat = V4L2_PIX_FMT_YUV420;
	if (xioctl(dec, VIDIOC_S_FMT, &c) == -1)
		die("decoder S_FMT capture");

	*width = c.fmt.pix_mp.width;
	*height = c.fmt.pix_mp.height;
	size_t frame_size = c.fmt.pix_mp.plane_fmt[0].sizeimage;

	int want = 8;
	struct v4l2_control min = { .id = V4L2_CID_MIN_BUFFERS_FOR_CAPTURE };
	if (xioctl(dec, VIDIOC_G_CTRL, &min) == 0 && min.value + 3 > want)
		want = min.value + 3;
	if (want > MAX_BUFS)
		want = MAX_BUFS;

	queue_alloc(dec, cap, V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE, V4L2_MEMORY_MMAP, want,
	            "dec-cap");
	if (use_dmabuf)
		queue_export(dec, cap, "dec-cap");

	stream_on(dec, V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE, "dec-cap");

	for (int i = 0; i < cap->count; i++) {
		struct v4l2_plane p = {0};
		struct v4l2_buffer b = {
			.type = V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE,
			.memory = V4L2_MEMORY_MMAP, .index = i, .m.planes = &p, .length = 1,
		};
		if (xioctl(dec, VIDIOC_QBUF, &b) == -1)
			die("decoder QBUF capture[%d]", i);
		cap->b[i].queued = 1;
	}

	vlog("source change: %dx%d, %zu bytes/frame, %d buffers", *width, *height, frame_size,
	     cap->count);
	return frame_size;
}

static void usage(const char *argv0) {
	fprintf(stderr,
	        "usage: %s -i FILE -W WIDTH -H HEIGHT [options]\n"
	        "  -i FILE     Annex-B H.264 input\n"
	        "  -W, -H      dimensions\n"
	        "  -f FPS      submit cadence, 0 = as fast as possible (default 30)\n"
	        "  -n N        stop after N frames (default 300)\n"
	        "  -b BPS      encoder bitrate (default 5000000)\n"
	        "  -m MODE     dec, enc, or both (default both)\n"
	        "  -D          hand frames over as dmabuf instead of copying\n"
	        "  -o FILE     write the encoded output here\n"
	        "  -v          verbose\n",
	        argv0);
	exit(2);
}

int main(int argc, char **argv) {
	const char *input = NULL, *output = NULL, *mode = "both";
	int width = 0, height = 0, fps = 30, limit = 300, bitrate = 5000000, use_dmabuf = 0;

	setvbuf(stdout, NULL, _IOLBF, 0);

	int opt;
	while ((opt = getopt(argc, argv, "i:W:H:f:n:b:m:o:Dvh")) != -1) {
		switch (opt) {
		case 'i': input = optarg; break;
		case 'W': width = atoi(optarg); break;
		case 'H': height = atoi(optarg); break;
		case 'f': fps = atoi(optarg); break;
		case 'n': limit = atoi(optarg); break;
		case 'b': bitrate = atoi(optarg); break;
		case 'm': mode = optarg; break;
		case 'o': output = optarg; break;
		case 'D': use_dmabuf = 1; break;
		case 'v': verbose = 1; break;
		default: usage(argv[0]);
		}
	}

	if (!input || !width || !height)
		usage(argv[0]);

	int want_dec = strcmp(mode, "enc") != 0;
	int want_enc = strcmp(mode, "dec") != 0;

	if (!want_dec && use_dmabuf)
		die("enc-only mode has no decoder to import from");

	// ------------------------------------------------------------ input

	int in_fd = open(input, O_RDONLY);
	if (in_fd == -1)
		die("open %s", input);

	struct stat st;
	fstat(in_fd, &st);
	uint8_t *data = mmap(NULL, st.st_size, PROT_READ, MAP_PRIVATE, in_fd, 0);
	if (data == MAP_FAILED)
		die("mmap input");

	int au_count = 0;
	struct au *aus = split_access_units(data, st.st_size, &au_count);
	printf("input: %s, %.1f MiB, %d access units, %dx%d\n", input,
	       st.st_size / 1048576.0, au_count, width, height);

	size_t frame_size = (size_t)width * height * 3 / 2;

	FILE *out = NULL;
	if (output && !(out = fopen(output, "wb")))
		die("open %s", output);

	// ------------------------------------------------------------ decoder

	int dec = -1;
	int dec_cap_ready_init = 0;
	struct queue dec_out = {0}, dec_cap = {0};

	if (want_dec) {
		dec = open("/dev/video10", O_RDWR | O_NONBLOCK);
		if (dec == -1)
			die("open /dev/video10");

		struct v4l2_format f = {
			.type = V4L2_BUF_TYPE_VIDEO_OUTPUT_MPLANE,
			.fmt.pix_mp = {
				.width = width, .height = height,
				.pixelformat = V4L2_PIX_FMT_H264,
				.num_planes = 1,
				.plane_fmt[0].sizeimage = 1024 * 1024,
			},
		};
		if (xioctl(dec, VIDIOC_S_FMT, &f) == -1)
			die("decoder S_FMT output");

		// bcm2835-codec is a stateful decoder: it only knows the frame geometry once it
		// has parsed an SPS, and signals that with V4L2_EVENT_SOURCE_CHANGE. Setting up
		// the CAPTURE queue before that just stalls, so only OUTPUT is started here.
		struct v4l2_event_subscription sub = { .type = V4L2_EVENT_SOURCE_CHANGE };
		if (xioctl(dec, VIDIOC_SUBSCRIBE_EVENT, &sub) == -1)
			die("decoder subscribe source-change");

		struct v4l2_event_subscription eos = { .type = V4L2_EVENT_EOS };
		xioctl(dec, VIDIOC_SUBSCRIBE_EVENT, &eos);

		queue_alloc(dec, &dec_out, V4L2_BUF_TYPE_VIDEO_OUTPUT_MPLANE, V4L2_MEMORY_MMAP,
		            6, "dec-out");
		stream_on(dec, V4L2_BUF_TYPE_VIDEO_OUTPUT_MPLANE, "dec-out");

		// bcm2835-codec propagates the OUTPUT geometry to CAPTURE straight away and
		// does not raise SOURCE_CHANGE for a stream whose size we already told it, so
		// set the capture side up now. The event stays subscribed in case it ever does
		// renegotiate, which would invalidate these buffers.
		dec_cap_ready_init = 1;
	}

	// ------------------------------------------------------------ encoder

	int enc = -1;
	struct queue enc_out = {0}, enc_cap = {0};
	int enc_src[MAX_BUFS];  // encoder output buffer -> decoder capture buffer it holds
	for (int i = 0; i < MAX_BUFS; i++)
		enc_src[i] = -1;

	int dec_cap_ready = !want_dec;
	int enc_ready = 0;

	if (dec_cap_ready_init) {
		frame_size = setup_decoder_capture(dec, &dec_cap, use_dmabuf, &width, &height);
		dec_cap_ready = 1;
	}

	// The encoder is configured for whatever geometry the decoder settled on, so it
	// comes up after the capture queue above.
	if (want_enc) {
		enc = open("/dev/video11", O_RDWR | O_NONBLOCK);
		if (enc == -1)
			die("open /dev/video11");

		setup_encoder(enc, &enc_out, &enc_cap, width, height, bitrate, fps, use_dmabuf);
		enc_ready = 1;
	}


	// ------------------------------------------------------------ run

	uint64_t *lat_dec = calloc(limit, sizeof(uint64_t));
	uint64_t *lat_enc = calloc(limit, sizeof(uint64_t));
	uint64_t *lat_tot = calloc(limit, sizeof(uint64_t));
	int n_dec = 0, n_enc = 0, n_tot = 0;

	int submitted = 0, done = 0;
	size_t bytes_out = 0;
	double interval_us = fps > 0 ? 1000000.0 / fps : 0;

	double cpu_user0, cpu_sys0;
	cpu_times(&cpu_user0, &cpu_sys0);
	uint64_t wall0 = now_us();

	uint64_t last_progress = wall0;

	while (done < limit) {
		uint64_t now = now_us();

		// Nothing moving for five seconds means the pipeline is wedged; report what we
		// have rather than hanging.
		if (now - last_progress > 5000000) {
			fprintf(stderr,
			        "stalled after %d submitted / %d done (dec_cap=%d enc=%d)\n",
			        submitted, done, dec_cap_ready, enc_ready);
			break;
		}

		// --- feed the decoder (or the encoder directly in enc-only mode)
		int due = fps <= 0 || (double)(now - wall0) >= submitted * interval_us;

		if (due && submitted < limit && submitted < au_count && (want_dec || enc_ready)) {
			int target_fd = want_dec ? dec : enc;
			struct queue *q = want_dec ? &dec_out : &enc_out;

			int idx = -1;
			for (int i = 0; i < q->count; i++)
				if (!q->b[i].queued) { idx = i; break; }

			if (idx >= 0) {
				uint64_t key = now;
				struct track *t = track_get(key, 1);
				if (t)
					t->submitted = now;

				size_t len;
				if (want_dec) {
					const struct au *a = &aus[submitted];
					len = a->length;
					if (len > q->b[idx].length)
						die("access unit %d is %zu bytes, buffer is %zu",
						    submitted, len, q->b[idx].length);
					memcpy(q->b[idx].start, data + a->offset, len);
				} else {
					len = frame_size < q->b[idx].length ? frame_size
					                                    : q->b[idx].length;
					memset(q->b[idx].start, 0x80, len);
				}

				struct v4l2_plane p = { .bytesused = len, .length = q->b[idx].length };
				struct v4l2_buffer b = {
					.type = q->type, .memory = V4L2_MEMORY_MMAP, .index = idx,
					.m.planes = &p, .length = 1,
				};
				key_to_tv(key, &b.timestamp);

				if (xioctl(target_fd, VIDIOC_QBUF, &b) == -1)
					die("QBUF input[%d]", idx);

				q->b[idx].queued = 1;
				submitted++;
			}
		}

		// --- wait for something to happen
		struct pollfd fds[2];
		int nfds = 0;
		if (dec != -1)
			fds[nfds++] = (struct pollfd){ .fd = dec,
			                               .events = POLLIN | POLLOUT | POLLPRI };
		if (enc != -1 && enc_ready)
			fds[nfds++] = (struct pollfd){ .fd = enc, .events = POLLIN | POLLOUT };

		int timeout = fps > 0 ? 5 : 200;
		int ready = poll(fds, nfds, timeout);
		if (ready == -1 && errno != EINTR)
			die("poll");

		// --- the decoder has worked out the real geometry
		if (dec != -1 && !dec_cap_ready) {
			struct v4l2_event ev;
			while (xioctl(dec, VIDIOC_DQEVENT, &ev) == 0) {
				if (ev.type != V4L2_EVENT_SOURCE_CHANGE)
					continue;
				frame_size = setup_decoder_capture(dec, &dec_cap, use_dmabuf,
				                                   &width, &height);
				dec_cap_ready = 1;
				last_progress = now_us();

				if (want_enc && !enc_ready) {
					setup_encoder(enc, &enc_out, &enc_cap, width, height, bitrate,
					              fps, use_dmabuf);
					enc_ready = 1;
				}
			}
		}

		// --- reclaim decoder input buffers
		if (dec != -1) {
			for (;;) {
				struct v4l2_plane p = {0};
				struct v4l2_buffer b = {
					.type = V4L2_BUF_TYPE_VIDEO_OUTPUT_MPLANE,
					.memory = V4L2_MEMORY_MMAP, .m.planes = &p, .length = 1,
				};
				if (xioctl(dec, VIDIOC_DQBUF, &b) == -1)
					break;
				dec_out.b[b.index].queued = 0;
			}
		}

		// --- decoded frames out
		if (dec != -1 && dec_cap_ready) {
			for (;;) {
				struct v4l2_plane p = {0};
				struct v4l2_buffer b = {
					.type = V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE,
					.memory = V4L2_MEMORY_MMAP, .m.planes = &p, .length = 1,
				};
				if (xioctl(dec, VIDIOC_DQBUF, &b) == -1)
					break;

				dec_cap.b[b.index].queued = 0;

				uint64_t key = key_of(&b.timestamp);
				struct track *t = track_get(key, 0);
				uint64_t t_now = now_us();
				if (t) {
					t->decoded = t_now;
					if (n_dec < limit)
						lat_dec[n_dec++] = t_now - t->submitted;
				}

				if (!want_enc) {
					done++;
					last_progress = now_us();
					if (n_tot < limit && t)
						lat_tot[n_tot++] = t_now - t->submitted;
					struct v4l2_plane rp = {0};
					struct v4l2_buffer rb = {
						.type = V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE,
						.memory = V4L2_MEMORY_MMAP, .index = b.index,
						.m.planes = &rp, .length = 1,
					};
					xioctl(dec, VIDIOC_QBUF, &rb);
					dec_cap.b[b.index].queued = 1;
					continue;
				}

				// hand it to the encoder
				int idx = -1;
				for (int i = 0; i < enc_out.count; i++)
					if (!enc_out.b[i].queued) { idx = i; break; }

				if (idx < 0) {
					// No encoder slot free; give the frame back and let the
					// decoder stall rather than dropping it silently.
					struct v4l2_plane rp = {0};
					struct v4l2_buffer rb = {
						.type = V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE,
						.memory = V4L2_MEMORY_MMAP, .index = b.index,
						.m.planes = &rp, .length = 1,
					};
					xioctl(dec, VIDIOC_QBUF, &rb);
					dec_cap.b[b.index].queued = 1;
					continue;
				}

				size_t used = p.bytesused ? p.bytesused : frame_size;
				struct v4l2_plane ep = { .bytesused = used };
				struct v4l2_buffer eb = {
					.type = V4L2_BUF_TYPE_VIDEO_OUTPUT_MPLANE,
					.memory = enc_out.memory, .index = idx,
					.m.planes = &ep, .length = 1, .timestamp = b.timestamp,
				};

				if (use_dmabuf) {
					ep.m.fd = dec_cap.b[b.index].dmafd;
					ep.length = dec_cap.b[b.index].length;
					enc_src[idx] = b.index;
				} else {
					size_t n = used < enc_out.b[idx].length ? used
					                                        : enc_out.b[idx].length;
					memcpy(enc_out.b[idx].start, dec_cap.b[b.index].start, n);
					ep.bytesused = n;
					ep.length = enc_out.b[idx].length;
				}

				if (xioctl(enc, VIDIOC_QBUF, &eb) == -1)
					die("encoder QBUF output[%d]", idx);
				enc_out.b[idx].queued = 1;

				if (t)
					t->encoded = 0;

				// With a copy the decoder buffer is free immediately; with dmabuf
				// the encoder still owns it until we dequeue that output buffer.
				if (!use_dmabuf) {
					struct v4l2_plane rp = {0};
					struct v4l2_buffer rb = {
						.type = V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE,
						.memory = V4L2_MEMORY_MMAP, .index = b.index,
						.m.planes = &rp, .length = 1,
					};
					xioctl(dec, VIDIOC_QBUF, &rb);
					dec_cap.b[b.index].queued = 1;
				}
			}
		}

		// --- reclaim encoder input buffers, releasing the frames behind them
		if (enc != -1 && enc_ready) {
			for (;;) {
				struct v4l2_plane p = {0};
				struct v4l2_buffer b = {
					.type = V4L2_BUF_TYPE_VIDEO_OUTPUT_MPLANE,
					.memory = enc_out.memory, .m.planes = &p, .length = 1,
				};
				if (xioctl(enc, VIDIOC_DQBUF, &b) == -1)
					break;

				enc_out.b[b.index].queued = 0;

				int src = enc_src[b.index];
				if (src >= 0) {
					struct v4l2_plane rp = {0};
					struct v4l2_buffer rb = {
						.type = V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE,
						.memory = V4L2_MEMORY_MMAP, .index = src,
						.m.planes = &rp, .length = 1,
					};
					xioctl(dec, VIDIOC_QBUF, &rb);
					dec_cap.b[src].queued = 1;
					enc_src[b.index] = -1;
				}
			}
		}

		// --- encoded frames out
		if (enc != -1 && enc_ready) {
			for (;;) {
				struct v4l2_plane p = {0};
				struct v4l2_buffer b = {
					.type = V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE,
					.memory = V4L2_MEMORY_MMAP, .m.planes = &p, .length = 1,
				};
				if (xioctl(enc, VIDIOC_DQBUF, &b) == -1)
					break;

				uint64_t t_now = now_us();
				uint64_t key = key_of(&b.timestamp);
				struct track *t = track_get(key, 0);

				if (t) {
					if (t->decoded && n_enc < limit)
						lat_enc[n_enc++] = t_now - t->decoded;
					if (n_tot < limit)
						lat_tot[n_tot++] = t_now - t->submitted;
				}

				bytes_out += p.bytesused;
				if (out && p.bytesused)
					fwrite(enc_cap.b[b.index].start, 1, p.bytesused, out);

				done++;
				last_progress = t_now;

				struct v4l2_plane rp = {0};
				struct v4l2_buffer rb = {
					.type = V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE,
					.memory = V4L2_MEMORY_MMAP, .index = b.index,
					.m.planes = &rp, .length = 1,
				};
				xioctl(enc, VIDIOC_QBUF, &rb);
			}
		}

		if (submitted >= au_count && submitted == done)
			break;
	}

	uint64_t wall1 = now_us();
	double cpu_user1, cpu_sys1;
	cpu_times(&cpu_user1, &cpu_sys1);

	// ------------------------------------------------------------ results

	double elapsed = (wall1 - wall0) / 1000000.0;
	double cpu = (cpu_user1 - cpu_user0) + (cpu_sys1 - cpu_sys0);
	double frame_ms = fps > 0 ? 1000.0 / fps : 0;

	printf("\nmode=%s transfer=%s cadence=%s %dx%d\n", mode,
	       use_dmabuf ? "dmabuf" : "memcpy",
	       fps > 0 ? "realtime" : "unthrottled", width, height);
	printf("frames: %d in %.2f s = %.1f fps\n", done, elapsed, done / elapsed);
	if (want_enc)
		printf("output: %.2f MiB = %.2f Mbps\n", bytes_out / 1048576.0,
		       bytes_out * 8.0 / elapsed / 1000000.0);
	printf("cpu:    %.2f s = %.1f%% of one core, %.1f%% of the machine\n", cpu,
	       100.0 * cpu / elapsed, 100.0 * cpu / elapsed / sysconf(_SC_NPROCESSORS_ONLN));

	printf("latency:\n");
	if (want_dec)
		report("decode", lat_dec, n_dec, frame_ms);
	if (want_enc && want_dec)
		report("encode", lat_enc, n_enc, frame_ms);
	report("end to end", lat_tot, n_tot, frame_ms);

	if (out)
		fclose(out);
	if (dec != -1) {
		stream_off(dec, V4L2_BUF_TYPE_VIDEO_OUTPUT_MPLANE);
		stream_off(dec, V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE);
		close(dec);
	}
	if (enc != -1) {
		stream_off(enc, V4L2_BUF_TYPE_VIDEO_OUTPUT_MPLANE);
		stream_off(enc, V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE);
		close(enc);
	}

	return 0;
}
