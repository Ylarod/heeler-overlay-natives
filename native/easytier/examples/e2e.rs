// SPDX-License-Identifier: LGPL-3.0-or-later
//
// macOS end-to-end check of the C ABI. Node B runs in this process through the
// EasyTier API (static 10.144.144.2, hostname "peer-b", listener on
// 127.0.0.1:21010, and a wss:// listener with EasyTier's default self-signed
// certificate on 127.0.0.1:21011) and serves a fake SSH banner plus echo on
// overlay port 22. Node A is driven only through the heeler_et_* functions,
// exactly as the app configures it: first with DHCP, then replaced by a fixed
// 10.144.144.7/24, and finally through a wss:// peer, whose certificate is not
// verified (peers authenticate with the network secret; only config-server
// connections verify certificates).
//
//   cargo run --release --example e2e

use std::{
    ffi::{CStr, CString, c_char, c_int},
    io::{Read, Write},
    os::fd::FromRawFd,
    time::{Duration, Instant},
};

use easytier::common::config::TomlConfigLoader;
use easytier::instance::factory::create_native_instance;
use heeler_easytier::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const BANNER: &[u8] = b"SSH-2.0-heeler-e2e\r\n";
const KEY: &std::ffi::CStr = c"e2e";

fn node_a_toml(instance_name: &str, ipv4: Option<&str>) -> CString {
    node_a_toml_via(instance_name, ipv4, "tcp://127.0.0.1:21010")
}

fn node_a_toml_via(instance_name: &str, ipv4: Option<&str>, peer: &str) -> CString {
    let addressing = match ipv4 {
        Some(ipv4) => format!("ipv4 = \"{ipv4}\"\ndhcp = false"),
        None => "dhcp = true".to_owned(),
    };
    CString::new(format!(
        r#"instance_name = "{instance_name}"
hostname = "heeler-a"
{addressing}
listeners = []
[network_identity]
network_name = "heeler-e2e"
network_secret = "s3cret"
[[peer]]
uri = "{peer}"
[flags]
no_tun = true
"#
    ))
    .expect("toml")
}

fn status() -> String {
    let mut buf = vec![0 as c_char; 4096];
    let n = unsafe { heeler_et_status_json(KEY.as_ptr(), buf.as_mut_ptr(), buf.len()) };
    assert!(n >= 0 && (n as usize) < buf.len(), "status_json returned {n}");
    unsafe { CStr::from_ptr(buf.as_ptr()) }.to_string_lossy().into_owned()
}

fn start(toml: &CString) -> (c_int, String) {
    let mut err = vec![0 as c_char; 512];
    let rc = unsafe { heeler_et_start(KEY.as_ptr(), toml.as_ptr(), 10_000, err.as_mut_ptr(), err.len()) };
    (rc, unsafe { CStr::from_ptr(err.as_ptr()) }.to_string_lossy().into_owned())
}

fn connect(host: &str, port: u16, timeout_ms: u32) -> (c_int, String) {
    let host = CString::new(host).expect("host");
    let mut err = vec![0 as c_char; 512];
    let fd = unsafe { heeler_et_tcp_connect_fd(KEY.as_ptr(), std::ptr::null(), host.as_ptr(), port, timeout_ms, err.as_mut_ptr(), err.len()) };
    (fd, unsafe { CStr::from_ptr(err.as_ptr()) }.to_string_lossy().into_owned())
}

fn wait_for_ipv4(t0: Instant) -> serde_json::Value {
    for _ in 0..120 {
        let s = status();
        let parsed: serde_json::Value = serde_json::from_str(&s).expect("status JSON");
        if parsed["ipv4"].as_str().is_some_and(|ipv4| ipv4.starts_with("10.")) {
            eprintln!("[{:?}] online: {s}", t0.elapsed());
            return parsed;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    panic!("no virtual IPv4: {}", status());
}

/// Waits until peer-b shows up in the status as a direct peer at
/// 10.144.144.2, and returns that status.
fn wait_for_peer_b() -> serde_json::Value {
    for _ in 0..120 {
        let status: serde_json::Value = serde_json::from_str(&status()).expect("status JSON");
        let found = status["peers"].as_array().is_some_and(|peers| {
            peers.iter().any(|peer| {
                peer["hostname"] == "peer-b"
                    && peer["ipv4"] == "10.144.144.2"
                    && peer["direct"] == true
                    && peer["cost"] == 1
                    && peer["peer_id"].as_u64().is_some()
            })
        });
        if found {
            assert_eq!(status["peer_count"], 1, "{status}");
            return status;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    panic!("peer-b never appeared among the peers: {}", status());
}

/// Reads the banner, then checks an echo round trip, on a dialled descriptor.
fn exercise(fd: c_int) {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    assert!(flags & libc::O_NONBLOCK != 0, "descriptor must be non-blocking");
    let mut value: c_int = 0;
    let mut size = std::mem::size_of::<c_int>() as libc::socklen_t;
    let rc = unsafe {
        libc::getsockopt(fd, libc::SOL_SOCKET, libc::SO_NOSIGPIPE, (&mut value as *mut c_int).cast(), &mut size)
    };
    assert!(rc == 0 && value != 0, "SO_NOSIGPIPE must be set");

    let mut stream = unsafe { std::os::unix::net::UnixStream::from_raw_fd(fd) };
    stream.set_nonblocking(false).expect("blocking");
    stream.set_read_timeout(Some(Duration::from_secs(5))).expect("timeout");
    let mut banner = vec![0u8; BANNER.len()];
    stream.read_exact(&mut banner).expect("banner");
    assert_eq!(banner, BANNER);
    let payload = vec![0x5au8; 256 * 1024];
    stream.write_all(&payload).expect("write");
    let mut echoed = vec![0u8; payload.len()];
    stream.read_exact(&mut echoed).expect("echo");
    assert_eq!(echoed, payload);
}

fn main() {
    let rt = tokio::runtime::Runtime::new().expect("runtime");
    let b = rt.block_on(async {
        let cfg = TomlConfigLoader::new_from_str(
            r#"
instance_name = "b"
hostname = "peer-b"
ipv4 = "10.144.144.2/24"
listeners = ["tcp://127.0.0.1:21010", "wss://127.0.0.1:21011"]
[network_identity]
network_name = "heeler-e2e"
network_secret = "s3cret"
[flags]
no_tun = true
"#,
        )
        .expect("config B");
        let b = create_native_instance(cfg).expect("create B");
        b.start().await.expect("start B");
        let mut listener = b.data_plane_tcp_bind(22, Duration::from_secs(5)).await.expect("bind B:22");
        tokio::spawn(async move {
            while let Ok((mut s, peer)) = listener.accept().await {
                eprintln!("B accepted {peer}");
                tokio::spawn(async move {
                    if s.write_all(BANNER).await.is_err() {
                        return;
                    }
                    let mut buf = vec![0u8; 16 * 1024];
                    while let Ok(n) = s.read(&mut buf).await {
                        if n == 0 || s.write_all(&buf[..n]).await.is_err() {
                            break;
                        }
                    }
                });
            }
        });
        b
    });

    let t0 = Instant::now();
    eprintln!("status before start: {}", status());
    let toml = node_a_toml("heeler-a", None);
    let (rc, err) = start(&toml);
    assert_eq!(rc, 0, "start A: {err}");
    eprintln!("[{:?}] start returned", t0.elapsed());
    wait_for_ipv4(t0);
    let peers = wait_for_peer_b();
    eprintln!("peers: {}", peers["peers"]);

    let t1 = Instant::now();
    assert_eq!(start(&toml).0, 0);
    eprintln!("idempotent start took {:?}", t1.elapsed());

    let mut fd = -1;
    for attempt in 0..20 {
        let (result, err) = connect("10.144.144.2", 22, 3000);
        if result >= 0 {
            eprintln!("[{:?}] dial by IPv4 ok after {attempt} retries", t0.elapsed());
            fd = result;
            break;
        }
        eprintln!("dial attempt {attempt}: {result} {err}");
        std::thread::sleep(Duration::from_millis(500));
    }
    assert!(fd >= 0, "dial by IPv4 failed");
    exercise(fd);
    eprintln!("banner + 256 KiB echo over IPv4 dial ok");

    let (rc, err) = start(&CString::new("not = [valid").expect("toml"));
    assert_eq!(rc, HEELER_ET_ERR);
    let kept: serde_json::Value = serde_json::from_str(&status()).expect("status JSON");
    assert!(kept["ipv4"].as_str().is_some_and(|ipv4| ipv4.starts_with("10.")), "a bad config must keep the network: {err}");
    eprintln!("unparsable config rejected, network kept");

    let (fd, err) = connect("PEER-B.et.net", 22, 3000);
    assert!(fd >= 0, "dial by hostname failed: {err}");
    exercise(fd);
    eprintln!("dial by hostname ok");

    let (rc, err) = connect("nobody", 22, 1000);
    assert_eq!(rc, HEELER_ET_ERR_UNRESOLVED);
    eprintln!("unknown hostname -> {rc}: {err}");

    let t2 = Instant::now();
    let (rc, err) = connect("10.144.144.2", 2222, 2000);
    assert!(rc < 0);
    eprintln!("closed port -> {rc} after {:?}: {err}", t2.elapsed());

    let t3 = Instant::now();
    let (rc, err) = connect("10.144.144.99", 22, 1500);
    assert!(rc < 0);
    eprintln!("absent address -> {rc} after {:?}: {err}", t3.elapsed());

    let t4 = Instant::now();
    let (rc, err) = start(&node_a_toml("heeler-a2", Some("10.144.144.250/33")));
    assert_eq!(rc, HEELER_ET_ERR, "a bad static address must be refused");
    eprintln!("bad static ipv4 refused: {err}");
    let (rc, err) = start(&node_a_toml("heeler-a2", Some("10.144.144.7/24")));
    assert_eq!(rc, 0, "replacement start: {err}");
    let online = wait_for_ipv4(t4);
    assert_eq!(online["ipv4"], "10.144.144.7", "the static address must be used: {online}");
    wait_for_peer_b();
    eprintln!("static ipv4 10.144.144.7 in use, peer-b listed");
    let mut fd = -1;
    for _ in 0..20 {
        let (result, _) = connect("10.144.144.2", 22, 3000);
        if result >= 0 {
            fd = result;
            break;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    assert!(fd >= 0, "dial after replacement failed");
    exercise(fd);
    eprintln!("[{:?}] replaced network and dialled again", t4.elapsed());

    let t5 = Instant::now();
    let (rc, err) = start(&node_a_toml_via("heeler-a3", Some("10.144.144.8/24"), "wss://127.0.0.1:21011"));
    assert_eq!(rc, 0, "wss:// peer start: {err}");
    wait_for_ipv4(t5);
    wait_for_peer_b();
    let mut fd = -1;
    for _ in 0..20 {
        let (result, _) = connect("10.144.144.2", 22, 3000);
        if result >= 0 {
            fd = result;
            break;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    assert!(fd >= 0, "dial through the wss:// peer failed");
    exercise(fd);
    eprintln!("[{:?}] wss:// peer with a self-signed certificate connected and dialled", t5.elapsed());

    unsafe { heeler_et_stop(KEY.as_ptr()) };
    eprintln!("status after stop: {}", status());
    let (rc, err) = connect("10.144.144.2", 22, 1000);
    assert_eq!(rc, HEELER_ET_ERR_NOT_RUNNING, "{err}");
    unsafe { heeler_et_stop(KEY.as_ptr()) };
    rt.block_on(b.stop());
    println!("E2E OK");
}
