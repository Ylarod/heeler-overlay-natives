/*
 * heeler-easytier: in-process, TUN-less EasyTier nodes behind a C ABI.
 *
 * Copyright (C) 2026 Heeler contributors
 * SPDX-License-Identifier: LGPL-3.0-or-later
 *
 * The process runs any number of EasyTier networks side by side (at most
 * HEELER_ET_MAX_INSTANCES keys at once). Every call names an instance key,
 * 1 to 128 bytes of printable UTF-8 the caller chooses (Heeler uses the
 * Overlay Network's UUID). A key holds either one network from a TOML
 * configuration (heeler_et_start) or one EasyTier config-server session
 * (heeler_et_web_start) running up to HEELER_ET_MAX_WEB_NETWORKS networks the
 * server assigns; starting either under a key replaces what the key held and
 * never touches another key. Every network is its own EasyTier instance with
 * its own peers, routes and userspace TCP/IP stack, all on one shared
 * two-thread runtime. A dial goes through exactly one network of its key and
 * resolves hostnames in that network's route table only.
 *
 * Every function blocks the calling thread; call them from a dedicated
 * queue or thread, never from a Swift cooperative-pool task or the main
 * thread. Error buffers receive a NUL-terminated UTF-8 message (empty on
 * success) and may be NULL.
 */
#ifndef HEELER_EASYTIER_H
#define HEELER_EASYTIER_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* The multi-instance ABI: every function takes an instance key. */
#define HEELER_ET_ABI_VERSION 2

#define HEELER_ET_OK 0
#define HEELER_ET_ERR (-1)
#define HEELER_ET_ERR_TIMEOUT (-2)
#define HEELER_ET_ERR_NOT_RUNNING (-3)
#define HEELER_ET_ERR_UNRESOLVED (-4)
#define HEELER_ET_ERR_AMBIGUOUS (-5)

#define HEELER_ET_MAX_INSTANCES 32
#define HEELER_ET_MAX_WEB_NETWORKS 8

/* Starts key's network from an EasyTier TOML configuration under Heeler's
 * outbound-only policy: no_tun, no exit-node service, no relaying of other
 * peers' data or foreign networks, and private mode are forced; a
 * configuration with listeners, exit_nodes, proxy_network, port_forward,
 * socks5_proxy, vpn_portal_config, or an empty network name or secret is
 * refused. A fixed ipv4 ("a.b.c.d/n", n 1 to 32, unicast) turns DHCP off; an
 * ipv4 that does not parse or is not unicast is refused. Waits at most
 * timeout_ms for EasyTier to start; joining the network and getting an
 * address happen afterwards (poll heeler_et_status_json). Starting the
 * configuration the key already runs returns HEELER_ET_OK at once; anything
 * else the key held (a network or a config-server session) is stopped first.
 * A configuration that does not parse or is refused leaves the key as it was;
 * any later failure leaves the key empty. A new key beyond
 * HEELER_ET_MAX_INSTANCES is refused. Returns HEELER_ET_OK or a negative
 * HEELER_ET_ERR_* code. */
int heeler_et_start(const char *key, const char *toml, uint32_t timeout_ms, char *err, size_t errlen);

/* Stops key's network or config-server session, if any; its dialled streams
 * fail afterwards. Other keys keep running. A NULL key is a no-op. */
void heeler_et_stop(const char *key);

/* Stops every key's network and session (app suspension). */
void heeler_et_stop_all(void);

/* Starts (or keeps) key's config-server session: connects to url as the
 * device machine_id (a UUID the caller keeps; never derived from the device)
 * named hostname (empty: "Heeler"), and runs the networks the server assigns
 * (at most HEELER_ET_MAX_WEB_NETWORKS, with distinct network names).
 * url is udp:// or tcp://host:port/<token>, or ws:// or wss://host[:port]/
 * .../<token>; a wss:// server must present a certificate the system trusts.
 * The token is the console user name. The connection upgrades to EasyTier's
 * encrypted web tunnel (Noise NN, which does not authenticate the server)
 * whenever the server offers it. With secure_mode non-zero a server that does
 * not offer it, or a path that strips the offer, is never used: the session
 * retries instead of running in clear text. With secure_mode 0 such a server
 * is used in clear text, where the token and the networks it sends can be
 * read and changed on the path. ws:// sends the token in clear text in its
 * HTTP upgrade request either way.
 *
 * Every network a server assigns gets the heeler_et_start policy after its
 * listener_urls are dropped; a credential file, disabled encryption, or
 * managed credentials are refused too, and so are a network whose name
 * another of the session's networks has and one beyond the limit. Refused
 * networks are reported to the server as failed instances and in the status.
 * Config patches are refused; the server's own configuration is what it reads
 * back. Network reports to the server carry no underlay address (interface,
 * LAN or public addresses, ports, tunnel endpoints, peers' public addresses,
 * or management events). A network without a hostname gets this hostname.
 *
 * Returns at once (connecting and joining happen in the background; poll
 * heeler_et_status_json). The same url, machine_id, hostname and secure_mode
 * under the same key is a no-op; anything else the key held is stopped
 * first. Returns HEELER_ET_OK or HEELER_ET_ERR for an invalid key, URL or
 * machine ID, or a new key beyond HEELER_ET_MAX_INSTANCES. */
int heeler_et_web_start(const char *key, const char *url, const char *machine_id, const char *hostname,
                        int secure_mode, char *err, size_t errlen);

/* Ends key's config-server session and stops its networks; a manual network
 * under key is left alone. A NULL key is a no-op. */
void heeler_et_web_stop(const char *key);

/* Opens a TCP stream to host:port through exactly one of key's networks.
 * host is an IPv4 literal or a peer's EasyTier hostname (case-insensitive,
 * optionally ending in ".et.net"), resolved in that network's own route
 * table; a hostname shared by several of its peers fails with
 * HEELER_ET_ERR_UNRESOLVED.
 *
 * network (NULL or "": unspecified) names the network: a manual key's
 * network name must equal it, and a config-server session's network is the
 * one with that network name or instance ID (HEELER_ET_ERR_UNRESOLVED if
 * none). Unspecified, a session with one running network uses it; with
 * several, an IPv4 host selects the one network with a peer at exactly that
 * address, else the one network whose own virtual subnet holds it, and a
 * hostname selects the one network with a peer of that name. Several fits
 * fail with HEELER_ET_ERR_AMBIGUOUS, none with HEELER_ET_ERR_UNRESOLVED. A
 * peer's advertised subnets (proxy_cidrs) never select a network.
 *
 * Returns a connected, non-blocking AF_UNIX descriptor with SO_NOSIGPIPE set,
 * owned by the caller (closing it ends the stream), or a negative
 * HEELER_ET_ERR_* code (HEELER_ET_ERR_NOT_RUNNING for an empty key or a
 * session with no running network). */
int heeler_et_tcp_connect_fd(const char *key, const char *network, const char *host, uint16_t port,
                             uint32_t timeout_ms, char *err, size_t errlen);

/* Writes key's status as NUL-terminated JSON. A manual network:
 *   {"mode":"manual","network_name":string,"running":bool,
 *    "ipv4":string|null,"ipv4_prefix":int|null,"hostname":string,
 *    "peer_count":int,"peers":[peer...],"error":string|null}
 * A config-server session:
 *   {"mode":"web","running":bool,"web":{"connected":bool,
 *    "machine_id":string,"networks":[network...],
 *    "failures":[{"instance_id":string,"network_name":string,
 *                 "message":string}...]}}
 * where running is whether any of its networks runs, connected means the
 * server answers now, each network has the manual object's fields from
 * "running" on plus "instance_id" and "network_name", sorted by instance ID,
 * and failures lists the networks the server asked for and this device
 * refused or could not start, oldest first. An empty key is
 * {"running":false}. Each peer, from the network's route table and sorted by
 * peer_id, is
 *   {"peer_id":int,"hostname":string,"ipv4":string|null,"direct":bool,
 *    "cost":int,"latency_ms":number|null}
 * where direct means route cost 1 (a connection of its own) and latency_ms is
 * the connection's latency for a direct peer, or the measured path latency
 * for a relayed one. Returns the JSON length without the terminator; a result
 * >= len means nothing was written and the buffer must grow. Returns
 * HEELER_ET_ERR on failure (a NULL or invalid key). */
int heeler_et_status_json(const char *key, char *buf, size_t len);

#ifdef __cplusplus
}
#endif

#endif /* HEELER_EASYTIER_H */
