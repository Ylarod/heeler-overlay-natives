// heeler-easytier: one in-process, TUN-less EasyTier node behind a C ABI.
//
// Copyright (C) 2026 Heeler contributors
// SPDX-License-Identifier: LGPL-3.0-or-later
//
// This library links EasyTier (LGPL-3.0). It is free software: you can
// redistribute it and/or modify it under the terms of the GNU Lesser General
// Public License as published by the Free Software Foundation, either version 3
// of the License, or (at your option) any later version.
//
// EasyTier keeps process-wide state for its instance, so this library runs at
// most one network at a time: starting a different configuration stops the
// previous one first. The network comes either from a TOML configuration
// (heeler_et_start) or from an EasyTier config server (heeler_et_web_start, see
// web.rs); starting one mode ends the other. Every exported function blocks the
// calling thread and must not be called from inside a Tokio runtime.
//
// The node only dials out. heeler_et_start forces the flags and rejects the
// configuration that would let peers reach through it: no TUN, no exit-node
// service, no relaying of other peers' data, RPC or foreign networks, private
// mode, no public-IPv6 provider, broadcast relay, magic DNS or UPnP, and no
// listeners, subnet proxies, port forwards, SOCKS5 or VPN portals.

mod web;

use std::{
    ffi::{CStr, c_char, c_int},
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    os::fd::{AsRawFd, IntoRawFd},
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{Arc, Mutex, MutexGuard, OnceLock},
    time::Duration,
};

use easytier::common::config::{ConfigLoader, TomlConfigLoader};
use easytier::instance::factory::{NativeCoreInstance, create_native_instance};
use easytier_core::gateway::DataPlaneErrorKind;
use easytier_core::instance::CoreInstanceState;
use tokio::runtime::Runtime;

/// Success.
pub const HEELER_ET_OK: c_int = 0;
/// Generic failure; `err` holds the reason.
pub const HEELER_ET_ERR: c_int = -1;
/// The operation did not finish within its timeout.
pub const HEELER_ET_ERR_TIMEOUT: c_int = -2;
/// No network is running.
pub const HEELER_ET_ERR_NOT_RUNNING: c_int = -3;
/// The host is neither an IPv4 literal nor a peer hostname on the network,
/// or the hostname names more than one peer.
pub const HEELER_ET_ERR_UNRESOLVED: c_int = -4;

/// How long stopping a replaced or stopped network may take before it is
/// abandoned (its tasks are cancelled when the last reference drops).
const STOP_TIMEOUT: Duration = Duration::from_secs(5);

/// Who started the running network.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Owner {
    /// heeler_et_start, from the caller's TOML.
    Manual,
    /// A config server, through the web session.
    Web,
}

struct Running {
    instance: Arc<NativeCoreInstance>,
    /// The caller's TOML verbatim (so an identical start is a no-op), or the
    /// checked TOML a config server's network became.
    toml: String,
    owner: Owner,
}

static RUNTIME: OnceLock<Result<Runtime, String>> = OnceLock::new();
/// Serializes start, stop, and the web session's start and stop. Dials never
/// take it, and neither do the web session's RPC handlers.
static LIFECYCLE: Mutex<()> = Mutex::new(());
static CURRENT: Mutex<Option<Running>> = Mutex::new(None);

#[derive(Debug)]
struct Failure {
    code: c_int,
    message: String,
}

impl Failure {
    fn new(code: c_int, message: impl Into<String>) -> Self {
        Self { code, message: message.into() }
    }

    fn generic(message: impl Into<String>) -> Self {
        Self::new(HEELER_ET_ERR, message)
    }
}

impl From<anyhow::Error> for Failure {
    fn from(error: anyhow::Error) -> Self {
        Self::generic(format!("{error:#}"))
    }
}

impl From<std::io::Error> for Failure {
    fn from(error: std::io::Error) -> Self {
        Self::generic(error.to_string())
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn runtime() -> Result<&'static Runtime, Failure> {
    RUNTIME
        .get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .thread_name("heeler-easytier")
                .build()
                .map_err(|error| format!("cannot create the EasyTier runtime: {error}"))
        })
        .as_ref()
        .map_err(|message| Failure::generic(message.clone()))
}

fn current_instance() -> Option<Arc<NativeCoreInstance>> {
    lock(&CURRENT).as_ref().map(|running| running.instance.clone())
}

/// Copies `message` into the caller's buffer as a NUL-terminated string,
/// truncated on a UTF-8 boundary.
fn write_c_string(message: &str, buf: *mut c_char, len: usize) {
    if buf.is_null() || len == 0 {
        return;
    }
    let mut end = message.len().min(len - 1);
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    // SAFETY: the caller promises `buf` points at `len` writable bytes, and
    // `end < len` leaves room for the terminator.
    unsafe {
        std::ptr::copy_nonoverlapping(message.as_ptr(), buf.cast::<u8>(), end);
        *buf.add(end) = 0;
    }
}

/// Runs `body` with panics stopped at the FFI boundary and maps a failure to
/// its code, writing the message into `err`.
fn ffi_call(err: *mut c_char, errlen: usize, body: impl FnOnce() -> Result<c_int, Failure>) -> c_int {
    let failure = match catch_unwind(AssertUnwindSafe(body)) {
        Ok(Ok(value)) => {
            write_c_string("", err, errlen);
            return value;
        }
        Ok(Err(failure)) => failure,
        Err(panic) => {
            let detail = panic
                .downcast_ref::<&str>()
                .map(|s| (*s).to_owned())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown panic".to_owned());
            Failure::generic(format!("EasyTier panicked: {detail}"))
        }
    };
    write_c_string(&failure.message, err, errlen);
    failure.code
}

/// # Safety
/// `ptr` is null or a NUL-terminated string valid for the call.
unsafe fn borrowed_str<'a>(ptr: *const c_char, what: &str) -> Result<&'a str, Failure> {
    if ptr.is_null() {
        return Err(Failure::generic(format!("{what} is null")));
    }
    // SAFETY: guaranteed by the caller.
    unsafe { CStr::from_ptr(ptr) }
        .to_str()
        .map_err(|_| Failure::generic(format!("{what} is not UTF-8")))
}

fn is_live(instance: &NativeCoreInstance) -> bool {
    matches!(
        instance.state(),
        CoreInstanceState::Starting | CoreInstanceState::Running
    )
}

/// Parses `toml` and applies Heeler's outbound-only policy to it.
fn outbound_only_config(toml: &str) -> Result<TomlConfigLoader, Failure> {
    let config = TomlConfigLoader::new_from_str(toml)?;
    let identity = config.get_network_identity();
    if identity.network_name.trim().is_empty() {
        return Err(Failure::generic("the network name is empty"));
    }
    if identity.network_secret.as_deref().is_none_or(str::is_empty) {
        return Err(Failure::generic("the network secret is empty"));
    }
    let refuse = |what: &str| Err(Failure::generic(format!("{what} is not supported: Heeler only dials out")));
    if config.get_listeners().is_some_and(|listeners| !listeners.is_empty())
        || !config.get_mapped_listeners().is_empty()
    {
        return refuse("listening for peers");
    }
    if !config.get_proxy_cidrs().is_empty() {
        return refuse("proxy_network");
    }
    if !config.get_port_forwards().is_empty() {
        return refuse("port_forward");
    }
    if config.get_socks5_portal().is_some() {
        return refuse("socks5_proxy");
    }
    if config.get_vpn_portal_config().is_some() {
        return refuse("vpn_portal_config");
    }
    if !config.get_exit_nodes().is_empty() {
        return refuse("exit_nodes");
    }
    // A fixed address replaces DHCP. EasyTier drops an ipv4 it cannot parse
    // without a word, which would leave the node with no address at all.
    match config.get_ipv4() {
        Some(inet) => {
            check_static_ipv4(inet.address(), inet.network_length())?;
            config.set_dhcp(false);
        }
        None if declares_top_level_key(toml, "ipv4") => {
            return Err(Failure::generic("ipv4 is not an address with a prefix length, such as 10.144.144.7/24"));
        }
        None => {}
    }
    let mut flags = config.get_flags();
    flags.no_tun = true;
    flags.enable_exit_node = false;
    flags.disable_relay_data = true;
    flags.relay_network_whitelist = String::new();
    flags.private_mode = true;
    // Nothing that serves or relays for other nodes, or opens the network
    // the phone sits on: no peer-RPC relaying, no relaying of foreign
    // networks' KCP/QUIC streams, no KCP/QUIC proxy input or output (not
    // compiled in; defence in depth), no subnet forwarding, no broadcast
    // relay, no magic DNS server, and no UPnP port mappings on the router.
    flags.relay_all_peer_rpc = false;
    flags.enable_relay_foreign_network_kcp = false;
    flags.enable_relay_foreign_network_quic = false;
    flags.enable_kcp_proxy = false;
    flags.enable_quic_proxy = false;
    flags.disable_kcp_input = true;
    flags.disable_quic_input = true;
    flags.proxy_forward_by_system = false;
    flags.enable_udp_broadcast_relay = false;
    flags.accept_dns = false;
    flags.disable_upnp = true;
    config.set_flags(flags);
    // Never lease public IPv6 addresses to peers (nor ask for one: there is
    // no TUN to put it on).
    config.set_ipv6_public_addr_provider(false);
    config.set_ipv6_public_addr_auto(false);
    config.set_ipv6_public_addr_prefix(None);
    Ok(config)
}

/// Whether a bare top-level `key = ...` line appears before the first table.
fn declares_top_level_key(toml: &str, key: &str) -> bool {
    toml.lines()
        .map(str::trim_start)
        .take_while(|line| !line.starts_with('['))
        .any(|line| {
            line.strip_prefix(key)
                .is_some_and(|rest| rest.trim_start().starts_with('='))
        })
}

/// A fixed virtual address must be a unicast address with a prefix of 1 to 32.
fn check_static_ipv4(address: Ipv4Addr, prefix: u8) -> Result<(), Failure> {
    if !(1..=32).contains(&prefix) {
        return Err(Failure::generic(format!("ipv4 prefix length {prefix} is not between 1 and 32")));
    }
    let first = address.octets()[0];
    if first == 0 || first == 127 || first >= 224 {
        return Err(Failure::generic(format!("ipv4 {address} cannot be a virtual address")));
    }
    Ok(())
}

async fn stop_bounded(instance: &Arc<NativeCoreInstance>) {
    // On timeout the instance is abandoned; dropping the last reference
    // cancels its tasks.
    let _ = tokio::time::timeout(STOP_TIMEOUT, instance.stop()).await;
}

fn start(toml: &str, timeout_ms: u32) -> Result<c_int, Failure> {
    let rt = runtime()?;
    // Rejected before anything stops: a bad configuration leaves the running
    // network as it was.
    let config = outbound_only_config(toml)?;
    let _lifecycle = lock(&LIFECYCLE);
    // A manual network replaces a config server's session and its network.
    web::shutdown(rt);

    let previous = {
        let mut current = lock(&CURRENT);
        match current.as_ref() {
            Some(running)
                if running.owner == Owner::Manual && running.toml == toml && is_live(&running.instance) =>
            {
                return Ok(HEELER_ET_OK);
            }
            _ => current.take(),
        }
    };
    let timeout = Duration::from_millis(u64::from(timeout_ms.max(1)));
    // EasyTier constructs and tears down its instance inside a Tokio context.
    let instance = rt.block_on(async {
        if let Some(previous) = previous {
            stop_bounded(&previous.instance).await;
        }
        let instance = create_native_instance(config)?;
        match tokio::time::timeout(timeout, instance.start()).await {
            Ok(result) => result.map_err(Failure::from)?,
            Err(_) => {
                stop_bounded(&instance).await;
                return Err(Failure::new(
                    HEELER_ET_ERR_TIMEOUT,
                    format!("EasyTier did not start within {timeout:?}"),
                ));
            }
        }
        Ok(instance)
    })?;
    *lock(&CURRENT) = Some(Running { instance, toml: toml.to_owned(), owner: Owner::Manual });
    Ok(HEELER_ET_OK)
}

fn stop() {
    let Ok(rt) = runtime() else { return };
    let _lifecycle = lock(&LIFECYCLE);
    web::shutdown(rt);
    let previous = lock(&CURRENT).take();
    if let Some(previous) = previous {
        rt.block_on(async move { stop_bounded(&previous.instance).await });
    }
}

/// Matches a peer by its EasyTier hostname, case-insensitively; a trailing
/// `.et.net` (EasyTier's magic DNS zone, any case) is ignored.
fn hostname_matches(candidate: &str, wanted: &str) -> bool {
    let wanted = wanted.trim_end_matches('.');
    const ZONE: &str = ".et.net";
    let wanted = match wanted.len().checked_sub(ZONE.len()) {
        Some(split)
            if wanted.is_char_boundary(split) && wanted[split..].eq_ignore_ascii_case(ZONE) =>
        {
            &wanted[..split]
        }
        _ => wanted,
    };
    !wanted.is_empty() && candidate.eq_ignore_ascii_case(wanted)
}

/// Picks the one peer address for `host`; two peers sharing a hostname are
/// refused rather than guessed between.
fn resolve_hostname(host: &str, peers: &[(String, Option<Ipv4Addr>)]) -> Result<Ipv4Addr, Failure> {
    let mut matches = peers
        .iter()
        .filter(|(hostname, _)| hostname_matches(hostname, host))
        .filter_map(|(_, address)| *address)
        .collect::<Vec<_>>();
    matches.sort_unstable();
    matches.dedup();
    match matches.as_slice() {
        [address] => Ok(*address),
        [] => Err(Failure::new(
            HEELER_ET_ERR_UNRESOLVED,
            format!("no EasyTier peer named \"{host}\" with a virtual IPv4 address"),
        )),
        many => Err(Failure::new(
            HEELER_ET_ERR_UNRESOLVED,
            format!(
                "\"{host}\" names {} EasyTier peers ({}); dial one by its address",
                many.len(),
                many.iter().map(ToString::to_string).collect::<Vec<_>>().join(", ")
            ),
        )),
    }
}

async fn resolve(instance: &NativeCoreInstance, host: &str) -> Result<Ipv4Addr, Failure> {
    if let Ok(address) = host.parse::<Ipv4Addr>() {
        return Ok(address);
    }
    let peers = instance
        .route_snapshots()
        .await
        .into_iter()
        .map(|route| {
            let address = route.ipv4_addr.and_then(|inet| inet.address).map(Ipv4Addr::from);
            (route.hostname, address)
        })
        .collect::<Vec<_>>();
    resolve_hostname(host, &peers)
}

fn set_nosigpipe(fd: c_int) -> std::io::Result<()> {
    let one: c_int = 1;
    // SAFETY: `fd` is an open socket and `one` outlives the call.
    let result = unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_NOSIGPIPE,
            (&one as *const c_int).cast(),
            std::mem::size_of::<c_int>() as libc::socklen_t,
        )
    };
    if result == 0 { Ok(()) } else { Err(std::io::Error::last_os_error()) }
}

fn tcp_connect(host: &str, port: u16, timeout_ms: u32) -> Result<c_int, Failure> {
    let rt = runtime()?;
    let instance = current_instance()
        .filter(|instance| is_live(instance))
        .ok_or_else(|| Failure::new(HEELER_ET_ERR_NOT_RUNNING, "EasyTier is not running"))?;
    let timeout = Duration::from_millis(u64::from(timeout_ms));

    rt.block_on(async move {
        let address = resolve(&instance, host).await?;
        let destination = SocketAddr::V4(SocketAddrV4::new(address, port));
        let stream = instance
            .data_plane_tcp_connect(destination, timeout)
            .await
            .map_err(|error| {
                let code = match error.kind() {
                    DataPlaneErrorKind::DeadlineExceeded => HEELER_ET_ERR_TIMEOUT,
                    DataPlaneErrorKind::InstanceStopped => HEELER_ET_ERR_NOT_RUNNING,
                    _ => HEELER_ET_ERR,
                };
                Failure::new(code, format!("cannot connect to {destination}: {error}"))
            })?;

        let (ours, theirs) = std::os::unix::net::UnixStream::pair()?;
        for end in [&ours, &theirs] {
            end.set_nonblocking(true)?;
            set_nosigpipe(end.as_raw_fd())?;
        }
        let ours = tokio::net::UnixStream::from_std(ours)?;
        // Pumps until either side closes: the caller closing its descriptor
        // ends the copy, so no explicit release call exists.
        tokio::spawn(async move {
            let (mut overlay, mut local) = (stream, ours);
            let _ = tokio::io::copy_bidirectional(&mut overlay, &mut local).await;
        });
        Ok(theirs.into_raw_fd())
    })
}

/// One peer as `heeler_et_status_json` reports it.
#[derive(Debug, Clone, PartialEq)]
struct PeerStatus {
    peer_id: u32,
    hostname: String,
    ipv4: Option<Ipv4Addr>,
    /// Route cost 1: a connection of its own, not through another peer.
    direct: bool,
    cost: i32,
    latency_ms: Option<f64>,
}

impl PeerStatus {
    fn json(&self) -> serde_json::Value {
        serde_json::json!({
            "peer_id": self.peer_id,
            "hostname": self.hostname,
            "ipv4": self.ipv4.map(|address| address.to_string()),
            "direct": self.direct,
            "cost": self.cost,
            "latency_ms": self.latency_ms,
        })
    }
}

/// EasyTier's latency-first routing counts a hop it has not measured as
/// 500 ms, so a path latency of 500 or more is (partly) assumed.
const UNMEASURED_HOP_MS: i32 = 500;

/// A peer's latency: direct peers report their connection's (the default
/// one, else the lowest), relayed peers their route's latency-first path
/// latency unless an unmeasured hop is in it. Zero or less is unknown.
fn peer_latency_ms(direct: bool, connection_latency_us: Option<u64>, path_latency_ms: i32) -> Option<f64> {
    if direct {
        connection_latency_us.filter(|us| *us > 0).map(|us| us as f64 / 1000.0)
    } else {
        Some(path_latency_ms)
            .filter(|ms| *ms > 0 && *ms < UNMEASURED_HOP_MS)
            .map(f64::from)
    }
}

async fn peer_statuses(instance: &NativeCoreInstance, my_peer_id: u32) -> Vec<PeerStatus> {
    let routes = instance.route_snapshots().await;
    let connections = instance.peer_snapshots().await;
    let mut peers = routes
        .iter()
        .filter(|route| route.peer_id != my_peer_id)
        .map(|route| {
            let latency_us = connections
                .iter()
                .find(|peer| peer.peer_id == route.peer_id)
                .and_then(|peer| {
                    let default = peer.default_conn_id.map(|id| id.to_string());
                    let latencies = peer
                        .conns
                        .iter()
                        .filter(|conn| !conn.is_closed)
                        .filter_map(|conn| conn.stats.as_ref().map(|stats| (conn, stats.latency_us)));
                    let mut lowest = None::<u64>;
                    for (conn, latency) in latencies {
                        if default.as_deref() == Some(conn.conn_id.as_str()) {
                            return Some(latency);
                        }
                        lowest = Some(lowest.map_or(latency, |current| current.min(latency)));
                    }
                    lowest
                });
            let direct = route.cost == 1;
            PeerStatus {
                peer_id: route.peer_id,
                hostname: route.hostname.clone(),
                ipv4: route.ipv4_addr.and_then(|inet| inet.address).map(Ipv4Addr::from),
                direct,
                cost: route.cost,
                latency_ms: peer_latency_ms(
                    direct,
                    latency_us,
                    route.path_latency_latency_first.unwrap_or(route.path_latency),
                ),
            }
        })
        .collect::<Vec<_>>();
    peers.sort_by_key(|peer| peer.peer_id);
    peers
}

fn status_json() -> String {
    // Any EasyTier handle released here is released inside the runtime context.
    let _context = runtime().ok().map(Runtime::enter);
    let web = web::status();
    let current = lock(&CURRENT).as_ref().map(|running| (running.instance.clone(), running.owner));
    let Some((instance, owner)) = current else {
        return match web {
            Some(web) => serde_json::json!({ "mode": "web", "running": false, "web": web }),
            None => serde_json::json!({ "running": false }),
        }
        .to_string();
    };
    let running = is_live(&instance);
    let error = instance.latest_error();
    let (ipv4, hostname, peers) = match runtime() {
        Ok(rt) if instance.is_ready() => rt.block_on(async {
            let node = instance.node_snapshot().await;
            let peers = peer_statuses(&instance, node.peer_id).await;
            (node.ipv4_addr.map(|inet| inet.address().to_string()), node.hostname, peers)
        }),
        _ => (None, String::new(), Vec::new()),
    };
    let mut json = serde_json::json!({
        "mode": match owner { Owner::Manual => "manual", Owner::Web => "web" },
        "running": running,
        "ipv4": ipv4,
        "hostname": hostname,
        "peer_count": peers.len(),
        "peers": peers.iter().map(PeerStatus::json).collect::<Vec<_>>(),
        "error": error,
    });
    if let Some(web) = web {
        json["web"] = web;
    }
    json.to_string()
}

/// Starts the process's EasyTier network from `toml` under the outbound-only
/// policy, waiting at most `timeout_ms` for EasyTier to start (joining the
/// network and getting an address happen afterwards; poll the status).
/// Starting the configuration that is already running is a no-op; a different
/// one stops the running network first. A configuration that does not parse or
/// breaks the policy leaves the running network as it was; any later failure
/// leaves none.
///
/// # Safety
/// `toml` is a NUL-terminated string; `err` is null or `errlen` writable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn heeler_et_start(
    toml: *const c_char,
    timeout_ms: u32,
    err: *mut c_char,
    errlen: usize,
) -> c_int {
    ffi_call(err, errlen, || {
        // SAFETY: forwarded from the caller.
        let toml = unsafe { borrowed_str(toml, "configuration") }?;
        start(toml, timeout_ms)
    })
}

/// Stops the running network, if any. Dialled streams fail afterwards.
#[unsafe(no_mangle)]
pub extern "C" fn heeler_et_stop() {
    let _ = catch_unwind(stop);
}

/// Starts (or keeps) the config-server session: connects to `url`
/// (`udp://` or `tcp://host:port/<token>`, also `ws://` and `wss://` with a
/// verified certificate) as the device `machine_id` (a UUID the caller keeps)
/// named `hostname`, and runs the one network the server assigns under the
/// outbound-only policy. With `secure_mode` non-zero the session only runs
/// over EasyTier's encrypted web tunnel and a server that does not offer it
/// is never used; with zero it upgrades when the server offers it and runs in
/// clear text otherwise. Returns at once; connecting and joining happen in
/// the background (poll the status). The same session is a no-op; anything
/// else running — another session or a heeler_et_start network — is stopped
/// first.
///
/// # Safety
/// `url`, `machine_id` and `hostname` are NUL-terminated strings; `err` is null
/// or `errlen` writable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn heeler_et_web_start(
    url: *const c_char,
    machine_id: *const c_char,
    hostname: *const c_char,
    secure_mode: c_int,
    err: *mut c_char,
    errlen: usize,
) -> c_int {
    ffi_call(err, errlen, || {
        // SAFETY: forwarded from the caller.
        let url = unsafe { borrowed_str(url, "config server URL") }?;
        // SAFETY: forwarded from the caller.
        let machine_id = unsafe { borrowed_str(machine_id, "machine ID") }?;
        // SAFETY: forwarded from the caller.
        let hostname = unsafe { borrowed_str(hostname, "hostname") }?;
        web::start(url, machine_id, hostname, secure_mode != 0)
    })
}

/// Ends the config-server session and stops its network, if any.
#[unsafe(no_mangle)]
pub extern "C" fn heeler_et_web_stop() {
    let _ = catch_unwind(web::stop);
}

/// Connects to `host:port` through the overlay and returns one end of a
/// connected, non-blocking AF_UNIX socketpair with SO_NOSIGPIPE set. The caller
/// owns the descriptor; closing it ends the stream. `host` is an IPv4 literal
/// or a peer's EasyTier hostname. Returns a negative HEELER_ET_ERR_* code on
/// failure.
///
/// # Safety
/// `host` is a NUL-terminated string; `err` is null or `errlen` writable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn heeler_et_tcp_connect_fd(
    host: *const c_char,
    port: u16,
    timeout_ms: u32,
    err: *mut c_char,
    errlen: usize,
) -> c_int {
    ffi_call(err, errlen, || {
        // SAFETY: forwarded from the caller.
        let host = unsafe { borrowed_str(host, "host") }?;
        tcp_connect(host, port, timeout_ms)
    })
}

/// Writes the node status as a NUL-terminated JSON object
/// `{"running":bool,"ipv4":string|null,"hostname":string,"peer_count":int,"peers":[...],"error":string|null}`
/// (only `running` when no network exists). Each peer is
/// `{"peer_id":int,"hostname":string,"ipv4":string|null,"direct":bool,"cost":int,"latency_ms":number|null}`. Returns the JSON length excluding
/// the terminator; a result `>= len` means the buffer was too small and the
/// output was not written. Returns -1 on failure.
///
/// # Safety
/// `buf` is null or `len` writable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn heeler_et_status_json(buf: *mut c_char, len: usize) -> c_int {
    let Ok(json) = catch_unwind(status_json) else { return HEELER_ET_ERR };
    let Ok(length) = c_int::try_from(json.len()) else { return HEELER_ET_ERR };
    if json.len() < len {
        write_c_string(&json, buf, len);
    }
    length
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hostname_matching_ignores_case_and_magic_dns_zone() {
        assert!(hostname_matches("Build-Box", "build-box"));
        assert!(hostname_matches("build-box", "build-box.et.net"));
        assert!(hostname_matches("build-box", "build-box.et.net."));
        assert!(hostname_matches("build-box", "BUILD-BOX.Et.Net"));
        assert!(!hostname_matches("et.net", ".et.net"));
        assert!(!hostname_matches("build-box", "build"));
        assert!(!hostname_matches("", ".et.net"));
    }

    #[test]
    fn hostnames_resolve_to_exactly_one_peer() {
        let a = Ipv4Addr::new(10, 0, 0, 2);
        let b = Ipv4Addr::new(10, 0, 0, 3);
        let peers = vec![
            ("box".to_owned(), Some(a)),
            ("twin".to_owned(), Some(a)),
            ("Twin".to_owned(), Some(b)),
            ("no-address".to_owned(), None),
        ];
        assert_eq!(resolve_hostname("BOX.et.net", &peers).ok(), Some(a));
        let ambiguous = resolve_hostname("twin", &peers).err().map(|f| (f.code, f.message));
        assert_eq!(
            ambiguous,
            Some((HEELER_ET_ERR_UNRESOLVED, "\"twin\" names 2 EasyTier peers (10.0.0.2, 10.0.0.3); dial one by its address".to_owned()))
        );
        assert_eq!(resolve_hostname("no-address", &peers).err().map(|f| f.code), Some(HEELER_ET_ERR_UNRESOLVED));
        assert_eq!(resolve_hostname("ghost", &peers).err().map(|f| f.code), Some(HEELER_ET_ERR_UNRESOLVED));
    }

    #[test]
    fn the_outbound_only_policy_forces_flags_and_refuses_inbound_features() {
        let base = "[network_identity]\nnetwork_name = \"n\"\nnetwork_secret = \"s\"\n";
        let config = outbound_only_config(&format!("listeners = []\n{base}[flags]\nenable_exit_node = true\nprivate_mode = false\n"))
            .expect("valid");
        let flags = config.get_flags();
        assert!(flags.no_tun && !flags.enable_exit_node && flags.disable_relay_data && flags.private_mode);
        assert_eq!(flags.relay_network_whitelist, "");

        let provider = outbound_only_config(&format!(
            "ipv6_public_addr_provider = true\nipv6_public_addr_auto = true\n\
             ipv6_public_addr_prefix = \"2001:db8::/64\"\n{base}[flags]\n\
             relay_all_peer_rpc = true\nenable_relay_foreign_network_kcp = true\n\
             enable_relay_foreign_network_quic = true\nenable_kcp_proxy = true\n\
             enable_quic_proxy = true\ndisable_kcp_input = false\ndisable_quic_input = false\n\
             proxy_forward_by_system = true\nenable_udp_broadcast_relay = true\n\
             accept_dns = true\ndisable_upnp = false\n"
        ))
        .expect("valid");
        let flags = provider.get_flags();
        assert!(
            !flags.relay_all_peer_rpc
                && !flags.enable_relay_foreign_network_kcp
                && !flags.enable_relay_foreign_network_quic
                && !flags.enable_kcp_proxy
                && !flags.enable_quic_proxy
                && flags.disable_kcp_input
                && flags.disable_quic_input
                && !flags.proxy_forward_by_system
                && !flags.enable_udp_broadcast_relay
                && !flags.accept_dns
                && flags.disable_upnp,
            "{flags:?}"
        );
        assert!(!provider.get_ipv6_public_addr_provider() && !provider.get_ipv6_public_addr_auto());
        assert!(provider.get_ipv6_public_addr_prefix().is_none());

        let refused = [
            format!("listeners = [\"tcp://0.0.0.0:11010\"]\n{base}"),
            format!("exit_nodes = [\"10.0.0.1\"]\n{base}"),
            format!("socks5_proxy = \"socks5://127.0.0.1:1080\"\n{base}"),
            format!("{base}[[proxy_network]]\ncidr = \"192.168.1.0/24\"\n"),
            format!("{base}[[port_forward]]\nbind_addr = \"0.0.0.0:2222\"\ndst_addr = \"10.0.0.2:22\"\nproto = \"tcp\"\n"),
            "[network_identity]\nnetwork_name = \"n\"\nnetwork_secret = \"\"\n".to_owned(),
            "[network_identity]\nnetwork_name = \"n\"\n".to_owned(),
        ];
        for toml in refused {
            assert!(outbound_only_config(&toml).is_err(), "accepted:\n{toml}");
        }
    }

    #[test]
    fn a_static_ipv4_replaces_dhcp_and_bad_ones_are_refused() {
        let base = "[network_identity]\nnetwork_name = \"n\"\nnetwork_secret = \"s\"\n";
        let config = outbound_only_config(&format!("ipv4 = \"10.144.144.7/24\"\ndhcp = true\n{base}"))
            .expect("valid");
        assert!(!config.get_dhcp());
        assert_eq!(config.get_ipv4().map(|inet| inet.to_string()), Some("10.144.144.7/24".to_owned()));

        let dhcp = outbound_only_config(&format!("dhcp = true\n{base}")).expect("valid");
        assert!(dhcp.get_dhcp() && dhcp.get_ipv4().is_none());

        let refused = [
            format!("ipv4 = \"10.144.144.7/33\"\ndhcp = false\n{base}"),
            format!("ipv4 = \"not an address\"\ndhcp = false\n{base}"),
            format!("ipv4 = \"0.0.0.0/8\"\n{base}"),
            format!("ipv4 = \"127.0.0.2/8\"\n{base}"),
            format!("ipv4 = \"224.0.0.1/4\"\n{base}"),
        ];
        for toml in refused {
            assert!(outbound_only_config(&toml).is_err(), "accepted:\n{toml}");
        }
        assert!(declares_top_level_key("a = 1\n  ipv4= \"x\"\n[t]\n", "ipv4"));
        assert!(!declares_top_level_key("ipv4_x = 1\n[t]\nipv4 = \"x\"\n", "ipv4"));
        assert!(check_static_ipv4(Ipv4Addr::new(10, 0, 0, 1), 0).is_err());
        assert!(check_static_ipv4(Ipv4Addr::new(10, 0, 0, 1), 32).is_ok());
    }

    #[test]
    fn peer_latency_comes_from_the_connection_or_the_route() {
        assert_eq!(peer_latency_ms(true, Some(1500), 9), Some(1.5));
        assert_eq!(peer_latency_ms(true, Some(0), 9), None);
        assert_eq!(peer_latency_ms(true, None, 9), None);
        assert_eq!(peer_latency_ms(false, Some(1500), 12), Some(12.0));
        assert_eq!(peer_latency_ms(false, None, 0), None);
        assert_eq!(peer_latency_ms(false, None, 501), None);
    }

    #[test]
    fn peer_status_json_has_the_documented_fields() {
        let peer = PeerStatus {
            peer_id: 7,
            hostname: "peer-b".to_owned(),
            ipv4: Some(Ipv4Addr::new(10, 144, 144, 2)),
            direct: true,
            cost: 1,
            latency_ms: Some(0.5),
        };
        assert_eq!(
            peer.json().to_string(),
            r#"{"cost":1,"direct":true,"hostname":"peer-b","ipv4":"10.144.144.2","latency_ms":0.5,"peer_id":7}"#
        );
    }

    #[test]
    fn c_strings_truncate_on_character_boundaries() {
        let mut buf = [0x7f as c_char; 5];
        write_c_string("héllo", buf.as_mut_ptr(), buf.len());
        let written = unsafe { CStr::from_ptr(buf.as_ptr()) };
        assert_eq!(written.to_str().ok(), Some("hél"));
    }

    #[test]
    fn calls_without_a_network_fail_cleanly() {
        let mut err = [0 as c_char; 128];
        let host = c"10.0.0.1";
        let fd = unsafe { heeler_et_tcp_connect_fd(host.as_ptr(), 22, 100, err.as_mut_ptr(), err.len()) };
        assert_eq!(fd, HEELER_ET_ERR_NOT_RUNNING);
        let message = unsafe { CStr::from_ptr(err.as_ptr()) };
        assert_eq!(message.to_str().ok(), Some("EasyTier is not running"));

        let mut buf = [0 as c_char; 64];
        let n = unsafe { heeler_et_status_json(buf.as_mut_ptr(), buf.len()) };
        assert!(n > 0 && (n as usize) < buf.len());
        let json = unsafe { CStr::from_ptr(buf.as_ptr()) };
        assert_eq!(json.to_str().ok(), Some(r#"{"running":false}"#));
        assert_eq!(unsafe { heeler_et_status_json(buf.as_mut_ptr(), 4) }, n);

        let rc = unsafe { heeler_et_start(c"not = [valid".as_ptr(), 1000, err.as_mut_ptr(), err.len()) };
        assert_eq!(rc, HEELER_ET_ERR);
        heeler_et_stop();
    }
}
