// Copyright (c) Heeler contributors
// SPDX-License-Identifier: Apache-2.0
//
// Heeler's addition to libzt, copied with heeler_zerotier.h into the pinned
// libzt source tree's src/ by Scripts/build-native.sh; libzt's CMake globs
// src/*.cpp into the zt-static library. See heeler_zerotier.h.

#include "heeler_zerotier.h"

#include "Events.hpp"
#include "NodeService.hpp"

#include "Buffer.hpp"
#include "C25519.hpp"
#include "Node.hpp"
#include "SHA512.hpp"
#include "World.hpp"
#include "VirtualTap.hpp"
#include "lwip/netif.h"
#include "lwip/tcpip.h"

#include <algorithm>
#include <vector>

#include <arpa/inet.h>
#include <netinet/in.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>

namespace ZeroTier {
extern NodeService* zts_service;
extern Mutex service_m;
}   // namespace ZeroTier

using namespace ZeroTier;

namespace {

// Formats one physical path as "ip/port" (ZeroTier's own notation).
bool heeler_format_path(const struct sockaddr_storage* address, char* out, size_t len)
{
    char ip[INET6_ADDRSTRLEN] = { 0 };
    unsigned int port = 0;
    if (address->ss_family == AF_INET) {
        const struct sockaddr_in* in4 = reinterpret_cast<const struct sockaddr_in*>(address);
        if (! inet_ntop(AF_INET, &(in4->sin_addr), ip, sizeof(ip))) {
            return false;
        }
        port = ntohs(in4->sin_port);
    }
    else if (address->ss_family == AF_INET6) {
        const struct sockaddr_in6* in6 = reinterpret_cast<const struct sockaddr_in6*>(address);
        if (! inet_ntop(AF_INET6, &(in6->sin6_addr), ip, sizeof(ip))) {
            return false;
        }
        port = ntohs(in6->sin6_port);
    }
    else {
        return false;
    }
    int written = snprintf(out, len, "%s/%u", ip, port);
    return written > 0 && static_cast<size_t>(written) < len;
}

// Appends `entry` to the comma-separated list in `paths` if it fits whole.
void heeler_append_path(char* paths, const char* entry)
{
    size_t used = strlen(paths);
    size_t needed = strlen(entry) + (used > 0 ? 1 : 0);
    if (used + needed >= HEELER_ZT_PEER_PATHS_LEN) {
        return;
    }
    if (used > 0) {
        paths[used++] = ',';
    }
    memcpy(paths + used, entry, strlen(entry) + 1);
}

void heeler_copy_peer(const ZT_Peer& source, heeler_zt_peer* out)
{
    memset(out, 0, sizeof(*out));
    out->peer_id = source.address;
    out->latency_ms = source.latency;
    out->role = static_cast<int32_t>(source.role);
    out->path_count = source.pathCount;
    char entry[INET6_ADDRSTRLEN + 8];
    // The preferred path first, then the rest in ZeroTier's order.
    for (int pass = 0; pass < 2; pass++) {
        for (unsigned int i = 0; i < source.pathCount && i < ZT_MAX_PEER_NETWORK_PATHS; i++) {
            const ZT_PeerPhysicalPath& path = source.paths[i];
            if ((pass == 0) != (path.preferred != 0)) {
                continue;
            }
            if (heeler_format_path(&path.address, entry, sizeof(entry))) {
                heeler_append_path(out->paths, entry);
            }
        }
    }
}

// A planet file read and checked, its roots merged by identity.
struct HeelerPlanet {
    uint64_t world_id = 0;
    uint64_t timestamp = 0;
    std::vector<World::Root> roots;
};

int heeler_read_planet(const void* data, unsigned int length, HeelerPlanet& out)
{
    if (! data || length == 0 || length > ZT_WORLD_MAX_SERIALIZED_LENGTH) {
        return HEELER_ZT_PLANET_INVALID;
    }
    World world;
    try {
        const unsigned int used =
            world.deserialize(Buffer<ZT_WORLD_MAX_SERIALIZED_LENGTH>(data, length), 0);
        if (used != length) {
            return HEELER_ZT_PLANET_INVALID;
        }
    }
    catch (...) {
        return HEELER_ZT_PLANET_INVALID;
    }
    if (world.type() != World::TYPE_PLANET) {
        return HEELER_ZT_PLANET_NOT_PLANET;
    }
    out.world_id = world.id();
    out.timestamp = world.timestamp();
    out.roots.clear();
    for (const World::Root& root : world.roots()) {
        if (! root.identity) {
            return HEELER_ZT_PLANET_INVALID;
        }
        bool merged = false;
        for (World::Root& existing : out.roots) {
            if (existing.identity.address() != root.identity.address()) {
                continue;
            }
            if (existing.identity != root.identity) {
                return HEELER_ZT_PLANET_ADDRESS_COLLISION;
            }
            for (const InetAddress& endpoint : root.stableEndpoints) {
                if (std::find(existing.stableEndpoints.begin(), existing.stableEndpoints.end(), endpoint)
                    == existing.stableEndpoints.end()
                    && existing.stableEndpoints.size() < ZT_WORLD_MAX_STABLE_ENDPOINTS_PER_ROOT) {
                    existing.stableEndpoints.push_back(endpoint);
                }
            }
            merged = true;
            break;
        }
        if (! merged) {
            out.roots.push_back(root);
        }
    }
    if (out.roots.empty()) {
        return HEELER_ZT_PLANET_NO_ROOTS;
    }
    std::sort(out.roots.begin(), out.roots.end());
    return ZTS_ERR_OK;
}

// A moon ID from the roots alone: SHA-512 over each root's public identity
// and endpoints, in address order.
uint64_t heeler_roots_id(const std::vector<World::Root>& roots)
{
    Buffer<ZT_WORLD_MAX_SERIALIZED_LENGTH> material;
    for (const World::Root& root : roots) {
        root.identity.serialize(material, false);
        material.append((uint8_t)root.stableEndpoints.size());
        for (const InetAddress& endpoint : root.stableEndpoints) {
            endpoint.serialize(material);
        }
    }
    uint8_t digest[64];
    SHA512(digest, material.data(), material.size());
    uint64_t id = 0;
    for (int i = 0; i < 8; i++) {
        id = (id << 8) | digest[i];
    }
    if (id == 0 || id == ZT_WORLD_ID_EARTH) {
        id ^= 0x8000000000000001ULL;
    }
    return id;
}

}   // namespace

extern "C" int heeler_zt_planet_inspect(const void* planet, unsigned int length, heeler_zt_planet_info* info)
{
    if (! planet || ! info) {
        return ZTS_ERR_ARG;
    }
    HeelerPlanet parsed;
    try {
        const int status = heeler_read_planet(planet, length, parsed);
        if (status != ZTS_ERR_OK) {
            return status;
        }
        info->world_id = parsed.world_id;
        info->timestamp = parsed.timestamp;
        info->roots_id = heeler_roots_id(parsed.roots);
        info->root_count = static_cast<uint32_t>(parsed.roots.size());
    }
    catch (...) {
        return HEELER_ZT_PLANET_INVALID;
    }
    return ZTS_ERR_OK;
}

extern "C" int heeler_zt_add_moon(const void* planet, unsigned int length, uint64_t moon_id)
{
    if (moon_id == 0) {
        return ZTS_ERR_ARG;
    }
    HeelerPlanet parsed;
    Buffer<ZT_WORLD_MAX_SERIALIZED_LENGTH> moon;
    try {
        const int status = heeler_read_planet(planet, length, parsed);
        if (status != ZTS_ERR_OK) {
            return status;
        }
        // Nothing checks this signature: a local moon is taken as is, and
        // no root sends an update to a moon it does not know. The key only
        // makes the World well-formed, so it is thrown away.
        const C25519::Pair key(C25519::generate());
        const World built =
            World::make(World::TYPE_MOON, moon_id, parsed.timestamp, key.pub, parsed.roots, key);
        built.serialize(moon, false);
    }
    catch (...) {
        return HEELER_ZT_PLANET_INVALID;
    }
    // The same locking as heeler_zt_peers, below.
    ACQUIRE_SERVICE(ZTS_ERR_SERVICE);
    Mutex::Lock _lr(zts_service->_run_m);
    if (! zts_service->_run || ! zts_service->_node
        || zts_service->reasonForTermination() != NodeService::ONE_STILL_RUNNING) {
        return ZTS_ERR_SERVICE;
    }
    const ZT_ResultCode result = zts_service->_node->addLocalMoon(NULL, moon.data(), moon.size());
    return result == ZT_RESULT_OK ? ZTS_ERR_OK : ZTS_ERR_ARG;
}

extern "C" int heeler_zt_peers(heeler_zt_peer* peers, unsigned int capacity)
{
    if (capacity > 0 && ! peers) {
        return ZTS_ERR_ARG;
    }
    // Same lock order as the zts_* control functions: the service lock,
    // then the node's run lock. That keeps an orderly stop (zts_node_stop
    // clears _run under _run_m) from tearing the node down meanwhile.
    //
    // It cannot cover a fatal exit of libzt's service thread: NodeService::run
    // records the reason, then deletes _node without either lock while _run
    // is still true. Every libzt call that reaches _node (zts_moon_orbit,
    // zts_node_get_id, ...) shares that window. Refusing once a termination
    // reason is recorded narrows it to the instant between that record and
    // the delete; closing it would take a change to libzt itself.
    ACQUIRE_SERVICE(ZTS_ERR_SERVICE);
    Mutex::Lock _lr(zts_service->_run_m);
    if (! zts_service->_run || ! zts_service->_node
        || zts_service->reasonForTermination() != NodeService::ONE_STILL_RUNNING) {
        return ZTS_ERR_SERVICE;
    }
    ZT_PeerList* list = zts_service->_node->peers();
    if (! list) {
        return ZTS_ERR_GENERAL;
    }
    unsigned long total = list->peerCount;
    for (unsigned long i = 0; i < total && i < capacity; i++) {
        heeler_copy_peer(list->peers[i], &peers[i]);
    }
    zts_service->_node->freeQueryResult(static_cast<void*>(list));
    return total > 0x7fffffffUL ? 0x7fffffff : static_cast<int>(total);
}

extern "C" int heeler_zt_bind_network(int fd, uint64_t net_id, int family)
{
    if (fd < 0 || net_id == 0 || (family != ZTS_AF_INET && family != ZTS_AF_INET6)) {
        return ZTS_ERR_ARG;
    }
    // lwIP names a netif by its two letters and number ("4b3"); an ifreq
    // carries the name, NUL-terminated, in its first bytes.
    char name[16] = { 0 };
    {
        ACQUIRE_SERVICE(ZTS_ERR_SERVICE);
        Mutex::Lock _ln(zts_service->_nets_m);
        auto network = zts_service->_nets.find(net_id);
        if (network == zts_service->_nets.end() || ! network->second.tap) {
            return ZTS_ERR_NO_RESULT;
        }
        VirtualTap* tap = network->second.tap;
        // `::netif` is lwIP's interface; ZeroTier has its own `netif`.
        struct ::netif* n = (struct ::netif*)(family == ZTS_AF_INET ? tap->netif4 : tap->netif6);
        if (! n) {
            return ZTS_ERR_NO_RESULT;
        }
        // The same lock order as libzt's network configuration: the
        // networks lock, then lwIP's core lock.
        LOCK_TCPIP_CORE();
        const bool named = netif_index_to_name(netif_get_index(n), name) != NULL;
        UNLOCK_TCPIP_CORE();
        if (! named) {
            return ZTS_ERR_NO_RESULT;
        }
    }
    return zts_bsd_setsockopt(fd, ZTS_SOL_SOCKET, ZTS_SO_BINDTODEVICE, name, sizeof(name)) < 0 ? ZTS_ERR_SOCKET
                                                                                             : ZTS_ERR_OK;
}

extern "C" int heeler_zt_network_reaches(uint64_t net_id, int family, const void* address)
{
    if (net_id == 0 || ! address || (family != ZTS_AF_INET && family != ZTS_AF_INET6)) {
        return ZTS_ERR_ARG;
    }
    ACQUIRE_SERVICE(ZTS_ERR_SERVICE);
    Mutex::Lock _ln(zts_service->_nets_m);
    auto network = zts_service->_nets.find(net_id);
    if (network == zts_service->_nets.end() || ! network->second.tap) {
        return ZTS_ERR_NO_RESULT;
    }
    VirtualTap* tap = network->second.tap;
    struct ::netif* n = (struct ::netif*)(family == ZTS_AF_INET ? tap->netif4 : tap->netif6);
    if (! n) {
        return ZTS_ERR_NO_RESULT;
    }
    if (family == ZTS_AF_INET6) {
        // lwIP holds no per-network IPv6 routes: not checked.
        return ZTS_ERR_OK;
    }
    ip4_addr_t dest;
    memcpy(&dest.addr, address, sizeof(dest.addr));
    int result = HEELER_ZT_ERR_NO_ROUTE;
    LOCK_TCPIP_CORE();
    if (! netif_is_up(n) || ! netif_is_link_up(n) || ip4_addr_isany_val(*netif_ip4_addr(n))) {
        result = ZTS_ERR_NO_RESULT;
    }
    else if (ip4_addr_netcmp(&dest, netif_ip4_addr(n), netif_ip4_netmask(n))) {
        result = ZTS_ERR_OK;
    }
    else {
        // The network's own managed routes through a gateway on its subnet,
        // as lwIP's routing hook (patch 0002) uses them.
        uint32_t via = 0;
        unsigned int bits = 0;
        if (tap->gatewayFor4(ip4_addr_get_u32(&dest), &via, &bits)) {
            ip4_addr_t gateway;
            ip4_addr_set_u32(&gateway, via);
            if (ip4_addr_netcmp(&gateway, netif_ip4_addr(n), netif_ip4_netmask(n))) {
                result = ZTS_ERR_OK;
            }
        }
    }
    UNLOCK_TCPIP_CORE();
    return result;
}
