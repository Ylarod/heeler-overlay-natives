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
