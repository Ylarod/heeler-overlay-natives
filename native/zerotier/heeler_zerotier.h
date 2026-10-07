/*
 * Copyright (c) Heeler contributors
 * SPDX-License-Identifier: Apache-2.0
 *
 * Heeler's addition to libzt (NativeSupport/heeler_zerotier.cpp, compiled
 * into CZeroTier). libzt has no way to list a node's peers: the
 * zts_core_query_path* functions are stubs, and the peer events copy
 * ZeroTierOne's ZT_Peer into the differently laid out zts_peer_info_t, so
 * only their first fields are meaningful. heeler_zt_peers reads the node's
 * peer list directly instead.
 *
 * libzt also takes custom roots only as the whole process's planet, at its
 * first start. heeler_zt_planet_inspect and heeler_zt_add_moon instead turn
 * a network's self-hosted planet into a moon built and signed on the device
 * and add it to the running node beside ZeroTier's own planet; the node is
 * patched (NativeSupport/zerotier-patches) to route each peer through the
 * root set that knows it. zts_moon_deorbit removes such a moon.
 *
 * Two joined networks can assign the node the same address, so a socket's
 * source address alone does not name its network: heeler_zt_bind_network
 * binds a socket to one network's interface, and heeler_zt_network_reaches
 * tells beforehand whether that network has a route to the destination.
 */
#ifndef HEELER_ZEROTIER_H
#define HEELER_ZEROTIER_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Room for the active physical paths of one peer as text. */
#define HEELER_ZT_PEER_PATHS_LEN 256

/* One peer of the running node, as ZeroTier::Node::peers reports it. */
typedef struct {
    /* 40-bit ZeroTier address. */
    uint64_t peer_id;
    /* Latency in milliseconds, or -1 when unknown. */
    int32_t latency_ms;
    /* zts_peer_role_t: 0 leaf, 1 moon root, 2 planet root. */
    int32_t role;
    /* Valid direct physical paths; 0 means traffic is relayed by a root. */
    uint32_t path_count;
    /* Those paths as "ip/port", comma-separated, the preferred one first.
     * Entries that do not fit are left out whole. NUL-terminated. */
    char paths[HEELER_ZT_PEER_PATHS_LEN];
} heeler_zt_peer;

/* Copies at most `capacity` of the running node's peers, in address order,
 * into `peers`. Returns the total number of peers, which may exceed
 * `capacity` (call again with a larger buffer), or a negative ZTS_ERR_*
 * code when the node is not running. `peers` may be NULL when `capacity`
 * is 0. Blocks briefly on libzt's service lock. */
int heeler_zt_peers(heeler_zt_peer *peers, unsigned int capacity);

/* Why a planet file was refused (results below ZTS_ERR_*). */
/* Not a serialized ZeroTier World, longer than one, or larger than allowed. */
#define HEELER_ZT_PLANET_INVALID (-100)
/* A World, but a moon (or null) rather than a planet. */
#define HEELER_ZT_PLANET_NOT_PLANET (-101)
/* A planet that lists no roots. */
#define HEELER_ZT_PLANET_NO_ROOTS (-102)
/* Two roots share a ZeroTier address with different identities. */
#define HEELER_ZT_PLANET_ADDRESS_COLLISION (-103)

/* What heeler_zt_planet_inspect reads from a planet file. */
typedef struct {
    /* The planet's world ID (149604618 for ZeroTier's own planet, which
     * many self-hosted planets reuse). */
    uint64_t world_id;
    /* The planet's revision timestamp. */
    uint64_t timestamp;
    /* A non-zero ID derived from the roots alone (their identities and
     * endpoints), never 149604618: the moon ID to use when world_id is
     * ZeroTier's or already taken. */
    uint64_t roots_id;
    /* Distinct roots, after merging entries for the same identity. */
    uint32_t root_count;
} heeler_zt_planet_info;

/* Checks a planet file (as made by ZeroTier's mkworld) and describes it.
 * Needs no running node. Returns ZTS_ERR_OK, ZTS_ERR_ARG for NULL
 * arguments, or a HEELER_ZT_PLANET_* code. */
int heeler_zt_planet_inspect(const void *planet, unsigned int length, heeler_zt_planet_info *info);

/* Adds the planet's roots to the running node as a moon with ID `moon_id`,
 * signed with a throwaway key and the planet's timestamp. Returns
 * ZTS_ERR_OK, a HEELER_ZT_PLANET_* code, ZTS_ERR_ARG for a zero `moon_id`
 * or a moon the node already has under that ID, or ZTS_ERR_SERVICE when the
 * node is not running. Remove the moon with zts_moon_deorbit(moon_id). */
int heeler_zt_add_moon(const void *planet, unsigned int length, uint64_t moon_id);

/* Binds socket `fd` to network `net_id`'s interface for `family`
 * (ZTS_AF_INET or ZTS_AF_INET6), as SO_BINDTODEVICE does: its packets leave
 * through that network alone and only that network's packets reach it, even
 * when another joined network assigned this node the same address. A bound
 * socket bypasses lwIP's routing, so a destination the network cannot reach
 * goes unanswered; check it first with heeler_zt_network_reaches. Returns
 * ZTS_ERR_OK, ZTS_ERR_ARG, ZTS_ERR_NO_RESULT when the network is not joined
 * or has no interface for `family` yet, ZTS_ERR_SERVICE when the node is not
 * running, or ZTS_ERR_SOCKET when the socket refuses the binding. */
int heeler_zt_bind_network(int fd, uint64_t net_id, int family);

/* The network has no route to the destination (heeler_zt_network_reaches). */
#define HEELER_ZT_ERR_NO_ROUTE (-110)

/* Whether network `net_id` reaches `address` by itself: 4 bytes for
 * ZTS_AF_INET, 16 for ZTS_AF_INET6, in network byte order. An IPv4 address
 * is reached when it is on the network's subnet or covered by one of the
 * network's managed routes whose gateway is on that subnet; IPv6 is not
 * checked (any joined network with an IPv6 interface reaches it). Returns
 * ZTS_ERR_OK, HEELER_ZT_ERR_NO_ROUTE, ZTS_ERR_ARG, ZTS_ERR_NO_RESULT when
 * the network is not joined or its interface for `family` is not up with an
 * address yet, or ZTS_ERR_SERVICE when the node is not running. */
int heeler_zt_network_reaches(uint64_t net_id, int family, const void *address);

#ifdef __cplusplus
}
#endif

#endif /* HEELER_ZEROTIER_H */
