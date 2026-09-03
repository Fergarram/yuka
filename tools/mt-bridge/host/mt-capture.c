//
// mt-capture: read the Mac trackpad's raw multitouch and stream it to the guest.
//
// Uses Apple's private MultitouchSupport.framework to get per-finger contact
// frames (normalized position, size, state) and forwards them, in the protocol.md
// wire format, to the om_wm dev VM where mt-inject turns them into a uinput
// touchpad. This is the host half of the bridge; see tools/mt-bridge/README.md.
//
// Build (on the Mac):
//   clang -O2 -Wall tools/mt-bridge/host/mt-capture.c -o mt-capture \
//       -F/System/Library/PrivateFrameworks \
//       -framework MultitouchSupport -framework CoreFoundation
//
// Run:
//   ./mt-capture                 listen on 192.168.64.1:9999 (the vmnet host IP)
//   ./mt-capture 0.0.0.0 9999    bind elsewhere
//
// The guest's mt-inject connects in; this then streams while you touch the pad.
// Private API, so it can break on an OS update; that is the deal for trackpad
// multitouch on macOS.
//

#include <arpa/inet.h>
#include <netinet/in.h>
#include <netinet/tcp.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#include <CoreFoundation/CoreFoundation.h>

//
// MultitouchSupport private API (reverse-engineered, stable for years)
//

typedef struct { float x, y; } MTPoint;
typedef struct { MTPoint position, velocity; } MTVector;

// One contact in a frame. Layout matters: it is read straight from the framework.
typedef struct {
    int32_t frame;
    double timestamp;
    int32_t identifier;   // stable per-finger id
    int32_t state;        // MTTouchState: 3 make, 4 touch, 6 linger, etc.
    int32_t fingerId;
    int32_t handId;
    MTVector normalized;  // position/velocity normalized to 0..1 on the surface
    float zTotal;         // total pressure/area for this contact
    int32_t unknown3;
    float angle;
    float majorAxis;
    float minorAxis;
    MTVector absolute;
    int32_t unknown4;
    int32_t unknown5[2];
    float zDensity;
} MTTouch;

typedef void *MTDeviceRef;
typedef int (*MTContactCallback)(MTDeviceRef, MTTouch *, int32_t, double, int32_t);

extern CFMutableArrayRef MTDeviceCreateList(void);
extern void MTRegisterContactFrameCallback(MTDeviceRef, MTContactCallback);
extern void MTDeviceStart(MTDeviceRef, int32_t);
extern void MTDeviceStop(MTDeviceRef);

//
// Wire format (must match protocol.md and mt-inject.c)
//

#define MAX_SLOTS 16

typedef struct __attribute__((packed)) {
    int32_t id;
    float x, y, size;
    uint32_t state;
} WireContact;

//
// Shared connection state
//

static pthread_mutex_t g_lock = PTHREAD_MUTEX_INITIALIZER;
static int g_client = -1;
static volatile int g_frames = 0; // for the heartbeat line

static void set_client(int fd) {
    pthread_mutex_lock(&g_lock);
    if (g_client >= 0) close(g_client);
    g_client = fd;
    pthread_mutex_unlock(&g_lock);
}

static void drop_client(void) {
    pthread_mutex_lock(&g_lock);
    if (g_client >= 0) close(g_client);
    g_client = -1;
    pthread_mutex_unlock(&g_lock);
}

// Send the whole buffer, or report the client as gone.
static void send_frame(const uint8_t *buf, size_t len) {
    pthread_mutex_lock(&g_lock);
    int fd = g_client;
    pthread_mutex_unlock(&g_lock);
    if (fd < 0) return;

    size_t sent = 0;
    while (sent < len) {
        ssize_t w = send(fd, buf + sent, len - sent, 0);
        if (w <= 0) { drop_client(); return; }
        sent += (size_t)w;
    }
}

//
// MultitouchSupport callback: pack the frame and ship it.
//

static int contact_cb(MTDeviceRef dev, MTTouch *touches, int32_t n, double ts, int32_t frame) {
    (void)dev; (void)ts; (void)frame;
    if (n < 0) n = 0;
    if (n > MAX_SLOTS) n = MAX_SLOTS;

    uint8_t buf[1 + MAX_SLOTS * sizeof(WireContact)];
    buf[0] = (uint8_t)n;
    WireContact *out = (WireContact *)(buf + 1);
    for (int i = 0; i < n; i++) {
        out[i].id = touches[i].identifier;
        out[i].x = touches[i].normalized.position.x;
        out[i].y = touches[i].normalized.position.y;
        // majorAxis is roughly finger contact size; scale toward 0..1.
        float size = touches[i].majorAxis / 10.0f;
        if (size > 1.0f) size = 1.0f;
        if (size < 0.0f) size = 0.0f;
        out[i].size = size;
        out[i].state = (uint32_t)touches[i].state;
    }
    send_frame(buf, 1 + (size_t)n * sizeof(WireContact));
    g_frames++;
    return 0;
}

//
// Accept loop, on its own thread, so the main thread can own the CFRunLoop that
// drives the multitouch callbacks.
//

static int g_listen = -1;

static void *accept_thread(void *arg) {
    (void)arg;
    for (;;) {
        struct sockaddr_in ca;
        socklen_t cl = sizeof(ca);
        int c = accept(g_listen, (struct sockaddr *)&ca, &cl);
        if (c < 0) continue;
        int one = 1;
        setsockopt(c, IPPROTO_TCP, TCP_NODELAY, &one, sizeof(one));
        char ip[INET_ADDRSTRLEN];
        inet_ntop(AF_INET, &ca.sin_addr, ip, sizeof(ip));
        fprintf(stderr, "mt-capture: guest connected from %s\n", ip);
        set_client(c);
    }
    return NULL;
}

//
// Heartbeat: one line a second so you can see frames flowing while you touch.
//

static void heartbeat(CFRunLoopTimerRef t, void *info) {
    (void)t; (void)info;
    int f = g_frames;
    g_frames = 0;
    pthread_mutex_lock(&g_lock);
    int connected = g_client >= 0;
    pthread_mutex_unlock(&g_lock);
    fprintf(stderr, "mt-capture: %s, %d frames/s\r", connected ? "connected" : "waiting", f);
    fflush(stderr);
}

int main(int argc, char **argv) {
    const char *host = "192.168.64.1";
    int port = 9999;
    if (argc >= 2) host = argv[1];
    if (argc >= 3) port = atoi(argv[2]);

    // Listening socket.
    g_listen = socket(AF_INET, SOCK_STREAM, 0);
    int one = 1;
    setsockopt(g_listen, SOL_SOCKET, SO_REUSEADDR, &one, sizeof(one));
    struct sockaddr_in a;
    memset(&a, 0, sizeof(a));
    a.sin_family = AF_INET;
    a.sin_port = htons((uint16_t)port);
    if (inet_pton(AF_INET, host, &a.sin_addr) != 1) {
        fprintf(stderr, "mt-capture: bad bind host %s\n", host);
        return 2;
    }
    if (bind(g_listen, (struct sockaddr *)&a, sizeof(a)) < 0) {
        perror("bind");
        return 1;
    }
    if (listen(g_listen, 1) < 0) {
        perror("listen");
        return 1;
    }
    fprintf(stderr, "mt-capture: listening on %s:%d\n", host, port);

    pthread_t th;
    pthread_create(&th, NULL, accept_thread, NULL);

    // Start every multitouch device (built-in trackpad and any Magic Trackpad).
    CFMutableArrayRef devs = MTDeviceCreateList();
    if (!devs || CFArrayGetCount(devs) == 0) {
        fprintf(stderr, "mt-capture: no multitouch devices found\n");
        return 1;
    }
    CFIndex ndev = CFArrayGetCount(devs);
    for (CFIndex i = 0; i < ndev; i++) {
        MTDeviceRef d = (MTDeviceRef)CFArrayGetValueAtIndex(devs, i);
        MTRegisterContactFrameCallback(d, contact_cb);
        MTDeviceStart(d, 0);
    }
    fprintf(stderr, "mt-capture: %ld multitouch device(s) started\n", (long)ndev);

    // One-second heartbeat on the run loop.
    CFRunLoopTimerRef timer = CFRunLoopTimerCreate(
        kCFAllocatorDefault, CFAbsoluteTimeGetCurrent() + 1.0, 1.0, 0, 0, heartbeat, NULL);
    CFRunLoopAddTimer(CFRunLoopGetCurrent(), timer, kCFRunLoopCommonModes);

    CFRunLoopRun();
    return 0;
}
