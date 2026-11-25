// Offline verifier for review_etw.ps1. Never decode, copy or print user data.
#include <windows.h>
#include <evntrace.h>
#include <evntcons.h>
#include <errno.h>
#include <stdio.h>
#include <string.h>
#include <wchar.h>

static const GUID provider = {
    0x87a9f157, 0xe3c2, 0x4589, {0x93, 0xa0, 0x29, 0x9a, 0x3d, 0x32, 0x5e, 0x72}
};
// Id, Version, Channel, Level, Opcode, Task, Keyword (16 bytes, no padding).
static const EVENT_DESCRIPTOR completion = {1, 1, 0, 4, 0, 0, 1};

typedef struct {
    DWORD in_pid, out_pid;
    ULONG in_count, out_count;
    BOOL descriptor_matches;
    USHORT payload_bytes;
} Review;

static void WINAPI on_record(EVENT_RECORD *event) {
    Review *review = (Review *)event->UserContext;
    const EVENT_HEADER *header = &event->EventHeader;
    if (!IsEqualGUID(&header->ProviderId, &provider)) return;
    // Count all of this provider's records from either child, even unexpected IDs.
    if (header->ProcessId == review->out_pid) {
        review->out_count++;
    } else if (header->ProcessId == review->in_pid) {
        review->in_count++;
        review->descriptor_matches =
            memcmp(&header->EventDescriptor, &completion, sizeof(completion)) == 0;
        review->payload_bytes = event->UserDataLength;
    }
}

static int accepted(const Review *review) {
    return review->in_count == 1 && review->out_count == 0 &&
           review->descriptor_matches && review->payload_bytes == 0;
}

// Exercise the same callback and verdict; no control event enters the trace.
static int accepts_control(USHORT length) {
    Review review = {1, 2, 0, 0, FALSE, 0};
    EVENT_RECORD event = {0};
    BYTE byte = 0;
    event.UserContext = &review;
    event.EventHeader.ProviderId = provider;
    event.EventHeader.ProcessId = review.in_pid;
    event.EventHeader.EventDescriptor = completion;
    event.UserData = &byte;
    event.UserDataLength = length;
    on_record(&event);
    return accepted(&review);
}

static DWORD parse_pid(const wchar_t *text) {
    wchar_t *end;
    errno = 0;
    DWORD pid = wcstoul(text, &end, 10);
    return *text >= L'1' && *text <= L'9' && !*end && !errno ? pid : 0;
}

int wmain(int argc, wchar_t **argv) {
    Review review = {0};
    EVENT_TRACE_LOGFILEW log = {0};
    if (argc != 4 || !(review.in_pid = parse_pid(argv[2])) ||
        !(review.out_pid = parse_pid(argv[3])) || review.in_pid == review.out_pid) {
        fputs("Expected an ETL path and distinct opted-in and opted-out PIDs.\n", stderr);
        return 1;
    }
    if (!accepts_control(0) || accepts_control(1)) {
        fputs("Raw zero/one-byte payload controls failed.\n", stderr);
        return 1;
    }
    log.LogFileName = argv[1];
    log.ProcessTraceMode = PROCESS_TRACE_MODE_EVENT_RECORD;
    log.EventRecordCallback = on_record;
    log.Context = &review;
    TRACEHANDLE trace = OpenTraceW(&log);
    if (trace == INVALID_PROCESSTRACE_HANDLE) {
        fprintf(stderr, "OpenTraceW failed: %lu\n", GetLastError());
        return 1;
    }
    ULONG processed = ProcessTrace(&trace, 1, NULL, NULL);
    ULONG closed = CloseTrace(trace);
    if (processed != ERROR_SUCCESS || closed != ERROR_SUCCESS) {
        fprintf(stderr, "ProcessTrace: %lu; CloseTrace: %lu\n", processed, closed);
        return 1;
    }
    if (!log.LogfileHeader.EndTime.QuadPart || log.LogfileHeader.EventsLost || log.LogfileHeader.BuffersLost) {
        fprintf(stderr, "Incomplete ETL: events lost=%lu, buffers lost=%lu, finalized=%d\n",
                log.LogfileHeader.EventsLost, log.LogfileHeader.BuffersLost, log.LogfileHeader.EndTime.QuadPart != 0);
        return 1;
    }
    if (!accepted(&review)) {
        fprintf(stderr, "Opted-in records=%lu, opted-out=%lu, descriptor matches=%d, payload bytes=%u\n",
                review.in_count, review.out_count, review.descriptor_matches, (unsigned)review.payload_bytes);
        return 1;
    }
    return 0;
}
