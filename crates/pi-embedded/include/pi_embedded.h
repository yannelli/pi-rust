/* C ABI of the pi-embedded crate (src/ffi.rs) for Swift and other native hosts. Methods and
 * payloads match upstream pi-embedded's native bridge (src/native.rs, src/native_host.rs).
 * Strings are NUL-terminated UTF-8, valid only for the duration of a call. */
#ifndef PI_EMBEDDED_H
#define PI_EMBEDDED_H

#ifdef __cplusplus
extern "C" {
#endif

typedef struct PiEmbedded PiEmbedded;

/* A core request: answer it with pi_embedded_receive and the same id. `command.result`
 * carries {"value":...} or {"error":"..."} for a finished command and needs no answer. */
typedef void (*PiHostCallFn)(void *context, const char *id, const char *method, const char *payload_json);

/* A session event, or {"version":1,"type":"host_error","error":"..."}. */
typedef void (*PiHostEventFn)(void *context, const char *event_json);

/* Both callbacks and `context` must be usable from any thread. NULL when startup fails. */
PiEmbedded *pi_embedded_new(PiHostCallFn call, PiHostEventFn event, void *context);

/* Run a command with a JSON object payload; its outcome arrives as `command.result`. */
void pi_embedded_command(PiEmbedded *handle, const char *id, const char *method, const char *payload_json);

/* Answer a core request. A non-NULL `error` rejects it; otherwise `result_json` is its value. */
void pi_embedded_receive(PiEmbedded *handle, const char *id, const char *result_json, const char *error);

/* Stop the runtime and release the handle. Do not call it from inside a callback. */
void pi_embedded_free(PiEmbedded *handle);

#ifdef __cplusplus
}
#endif

#endif /* PI_EMBEDDED_H */
