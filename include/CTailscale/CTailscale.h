#include <stddef.h>
#include <stdint.h>

#include "tailscale.h"

/* Heeler's additions, exported from NativeSupport/heeler_tailscale.go. */

/* Opts the process out of Tailscale log upload from inside the Go runtime
 * (envknob.SetNoLogsNoSupport and logtail.Disable). Call before the first
 * tailscale_new; idempotent. */
extern void heeler_tailscale_disable_log_upload(void);

/* Bit 0: envknob.NoLogsNoSupport() is true. Bit 1: logtail drops log lines
 * instead of buffering them for upload. 3 means log upload is off. */
extern int heeler_tailscale_log_upload_state(void);

/* Signs the started server sd out: tells the coordination server and drops
 * the node key, waiting at most timeout_millis. Returns 0 (also when the node
 * was never logged in), -1 with the reason in tailscale_errmsg, or EBADF. */
extern int heeler_tailscale_logout(int sd, int timeout_millis);

/*
 * tailcat clients, exported from native/tailscale/heeler_tailcat.go.
 *
 * A tailcat client reaches one `tailcat serve` peer through a WireGuard
 * tunnel set up over a DERP relay, with no tailnet and no coordination
 * server: the server's tailcat address ("tc...") holds everything the client
 * needs. By default that includes the tunnel's pre-shared key, so the
 * address is a secret. These functions never put it in a message or in the
 * client's log; heeler_tailcat_open says what that log does name.
 *
 * A handle is one tunnel to one server; every dial on it shares the tunnel.
 * Handles are positive and never reused. Every function blocks the calling
 * thread, and may be called from any thread. Error buffers receive a
 * NUL-terminated UTF-8 message (empty on success) and may be NULL.
 *
 * The clients share the process with the tsnet nodes. tailcat turns
 * Tailscale's socket binding (netns) off for the whole process when a client
 * first builds its tunnel; heeler_tailcat_dial and heeler_tailcat_ping turn
 * it back on before they return, so tsnet nodes keep binding their sockets
 * to the default route's interface. A socket either stack opens while such
 * a call runs is not bound.
 */
#define HEELER_TAILCAT_OK 0
#define HEELER_TAILCAT_ERR (-1)
#define HEELER_TAILCAT_ERR_TIMEOUT (-2)
/* The handle is not open, or was closed while the call waited. */
#define HEELER_TAILCAT_ERR_CLOSED (-3)
/* Not a tailcat address, or one a client cannot use. */
#define HEELER_TAILCAT_ERR_ADDRESS (-4)

/* Describes a tailcat address without touching the network, as
 * NUL-terminated JSON that never holds a key:
 *   {"preshared_key":bool,"region_id":int|null,"relays":[string...]}
 * preshared_key is whether the address carries one; without it, whoever
 * runs the relay could join the tunnel. region_id is the region of the relay
 * map (https://tailcat.dev/derpmap.json) the client fetches before its first
 * connection, or null when the address names its own relays, whose host
 * names are in relays. Returns the JSON length without the terminator; a
 * result >= len means nothing was written and the buffer must grow. Returns
 * HEELER_TAILCAT_ERR_ADDRESS for anything else. */
extern int heeler_tailcat_address_info(const char *address, char *buf, size_t len);

/* Creates a client for the server address names. Nothing reaches the
 * network until the first dial or ping. The client's log lines are written
 * to log_fd, which the caller owns and must keep open until
 * heeler_tailcat_close returns; nothing is written afterwards. -1 discards
 * them. The lines are tailcat's and Tailscale's own: they name the server's
 * public keys, its relay, and the endpoints tried, and (as of tailcat
 * v0.7.0) not the pre-shared key. For an address without a pre-shared key
 * that is all a client needs, so such a log is as secret as the address.
 * Returns the handle, or HEELER_TAILCAT_ERR_ADDRESS. */
extern int heeler_tailcat_open(const char *address, int log_fd, char *err, size_t errlen);

/* Opens a TCP stream to port (1 to 65535) on the client's server: the port
 * `tailcat serve <port>` exposes. The first dial brings the tunnel up:
 * fetching the relay map when the address needs it, connecting to the
 * relay, and registering with the server, which gives up after 10 seconds
 * whatever the timeout. All of it and the connection share timeout_ms.
 *
 * Returns a connected AF_UNIX descriptor owned by the caller, the same kind
 * tailscale_dial stores: closing it ends the stream, and a shutdown of
 * either direction reaches the server. Returns a negative HEELER_TAILCAT_ERR_*
 * code otherwise. A port the server does not serve answers nothing, so the
 * dial runs into its timeout; a port it serves with nothing listening behind
 * it is accepted, and the stream ends at once. */
extern int heeler_tailcat_dial(int client, uint16_t port, uint32_t timeout_ms, char *err, size_t errlen);

/* Measures the path to the server, bringing the tunnel up like a dial, as
 * NUL-terminated JSON:
 *   {"latency_ms":number,"direct":bool,"endpoint":string|null,"relay":string|null}
 * direct is whether the answer came over a direct UDP path, from endpoint
 * ("ip:port"); otherwise relay is the code of the relay region that carried
 * it. Each ping also prompts both ends to look for a direct path. Returns
 * the JSON length without the terminator (>= len: nothing was written), or
 * a negative HEELER_TAILCAT_ERR_* code. */
extern int heeler_tailcat_ping(int client, uint32_t timeout_ms, char *buf, size_t len, char *err, size_t errlen);

/* Closes a client: its tunnel, its relay connection, and every stream
 * dialled through it, which then end at their descriptors (the caller still
 * closes those). Dials and pings waiting on the handle from other threads
 * return HEELER_TAILCAT_ERR_CLOSED. Returns HEELER_TAILCAT_OK, or
 * HEELER_TAILCAT_ERR_CLOSED for a handle that is not open. */
extern int heeler_tailcat_close(int client);
