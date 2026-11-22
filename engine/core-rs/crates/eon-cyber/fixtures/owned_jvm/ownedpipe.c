/* Fixed-fixture OpenJDK JDWP transport: the already-connected anonymous stdin/stdout
 * pipes of OwnedFixture. No socket, address lookup, listener, or reconnect. */
#include <jdwpTransport.h>
#include <errno.h>
#include <stdint.h>
#include <stdatomic.h>
#include <string.h>
#include <unistd.h>

#define OK JDWPTRANSPORT_ERROR_NONE
#define IO JDWPTRANSPORT_ERROR_IO_ERROR
#define MAX_PACKET (256 * 1024)

static jdwpTransportCallback *callbacks;
static atomic_bool connected;
static int used;

static int anonymous_pipe(const char *fd) {
    char name[64];
    ssize_t n = readlink(fd, name, sizeof(name));
    return n >= 8 && n < (ssize_t)sizeof(name) &&
           memcmp(name, "pipe:[", 6) == 0 && name[n - 1] == ']';
}

static int transfer(void *data, size_t len, int sending) {
    unsigned char *p = data;
    while (len) {
        ssize_t n = sending ? write(STDOUT_FILENO, p, len) : read(STDIN_FILENO, p, len);
        if (n < 0 && errno == EINTR) continue;
        if (n <= 0) return 0;
        p += n;
        len -= (size_t)n;
    }
    return 1;
}

static jdwpTransportError attach(jdwpTransportEnv *env, const char *address, jlong t, jlong h) {
    if (used || !address || strcmp(address, "owned-fixture") ||
        !anonymous_pipe("/proc/self/fd/0") || !anonymous_pipe("/proc/self/fd/1"))
        return JDWPTRANSPORT_ERROR_ILLEGAL_ARGUMENT;
    used = 1;
    char handshake[] = "JDWP-Handshake", incoming[sizeof(handshake) - 1];
    if (!transfer(incoming, sizeof(incoming), 0) || memcmp(handshake, incoming, sizeof(incoming)) ||
        !transfer(handshake, sizeof(incoming), 1)) return IO;
    atomic_store(&connected, 1);
    return OK;
}

static uint32_t get32(const unsigned char *h) {
    return ((uint32_t)h[0] << 24) | ((uint32_t)h[1] << 16) | ((uint32_t)h[2] << 8) | h[3];
}

static void put32(unsigned char *h, uint32_t n) {
    h[0] = n >> 24; h[1] = n >> 16; h[2] = n >> 8; h[3] = n;
}

/* Only the metadata, event and count operations this JDI probe needs. In
 * particular: no get/set values, object enumeration, invoke, or VM redefinition. */
static int fixture_command(unsigned set, unsigned cmd) {
    switch (set) {
        case 1:  return cmd == 1 || cmd == 2 || cmd == 7 || cmd == 9 || cmd == 17 || cmd == 21;
        case 2:  return cmd == 10 || cmd == 12 || cmd == 15; // interfaces, debug extension, methods
        case 3:  return cmd == 1;                             // superclass
        case 6:  return cmd == 1;                             // line table
        case 11: return cmd == 6 || cmd == 7;                 // frames, frame count
        case 15: return cmd == 1;                             // install event request
        default: return 0;
    }
}

static jdwpTransportError receive(jdwpTransportEnv *env, jdwpPacket *pkt) {
    unsigned char h[JDWP_HEADER_SIZE];
    jdwpCmdPacket *p = &pkt->type.cmd;
    if (!atomic_load(&connected) || !transfer(h, sizeof(h), 0)) return IO;
    uint32_t len = get32(h);
    if (len < sizeof(h) || len > MAX_PACKET || h[8] != JDWPTRANSPORT_FLAGS_NONE ||
        !fixture_command(h[9], h[10])) return IO;
    p->len = (jint)len;
    p->id = (jint)get32(h + 4);
    p->flags = 0;
    p->cmdSet = h[9];
    p->cmd = h[10];
    size_t body = len - sizeof(h);
    p->data = body ? callbacks->alloc((jint)body) : NULL;
    if (body && !p->data) return JDWPTRANSPORT_ERROR_OUT_OF_MEMORY;
    if (!transfer(p->data, body, 0)) {
        callbacks->free(p->data);
        p->data = NULL;
        return IO;
    }
    return OK;
}

static jdwpTransportError send_packet(jdwpTransportEnv *env, const jdwpPacket *pkt) {
    unsigned char h[JDWP_HEADER_SIZE];
    const jdwpCmdPacket *p = &pkt->type.cmd;
    if (!atomic_load(&connected) || p->len < JDWP_HEADER_SIZE || p->len > MAX_PACKET) return IO;
    put32(h, (uint32_t)p->len);
    put32(h + 4, (uint32_t)p->id);
    h[8] = (unsigned char)p->flags;
    jbyte *data;
    if (h[8] == JDWPTRANSPORT_FLAGS_REPLY) {
        uint16_t code = (uint16_t)pkt->type.reply.errorCode;
        h[9] = code >> 8; h[10] = code;
        data = pkt->type.reply.data;
    } else {
        h[9] = p->cmdSet; h[10] = p->cmd;
        data = p->data;
    }
    return transfer(h, sizeof(h), 1) && transfer(data, (size_t)p->len - sizeof(h), 1) ? OK : IO;
}

static jdwpTransportError caps(jdwpTransportEnv *env, JDWPTransportCapabilities *c) {
    memset(c, 0, sizeof(*c));
    return OK;
}
static jboolean is_open(jdwpTransportEnv *env) { return atomic_load(&connected); }
static jdwpTransportError close_pipe(jdwpTransportEnv *env) {
    if (atomic_exchange(&connected, 0)) {
        close(STDIN_FILENO);
        close(STDOUT_FILENO);
    }
    return OK;
}
static jdwpTransportError last_error(jdwpTransportEnv *env, char **message) {
    return JDWPTRANSPORT_ERROR_MSG_NOT_AVAILABLE;
}
static jdwpTransportError no_listen(jdwpTransportEnv *env, const char *a, char **b) {
    return JDWPTRANSPORT_ERROR_ILLEGAL_STATE;
}
static jdwpTransportError no_stop(jdwpTransportEnv *env) {
    return JDWPTRANSPORT_ERROR_ILLEGAL_STATE;
}
static jdwpTransportError no_accept(jdwpTransportEnv *env, jlong t, jlong h) {
    return JDWPTRANSPORT_ERROR_ILLEGAL_STATE;
}

static const struct jdwpTransportNativeInterface_ api = {
    .GetCapabilities = caps, .Attach = attach, .StartListening = no_listen,
    .StopListening = no_stop, .Accept = no_accept, .IsOpen = is_open,
    .Close = close_pipe, .ReadPacket = receive, .WritePacket = send_packet,
    .GetLastError = last_error
};
static jdwpTransportEnv transport = &api;

JNIEXPORT jint JNICALL jdwpTransport_OnLoad(JavaVM *vm, jdwpTransportCallback *cb,
                                           jint version, jdwpTransportEnv **env) {
    if (version != JDWPTRANSPORT_VERSION_1_0) return JNI_EVERSION;
    callbacks = cb;
    *env = &transport;
    return JNI_OK;
}
