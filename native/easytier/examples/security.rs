// SPDX-License-Identifier: LGPL-3.0-or-later
//
// Regression check: a Heeler node only dials out. Node A runs through the C ABI
// with the configuration EasyTierTOML renders in the app. Node B is a hostile
// peer on the same network that names A as its exit node.
//
//   1. B cannot reach, through A, a host port that listens only on the Mac's
//      LAN address (exit-node proxying, TCP and UDP).
//   2. B cannot reach a port listening on A's 127.0.0.1 through A's virtual
//      IP (no-TUN local virtual IP forwarding, TCP and UDP).
//   3. A still dials B.
//   4. With A's relay and private-mode flags, A still reaches a peer through
//      a shared relay node that belongs to another network, as with public
//      EasyTier servers.
//
//   cargo run --release --example security

use std::{
    ffi::{CStr, CString, c_char},
    io::Read,
    net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, UdpSocket},
    os::fd::FromRawFd,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use easytier::common::config::TomlConfigLoader;
use easytier::instance::factory::{NativeCoreInstance, create_native_instance};
use heeler_easytier::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const BANNER: &[u8] = b"SSH-2.0-heeler-security\r\n";

/// Mirrors EasyTierTOML.render in EasyTierNode.swift.
fn heeler_toml(network: &str, peer: &str) -> CString {
    CString::new(format!(
        r#"instance_name = "heeler"
hostname = "heeler-a"
dhcp = true
listeners = []

[network_identity]
network_name = "{network}"
network_secret = "s3cret"

[[peer]]
uri = "{peer}"

[flags]
no_tun = true
{flags}"#,
        flags = std::env::var("HEELER_SEC_FLAGS").unwrap_or_else(|_| "disable_relay_data = true\nrelay_network_whitelist = \"\"\nprivate_mode = true\n".to_owned()).replace("\\n", "\n")
    ))
    .expect("toml")
}

fn status() -> String {
    let mut buf = vec![0 as c_char; 512];
    let n = unsafe { heeler_et_status_json(buf.as_mut_ptr(), buf.len()) };
    assert!(n >= 0 && (n as usize) < buf.len());
    unsafe { CStr::from_ptr(buf.as_ptr()) }.to_string_lossy().into_owned()
}

fn start_a(toml: &CString) {
    let mut err = vec![0 as c_char; 512];
    let rc = unsafe { heeler_et_start(toml.as_ptr(), 10_000, err.as_mut_ptr(), err.len()) };
    let message = unsafe { CStr::from_ptr(err.as_ptr()) }.to_string_lossy().into_owned();
    assert_eq!(rc, 0, "start A: {message}");
}

fn wait_for_ipv4(expected: &str) {
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(30) {
        let s = status();
        if s.contains(&format!(r#""ipv4":"{expected}""#)) {
            eprintln!("A online after {:?}: {s}", t0.elapsed());
            return;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    panic!("A never got {expected}: {}", status());
}

fn a_dials(host: &str) {
    let host_c = CString::new(host).expect("host");
    let mut err = vec![0 as c_char; 512];
    for _ in 0..20 {
        let fd = unsafe { heeler_et_tcp_connect_fd(host_c.as_ptr(), 22, 3000, err.as_mut_ptr(), err.len()) };
        if fd >= 0 {
            let mut stream = unsafe { std::os::unix::net::UnixStream::from_raw_fd(fd) };
            stream.set_nonblocking(false).expect("blocking");
            stream.set_read_timeout(Some(Duration::from_secs(5))).expect("timeout");
            let mut banner = vec![0u8; BANNER.len()];
            stream.read_exact(&mut banner).expect("banner");
            assert_eq!(banner, BANNER);
            eprintln!("PASS A dials {host}:22 and reads the banner");
            return;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    panic!("A could not dial {host}: {}", unsafe { CStr::from_ptr(err.as_ptr()) }.to_string_lossy());
}

async fn node(toml: &str) -> Arc<NativeCoreInstance> {
    let config = TomlConfigLoader::new_from_str(toml).expect("config");
    let node = create_native_instance(config).expect("create");
    node.start().await.expect("start");
    node
}

async fn serve_banner(node: &NativeCoreInstance) {
    let mut listener = node.data_plane_tcp_bind(22, Duration::from_secs(5)).await.expect("bind 22");
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                // Hold the stream until the dialler closes it; dropping it
                // right after the write can discard the unsent banner.
                if stream.write_all(BANNER).await.is_ok() {
                    let mut sink = [0u8; 256];
                    while matches!(stream.read(&mut sink).await, Ok(n) if n > 0) {}
                }
            });
        }
    });
}

/// A TCP listener on the host that counts every accepted connection.
fn counting_tcp_listener(ip: IpAddr) -> (SocketAddr, Arc<AtomicUsize>) {
    let listener = TcpListener::bind(SocketAddr::new(ip, 0)).expect("tcp listener");
    let address = listener.local_addr().expect("address");
    let accepted = Arc::new(AtomicUsize::new(0));
    let counter = accepted.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            if stream.is_ok() {
                counter.fetch_add(1, Ordering::SeqCst);
            }
        }
    });
    (address, accepted)
}

/// A UDP socket on the host that counts every datagram it receives.
fn counting_udp_socket(ip: IpAddr) -> (SocketAddr, Arc<AtomicUsize>) {
    let socket = UdpSocket::bind(SocketAddr::new(ip, 0)).expect("udp socket");
    let address = socket.local_addr().expect("address");
    let received = Arc::new(AtomicUsize::new(0));
    let counter = received.clone();
    std::thread::spawn(move || {
        let mut buf = [0u8; 2048];
        while socket.recv_from(&mut buf).is_ok() {
            counter.fetch_add(1, Ordering::SeqCst);
        }
    });
    (address, received)
}

/// The Mac's own LAN address (the source of its default route); no packet is
/// sent to determine it.
fn lan_ipv4() -> Ipv4Addr {
    let probe = UdpSocket::bind("0.0.0.0:0").expect("probe");
    probe.connect("192.0.2.1:9").expect("route");
    match probe.local_addr().expect("local").ip() {
        IpAddr::V4(ip) if !ip.is_loopback() && !ip.is_unspecified() => ip,
        other => panic!("no LAN IPv4 address for the exit-node check: {other}"),
    }
}

/// B must not reach `tcp`/`udp` through A at `via`/`via_udp`. With
/// HEELER_SEC_EXPECT_OPEN=1 the check inverts: run against an unpatched tree
/// with `--features packet-proxy` to prove each path is really exercised.
#[allow(clippy::too_many_arguments)]
async fn b_cannot_reach(b: &NativeCoreInstance, label: &str, tcp: SocketAddr, tcp_hits: &AtomicUsize, udp: SocketAddr, udp_hits: &AtomicUsize, via: SocketAddr, via_udp: SocketAddr) {
    let expect_open = std::env::var("HEELER_SEC_EXPECT_OPEN").is_ok_and(|v| v == "1");
    let t0 = Instant::now();
    let result = b.data_plane_tcp_connect(via, Duration::from_secs(3)).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let tcp_open = result.is_ok() || tcp_hits.load(Ordering::SeqCst) > 0;
    let detail = match &result {
        Ok(_) => "connected".to_owned(),
        Err(error) => error.to_string(),
    };
    assert_eq!(tcp_open, expect_open, "{label}: TCP {via} -> {tcp}: {detail}");
    eprintln!("PASS {label} TCP {via} open={tcp_open} after {:?}: {detail}", t0.elapsed());

    let socket = b.data_plane_udp_bind(0, Duration::from_secs(3)).await.expect("udp bind");
    for _ in 0..5 {
        let _ = socket.send_to(b"probe", via_udp).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    tokio::time::sleep(Duration::from_millis(800)).await;
    let udp_open = udp_hits.load(Ordering::SeqCst) > 0;
    assert_eq!(udp_open, expect_open, "{label}: UDP {via_udp} -> {udp}");
    eprintln!("PASS {label} UDP {via_udp} open={udp_open}");
}

fn main() {
    let rt = tokio::runtime::Runtime::new().expect("runtime");

    // --- 1-3: hostile peer B names A as its exit node ---------------------
    let b = rt.block_on(async {
        let b = node(
            r#"
instance_name = "hostile-b"
hostname = "hostile-b"
ipv4 = "10.144.145.2/24"
listeners = ["tcp://127.0.0.1:21030"]
exit_nodes = ["10.144.145.1"]
[network_identity]
network_name = "heeler-security"
network_secret = "s3cret"
[flags]
no_tun = true
"#,
        )
        .await;
        serve_banner(&b).await;
        b
    });
    start_a(&heeler_toml("heeler-security", "tcp://127.0.0.1:21030"));
    wait_for_ipv4("10.144.145.1");
    a_dials("10.144.145.2");

    let lan = lan_ipv4();
    let (lan_tcp, lan_tcp_hits) = counting_tcp_listener(IpAddr::V4(lan));
    let (lan_udp, lan_udp_hits) = counting_udp_socket(IpAddr::V4(lan));
    let (local_tcp, local_tcp_hits) = counting_tcp_listener(IpAddr::V4(Ipv4Addr::LOCALHOST));
    let (local_udp, local_udp_hits) = counting_udp_socket(IpAddr::V4(Ipv4Addr::LOCALHOST));
    let a_virtual = IpAddr::V4(Ipv4Addr::new(10, 144, 145, 1));

    rt.block_on(async {
        // Exit-node proxying to a LAN-only port of the Mac running A.
        b_cannot_reach(&b, "exit node", lan_tcp, &lan_tcp_hits, lan_udp, &lan_udp_hits, lan_tcp, lan_udp).await;
        // A's own loopback services through A's virtual IP.
        b_cannot_reach(
            &b,
            "virtual IP to loopback",
            local_tcp,
            &local_tcp_hits,
            local_udp,
            &local_udp_hits,
            SocketAddr::new(a_virtual, local_tcp.port()),
            SocketAddr::new(a_virtual, local_udp.port()),
        )
        .await;
    });
    a_dials("10.144.145.2");
    heeler_et_stop();
    rt.block_on(b.stop());

    // --- 4: through a shared relay of another network ----------------------
    let (relay, peer) = rt.block_on(async {
        let relay = node(
            r#"
instance_name = "shared-relay"
listeners = ["tcp://127.0.0.1:21031"]
[network_identity]
network_name = "shared-relay"
network_secret = "relay-secret"
[flags]
no_tun = true
"#,
        )
        .await;
        let peer = node(
            r#"
instance_name = "relayed-peer"
hostname = "relayed-peer"
ipv4 = "10.144.146.2/24"
listeners = []
[network_identity]
network_name = "heeler-relayed"
network_secret = "s3cret"
[[peer]]
uri = "tcp://127.0.0.1:21031"
[flags]
no_tun = true
"#,
        )
        .await;
        serve_banner(&peer).await;
        (relay, peer)
    });
    start_a(&heeler_toml("heeler-relayed", "tcp://127.0.0.1:21031"));
    wait_for_ipv4("10.144.146.1");
    a_dials("relayed-peer");
    heeler_et_stop();
    rt.block_on(async {
        peer.stop().await;
        relay.stop().await;
    });
    println!("SECURITY OK");
}
