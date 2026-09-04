//
// mt-inject: a synthetic multitouch trackpad for the yuka dev VM.
//
// Creates a uinput device that looks enough like an Apple/MacBook trackpad that
// libinput classifies it as a touchpad (so yuka finds it and, in Custom mode,
// reads its raw evdev protocol-B stream through touch.rs). It then injects
// contacts, either from the host over TCP (mt-capture on the Mac) or, with
// --test, from a built-in synthetic gesture generator.
//
// Build (in the guest):
//   gcc -O2 -Wall tools/mt-bridge/guest/mt-inject.c -o ~/mt-inject
//
// Run:
//   ~/mt-inject --test                 synthetic pan/pinch, no host needed
//   ~/mt-inject                         connect to 192.168.64.1:9999 (the Mac)
//   ~/mt-inject --connect 10.0.0.2:9999
//
// Needs write access to /dev/uinput (root, or a udev rule / the input group).
//

#include <errno.h>
#include <fcntl.h>
#include <math.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#include <arpa/inet.h>
#include <netinet/in.h>
#include <netinet/tcp.h>
#include <sys/ioctl.h>
#include <sys/socket.h>
#include <time.h>

#include <linux/input.h>
#include <linux/uinput.h>

//
// Device shape. The axis span matches the trackpad touch.rs was tuned against
// (~9760 x 6750 units), so its distance thresholds keep their intended feel.
//

#define AXIS_X_MAX 9760
#define AXIS_Y_MAX 6750
#define TOUCH_MAJOR_MAX 1020
#define PRESSURE_MAX 255
#define MAX_SLOTS 16

// One contact, in the wire format's normalized form (see protocol.md).
typedef struct {
    int32_t id;
    float x;      // 0 left .. 1 right
    float y;      // 0 bottom .. 1 top
    float size;   // ~0 .. 1
    uint32_t state;
} Contact;

// Slot bookkeeping: which host id currently owns each uinput MT slot.
static int32_t slot_id[MAX_SLOTS];

//
// uinput helpers
//

static void emit(int fd, uint16_t type, uint16_t code, int32_t value) {
    struct input_event ev;
    memset(&ev, 0, sizeof(ev));
    ev.type = type;
    ev.code = code;
    ev.value = value;
    if (write(fd, &ev, sizeof(ev)) != (ssize_t)sizeof(ev)) {
        // A short write here means the device went away; nothing to salvage.
        perror("write(uinput)");
        exit(1);
    }
}

static void syn(int fd) { emit(fd, EV_SYN, SYN_REPORT, 0); }

static void abs_setup(int fd, uint16_t code, int32_t min, int32_t max) {
    struct uinput_abs_setup a;
    memset(&a, 0, sizeof(a));
    a.code = code;
    a.absinfo.minimum = min;
    a.absinfo.maximum = max;
    if (ioctl(fd, UI_ABS_SETUP, &a) < 0) {
        perror("UI_ABS_SETUP");
        exit(1);
    }
}

static int create_device(void) {
    int fd = open("/dev/uinput", O_WRONLY | O_NONBLOCK);
    if (fd < 0) {
        fprintf(stderr, "open /dev/uinput: %s\n", strerror(errno));
        fprintf(stderr, "  need root, or the 'input' group plus a udev rule on /dev/uinput\n");
        exit(1);
    }

    ioctl(fd, UI_SET_EVBIT, EV_SYN);
    ioctl(fd, UI_SET_EVBIT, EV_KEY);
    ioctl(fd, UI_SET_EVBIT, EV_ABS);

    // Mark this as a touchpad (indirect pointing), not a touchscreen. We do NOT
    // set INPUT_PROP_BUTTONPAD: we forward real left/right buttons from the Mac,
    // and a buttonpad that also reports BTN_RIGHT trips libinput's "clickpad
    // advertising right button" kernel-bug warning.
    ioctl(fd, UI_SET_PROPBIT, INPUT_PROP_POINTER);

    // Buttons. BTN_TOOL_* finger counts are what libinput reads to decide the
    // device is a touchpad with a tap-finger-count, which is yuka's test for it.
    ioctl(fd, UI_SET_KEYBIT, BTN_LEFT);
    ioctl(fd, UI_SET_KEYBIT, BTN_RIGHT);
    ioctl(fd, UI_SET_KEYBIT, BTN_TOUCH);
    ioctl(fd, UI_SET_KEYBIT, BTN_TOOL_FINGER);
    ioctl(fd, UI_SET_KEYBIT, BTN_TOOL_DOUBLETAP);
    ioctl(fd, UI_SET_KEYBIT, BTN_TOOL_TRIPLETAP);
    ioctl(fd, UI_SET_KEYBIT, BTN_TOOL_QUADTAP);
    ioctl(fd, UI_SET_KEYBIT, BTN_TOOL_QUINTTAP);

    // Single-touch axes (pointer emulation) plus the protocol-B multitouch axes.
    ioctl(fd, UI_SET_ABSBIT, ABS_X);
    ioctl(fd, UI_SET_ABSBIT, ABS_Y);
    ioctl(fd, UI_SET_ABSBIT, ABS_PRESSURE);
    ioctl(fd, UI_SET_ABSBIT, ABS_MT_SLOT);
    ioctl(fd, UI_SET_ABSBIT, ABS_MT_TRACKING_ID);
    ioctl(fd, UI_SET_ABSBIT, ABS_MT_POSITION_X);
    ioctl(fd, UI_SET_ABSBIT, ABS_MT_POSITION_Y);
    ioctl(fd, UI_SET_ABSBIT, ABS_MT_TOUCH_MAJOR);
    ioctl(fd, UI_SET_ABSBIT, ABS_MT_TOUCH_MINOR);

    abs_setup(fd, ABS_X, 0, AXIS_X_MAX);
    abs_setup(fd, ABS_Y, 0, AXIS_Y_MAX);
    abs_setup(fd, ABS_PRESSURE, 0, PRESSURE_MAX);
    abs_setup(fd, ABS_MT_SLOT, 0, MAX_SLOTS - 1);
    abs_setup(fd, ABS_MT_TRACKING_ID, 0, 65535);
    abs_setup(fd, ABS_MT_POSITION_X, 0, AXIS_X_MAX);
    abs_setup(fd, ABS_MT_POSITION_Y, 0, AXIS_Y_MAX);
    abs_setup(fd, ABS_MT_TOUCH_MAJOR, 0, TOUCH_MAJOR_MAX);
    abs_setup(fd, ABS_MT_TOUCH_MINOR, 0, TOUCH_MAJOR_MAX);

    struct uinput_setup us;
    memset(&us, 0, sizeof(us));
    us.id.bustype = BUS_USB;
    // Apple's vendor/product for a Magic Trackpad 2, so quirks that key off it
    // treat this device the way the real one would be treated.
    us.id.vendor = 0x05ac;
    us.id.product = 0x0265;
    us.id.version = 0x0001;
    strncpy(us.name, "yuka mt-bridge trackpad", UINPUT_MAX_NAME_SIZE - 1);
    if (ioctl(fd, UI_DEV_SETUP, &us) < 0) {
        perror("UI_DEV_SETUP");
        exit(1);
    }
    if (ioctl(fd, UI_DEV_CREATE) < 0) {
        perror("UI_DEV_CREATE");
        exit(1);
    }

    for (int i = 0; i < MAX_SLOTS; i++) slot_id[i] = -1;
    return fd;
}

static int contact_on_surface(uint32_t state) {
    // making(3), touching(4), lingering(6): a real contact. Everything else is a
    // hover or a lift, so the finger is treated as gone.
    return state == 3 || state == 4 || state == 6;
}

// Turn one frame of contacts into a protocol-B report. buttons: bit0 left, bit1 right.
static void inject_frame(int fd, const Contact *c, int n, uint8_t buttons) {
    int want_slot[MAX_SLOTS];
    for (int i = 0; i < MAX_SLOTS; i++) want_slot[i] = -1;

    // Keep only real contacts, cap at MAX_SLOTS.
    Contact live[MAX_SLOTS];
    int live_n = 0;
    for (int i = 0; i < n && live_n < MAX_SLOTS; i++) {
        if (contact_on_surface(c[i].state)) live[live_n++] = c[i];
    }

    // Assign each live contact to the slot it already holds, else a free one.
    for (int i = 0; i < live_n; i++) {
        int s = -1;
        for (int k = 0; k < MAX_SLOTS; k++)
            if (slot_id[k] == live[i].id) { s = k; break; }
        if (s < 0) {
            for (int k = 0; k < MAX_SLOTS; k++)
                if (slot_id[k] == -1 && want_slot[k] == -1) { s = k; break; }
        }
        if (s >= 0) want_slot[s] = i;
    }

    float total_size = 0.0f;
    int primary = -1;

    for (int s = 0; s < MAX_SLOTS; s++) {
        int ci = want_slot[s];
        if (ci < 0) {
            // Slot becoming empty: retract its finger once.
            if (slot_id[s] != -1) {
                emit(fd, EV_ABS, ABS_MT_SLOT, s);
                emit(fd, EV_ABS, ABS_MT_TRACKING_ID, -1);
                slot_id[s] = -1;
            }
            continue;
        }
        const Contact *ct = &live[ci];
        int new_finger = (slot_id[s] != ct->id);
        emit(fd, EV_ABS, ABS_MT_SLOT, s);
        if (new_finger) {
            // A non-negative, unique-enough tracking id: the host's own id.
            emit(fd, EV_ABS, ABS_MT_TRACKING_ID, ct->id & 0xffff);
            slot_id[s] = ct->id;
        }
        int px = (int)(ct->x * AXIS_X_MAX);
        int py = (int)((1.0f - ct->y) * AXIS_Y_MAX); // Linux MT Y grows downward
        if (px < 0) px = 0;
        if (px > AXIS_X_MAX) px = AXIS_X_MAX;
        if (py < 0) py = 0;
        if (py > AXIS_Y_MAX) py = AXIS_Y_MAX;
        int major = (int)(ct->size * TOUCH_MAJOR_MAX);
        emit(fd, EV_ABS, ABS_MT_POSITION_X, px);
        emit(fd, EV_ABS, ABS_MT_POSITION_Y, py);
        emit(fd, EV_ABS, ABS_MT_TOUCH_MAJOR, major);
        emit(fd, EV_ABS, ABS_MT_TOUCH_MINOR, major);
        total_size += ct->size;
        if (primary < 0) primary = ci;
    }

    // Single-touch emulation and the finger-count tool bits.
    emit(fd, EV_KEY, BTN_TOUCH, live_n > 0 ? 1 : 0);
    emit(fd, EV_KEY, BTN_TOOL_FINGER, live_n == 1 ? 1 : 0);
    emit(fd, EV_KEY, BTN_TOOL_DOUBLETAP, live_n == 2 ? 1 : 0);
    emit(fd, EV_KEY, BTN_TOOL_TRIPLETAP, live_n == 3 ? 1 : 0);
    emit(fd, EV_KEY, BTN_TOOL_QUADTAP, live_n == 4 ? 1 : 0);
    emit(fd, EV_KEY, BTN_TOOL_QUINTTAP, live_n >= 5 ? 1 : 0);
    if (primary >= 0) {
        int px = (int)(live[primary].x * AXIS_X_MAX);
        int py = (int)((1.0f - live[primary].y) * AXIS_Y_MAX);
        emit(fd, EV_ABS, ABS_X, px);
        emit(fd, EV_ABS, ABS_Y, py);
    }
    int pressure = (int)(total_size * PRESSURE_MAX);
    if (pressure > PRESSURE_MAX) pressure = PRESSURE_MAX;
    emit(fd, EV_ABS, ABS_PRESSURE, pressure);

    // Physical click, from the host's mouse buttons. The kernel drops repeats,
    // so re-emitting the same level every frame is a no-op.
    emit(fd, EV_KEY, BTN_LEFT, (buttons & 1) ? 1 : 0);
    emit(fd, EV_KEY, BTN_RIGHT, (buttons & 2) ? 1 : 0);

    syn(fd);
}

//
// Synthetic gesture generator (--test)
//

static void sleep_ms(int ms) {
    struct timespec ts = { ms / 1000, (long)(ms % 1000) * 1000000L };
    nanosleep(&ts, NULL);
}

static void run_test(int fd) {
    fprintf(stderr, "mt-inject: --test, injecting pan then pinch on a loop (ctrl-c to stop)\n");
    const int hz = 60, ms = 1000 / hz;
    double t = 0.0;
    for (;;) {
        // Two fingers. Phase splits pan (both translate) and pinch (spread).
        double cycle = fmod(t, 8.0);
        double cx, span;
        if (cycle < 4.0) {
            // Pan: centre sweeps left-right, fixed spread.
            double u = (cycle < 2.0) ? (cycle / 2.0) : (2.0 - (cycle - 2.0) / 2.0);
            cx = 0.3 + 0.4 * u;
            span = 0.12;
        } else {
            // Pinch: centre fixed, spread grows and shrinks.
            double u = (cycle < 6.0) ? ((cycle - 4.0) / 2.0) : (2.0 - (cycle - 6.0) / 2.0);
            cx = 0.5;
            span = 0.06 + 0.20 * u;
        }
        Contact c[2];
        c[0] = (Contact){ .id = 0, .x = (float)(cx - span), .y = 0.5f, .size = 0.5f, .state = 4 };
        c[1] = (Contact){ .id = 1, .x = (float)(cx + span), .y = 0.5f, .size = 0.5f, .state = 4 };
        inject_frame(fd, c, 2, 0);
        sleep_ms(ms);
        t += (double)ms / 1000.0;
    }
}

//
// Network client: pull frames from the host and inject them.
//

static int read_full(int sock, void *buf, size_t n) {
    uint8_t *p = buf;
    size_t got = 0;
    while (got < n) {
        ssize_t r = read(sock, p + got, n - got);
        if (r == 0) return 0;         // peer closed
        if (r < 0) { if (errno == EINTR) continue; return -1; }
        got += (size_t)r;
    }
    return 1;
}

static void run_client(int fd, const char *host, int port) {
    for (;;) {
        int sock = socket(AF_INET, SOCK_STREAM, 0);
        struct sockaddr_in a;
        memset(&a, 0, sizeof(a));
        a.sin_family = AF_INET;
        a.sin_port = htons((uint16_t)port);
        if (inet_pton(AF_INET, host, &a.sin_addr) != 1) {
            fprintf(stderr, "mt-inject: bad host %s\n", host);
            exit(1);
        }
        fprintf(stderr, "mt-inject: connecting to %s:%d ...\n", host, port);
        if (connect(sock, (struct sockaddr *)&a, sizeof(a)) < 0) {
            close(sock);
            sleep_ms(1000);
            continue;
        }
        int one = 1;
        setsockopt(sock, IPPROTO_TCP, TCP_NODELAY, &one, sizeof(one));
        fprintf(stderr, "mt-inject: connected\n");

        for (;;) {
            uint8_t hdr[2];               // count, buttons
            int r = read_full(sock, hdr, 2);
            if (r <= 0) break;
            uint8_t count = hdr[0];
            uint8_t buttons = hdr[1];
            if (count > MAX_SLOTS) count = MAX_SLOTS;
            Contact c[MAX_SLOTS];
            if (count > 0) {
                r = read_full(sock, c, (size_t)count * sizeof(Contact));
                if (r <= 0) break;
            }
            inject_frame(fd, c, count, buttons);
        }
        fprintf(stderr, "mt-inject: host disconnected, retrying\n");
        close(sock);
        // Make sure no finger or button is left stuck down.
        inject_frame(fd, NULL, 0, 0);
        sleep_ms(500);
    }
}

int main(int argc, char **argv) {
    const char *host = "192.168.64.1";
    int port = 9999;
    int test = 0;

    for (int i = 1; i < argc; i++) {
        if (!strcmp(argv[i], "--test")) test = 1;
        else if (!strcmp(argv[i], "--connect") && i + 1 < argc) {
            char *sep = strchr(argv[++i], ':');
            if (sep) { *sep = 0; host = argv[i]; port = atoi(sep + 1); }
            else host = argv[i];
        } else {
            fprintf(stderr, "usage: %s [--test | --connect host:port]\n", argv[0]);
            return 2;
        }
    }

    int fd = create_device();
    fprintf(stderr, "mt-inject: uinput device created\n");
    // Give udev/libinput a moment to notice and classify the new device.
    sleep_ms(400);

    if (test) run_test(fd);
    else run_client(fd, host, port);

    ioctl(fd, UI_DEV_DESTROY);
    close(fd);
    return 0;
}
