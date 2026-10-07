// SPDX-License-Identifier: LGPL-3.0-or-later
//
// macOS end-to-end check of several EasyTier networks in one process, driven
// only through the heeler_et_* functions as the app drives them.
//
// Peers run in this process through the EasyTier API, one per network, each
// serving its own banner on overlay port 22 and counting what it accepts.
//
// 1. Two manual networks on different subnets (keys "net-a", "net-b"): each
//    key reaches its own peer by address and hostname; the other network's
//    peer is neither resolvable nor reachable through it, naming the wrong
//    network fails, and a peer of one network cannot reach the other
//    network through this device, which is on both.
// 2. Two manual networks on the same subnet with the same addresses
//    (keys "net-a", "net-c": this device is 10.144.144.10 and the peer
//    10.144.144.2 on both): every dial reaches the peer of the key's own
//    network, never the other one.
// 3. Keys stop, restart and change independently; stop_all ends them all.
// 4. With EASYTIER_WEB set (see examples/webconfig.rs), one config-server
//    session runs two networks the console assigns, beside a manual key:
//    dials select the network by exact peer address, own subnet and
//    hostname, or by an explicit network name; a third network on the same
//    subnet makes an unnamed dial ambiguous; deleting one network in the
//    console leaves the others running.
//
//   cargo run --release --example multi
//   EASYTIER_WEB=/path/to/easytier-web cargo run --release --example multi

use std::{
    ffi::{CStr, CString, c_char, c_int},
    io::{Read, Write},
    net::TcpStream,
    os::fd::FromRawFd,
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use easytier::common::config::TomlConfigLoader;
use easytier::instance::factory::{NativeCoreInstance, create_native_instance};
use heeler_easytier::*;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const MACHINE_ID: &str = "7b2f0e44-6c1e-4f43-9b7f-0123456789cd";
/// md5("user"): the console sends the MD5 of the password.
const USER_PASSWORD: &str = "ee11cbb19052e40b07aac0ca060c23ee";

fn banner(name: &str) -> Vec<u8> {
    format!("SSH-2.0-heeler-multi-{name}\r\n").into_bytes()
}

// --- the C ABI -------------------------------------------------------------

fn c(text: &str) -> CString {
    CString::new(text).expect("C string")
}

fn status(key: &str) -> Value {
    let key = c(key);
    let mut buf = vec![0 as c_char; 65536];
    let n = unsafe { heeler_et_status_json(key.as_ptr(), buf.as_mut_ptr(), buf.len()) };
    assert!(n >= 0 && (n as usize) < buf.len(), "status_json returned {n}");
    serde_json::from_str(&unsafe { CStr::from_ptr(buf.as_ptr()) }.to_string_lossy()).expect("status JSON")
}

fn start(key: &str, toml: &str) -> (c_int, String) {
    let (key, toml) = (c(key), c(toml));
    let mut err = vec![0 as c_char; 512];
    let rc = unsafe { heeler_et_start(key.as_ptr(), toml.as_ptr(), 10_000, err.as_mut_ptr(), err.len()) };
    (rc, unsafe { CStr::from_ptr(err.as_ptr()) }.to_string_lossy().into_owned())
}

fn stop(key: &str) {
    let key = c(key);
    unsafe { heeler_et_stop(key.as_ptr()) };
}

fn connect(key: &str, network: Option<&str>, host: &str, timeout_ms: u32) -> (c_int, String) {
    let (key, host) = (c(key), c(host));
    let network = network.map(c);
    let mut err = vec![0 as c_char; 512];
    let fd = unsafe {
        heeler_et_tcp_connect_fd(
            key.as_ptr(),
            network.as_ref().map_or(std::ptr::null(), |network| network.as_ptr()),
            host.as_ptr(),
            22,
            timeout_ms,
            err.as_mut_ptr(),
            err.len(),
        )
    };
    (fd, unsafe { CStr::from_ptr(err.as_ptr()) }.to_string_lossy().into_owned())
}

/// Reads the banner on a dialled descriptor.
fn read_banner(fd: c_int, length: usize) -> Vec<u8> {
    let stream = unsafe { std::os::unix::net::UnixStream::from_raw_fd(fd) };
    stream.set_nonblocking(false).expect("blocking");
    stream.set_read_timeout(Some(Duration::from_secs(5))).expect("timeout");
    let mut buf = vec![0u8; length];
    (&stream).read_exact(&mut buf).expect("banner");
    buf
}

/// Dials until it connects (routes take a moment) and returns the banner.
fn dial_banner(key: &str, network: Option<&str>, host: &str, expected: &[u8]) -> Vec<u8> {
    let mut last = String::new();
    for _ in 0..30 {
        let (fd, err) = connect(key, network, host, 3000);
        if fd >= 0 {
            return read_banner(fd, expected.len());
        }
        last = format!("{fd} {err}");
        std::thread::sleep(Duration::from_millis(500));
    }
    panic!("{key} could not dial {host} (network {network:?}): {last}");
}

fn expect_banner(key: &str, network: Option<&str>, host: &str, name: &str) {
    let expected = banner(name);
    assert_eq!(
        String::from_utf8_lossy(&dial_banner(key, network, host, &expected)),
        String::from_utf8_lossy(&expected),
        "{key} -> {host} (network {network:?}) reached the wrong peer"
    );
    eprintln!("  ok: {key} {network:?} {host} -> {name}");
}

fn wait(what: &str, limit: Duration, mut done: impl FnMut() -> bool) {
    let t0 = Instant::now();
    while t0.elapsed() < limit {
        if done() {
            eprintln!("  ok: {what} ({:?})", t0.elapsed());
            return;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    panic!("timed out waiting for {what}");
}

fn has_peer(network: &Value, ipv4: &str) -> bool {
    network["peers"].as_array().is_some_and(|peers| peers.iter().any(|peer| peer["ipv4"] == json!(ipv4)))
}

fn wait_online(key: &str, ipv4: &str, peer: &str) {
    wait(&format!("{key} online as {ipv4} with {peer}"), Duration::from_secs(30), || {
        let s = status(key);
        s["running"] == json!(true) && s["ipv4"] == json!(ipv4) && has_peer(&s, peer)
    });
}

// --- peers -----------------------------------------------------------------

struct Peer {
    node: Arc<NativeCoreInstance>,
    accepted: Arc<AtomicUsize>,
}

async fn peer(network: &str, hostname: &str, ipv4: &str, listener: u16, name: &str) -> Peer {
    let config = TomlConfigLoader::new_from_str(&format!(
        r#"
instance_name = "{hostname}"
hostname = "{hostname}"
ipv4 = "{ipv4}/24"
listeners = ["tcp://127.0.0.1:{listener}"]
[network_identity]
network_name = "{network}"
network_secret = "secret-{network}"
[flags]
no_tun = true
"#
    ))
    .expect("peer config");
    let node = create_native_instance(config).expect("peer");
    node.start().await.expect("peer starts");
    let mut listener = node.data_plane_tcp_bind(22, Duration::from_secs(5)).await.expect("bind 22");
    let accepted = Arc::new(AtomicUsize::new(0));
    let counter = accepted.clone();
    let banner = banner(name);
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            counter.fetch_add(1, Ordering::SeqCst);
            let banner = banner.clone();
            tokio::spawn(async move {
                if stream.write_all(&banner).await.is_ok() {
                    let mut sink = [0u8; 256];
                    while matches!(stream.read(&mut sink).await, Ok(n) if n > 0) {}
                }
            });
        }
    });
    Peer { node, accepted }
}

fn device_toml(network: &str, ipv4: &str, listener: u16) -> String {
    format!(
        r#"instance_name = "heeler"
hostname = "heeler-device"
ipv4 = "{ipv4}"
dhcp = false
listeners = []
[network_identity]
network_name = "{network}"
network_secret = "secret-{network}"
[[peer]]
uri = "tcp://127.0.0.1:{listener}"
[flags]
no_tun = true
disable_relay_data = true
relay_network_whitelist = ""
private_mode = true
"#
    )
}

/// Threads and resident memory of this process.
fn footprint() -> (usize, usize) {
    let pid = std::process::id().to_string();
    let threads = Command::new("ps")
        .args(["-M", "-p", &pid])
        .output()
        .map(|out| String::from_utf8_lossy(&out.stdout).lines().count().saturating_sub(1))
        .unwrap_or(0);
    let rss = Command::new("ps")
        .args(["-o", "rss=", "-p", &pid])
        .output()
        .ok()
        .and_then(|out| String::from_utf8_lossy(&out.stdout).trim().parse().ok())
        .unwrap_or(0);
    (threads, rss)
}

// --- the config server -----------------------------------------------------

struct Server {
    child: Child,
    dir: std::path::PathBuf,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn http(api: u16, cookie: &str, method: &str, path: &str, body: Option<&Value>) -> (u16, String, Vec<String>) {
    let mut stream = TcpStream::connect(("127.0.0.1", api)).expect("API");
    let body = body.map(Value::to_string).unwrap_or_default();
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\nCookie: {cookie}\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .expect("request");
    let mut response = String::new();
    stream.read_to_string(&mut response).expect("response");
    let (head, body) = response.split_once("\r\n\r\n").unwrap_or((&response, ""));
    let status = head.split(' ').nth(1).and_then(|code| code.parse().ok()).unwrap_or(0);
    let cookies = head
        .lines()
        .filter_map(|line| line.strip_prefix("set-cookie: ").or_else(|| line.strip_prefix("Set-Cookie: ")))
        .map(|cookie| cookie.split(';').next().unwrap_or_default().to_owned())
        .collect();
    (status, body.to_owned(), cookies)
}

fn web_network(id: &str, name: &str, device_ipv4: &str, listener: u16) -> Value {
    json!({
        "instance_id": id,
        "network_name": name,
        "network_secret": format!("secret-{name}"),
        "dhcp": false,
        "virtual_ipv4": device_ipv4,
        "network_length": 24,
        "networking_method": 1,
        "peer_urls": [format!("tcp://127.0.0.1:{listener}")],
        "listener_urls": ["tcp://0.0.0.0:11010"],
    })
}

fn web_networks(key: &str) -> Vec<Value> {
    status(key)["web"]["networks"].as_array().cloned().unwrap_or_default()
}

fn web_online(key: &str, name: &str, peer: &str) -> bool {
    web_networks(key)
        .iter()
        .any(|network| network["network_name"] == json!(name) && network["running"] == json!(true) && has_peer(network, peer))
}

fn config_server(binary: &str, rt: &tokio::runtime::Runtime) {
    eprintln!("== 4. one config-server session, several networks");
    const KEY: &str = "web";
    let (config_port, api) = (22540u16, 11740u16);
    let dir = std::env::temp_dir().join(format!("heeler-multi-web-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    let log = std::fs::File::create(dir.join("easytier-web.log")).expect("log");
    let child = Command::new(binary)
        .args(["-d", dir.join("et.db").to_str().expect("path")])
        .args(["-c", &config_port.to_string(), "-p", "udp", "-a", &api.to_string()])
        .args(["--api-server-addr", "127.0.0.1", "--console-log-level", "info"])
        .stdout(Stdio::null())
        .stderr(log)
        .spawn()
        .expect("easytier-web starts");
    let _server = Server { child, dir };
    wait("easytier-web API", Duration::from_secs(30), || TcpStream::connect(("127.0.0.1", api)).is_ok());

    // Three peer groups: x and z share 10.144.160.0/24 (and the peer
    // address 10.144.160.2), y is 10.144.170.0/24.
    let (x, y, z) = rt.block_on(async {
        (
            peer("web-x", "peer-x", "10.144.160.2", 21240, "x").await,
            peer("web-y", "peer-y", "10.144.170.2", 21241, "y").await,
            peer("web-z", "peer-z", "10.144.160.2", 21242, "z").await,
        )
    });

    let (key, url, machine, hostname) =
        (c(KEY), c(&format!("udp://127.0.0.1:{config_port}/user")), c(MACHINE_ID), c("heeler-multi"));
    let mut err = vec![0 as c_char; 512];
    let rc = unsafe {
        heeler_et_web_start(key.as_ptr(), url.as_ptr(), machine.as_ptr(), hostname.as_ptr(), 1, err.as_mut_ptr(), err.len())
    };
    assert_eq!(rc, 0, "web_start: {}", unsafe { CStr::from_ptr(err.as_ptr()) }.to_string_lossy());
    wait("connected to the config server", Duration::from_secs(20), || status(KEY)["web"]["connected"] == json!(true));

    let (code, body, cookies) = http(
        api,
        "",
        "POST",
        "/api/v1/auth/login",
        Some(&json!({ "username": "user", "password": USER_PASSWORD })),
    );
    assert_eq!(code, 200, "login: {body}");
    let cookie = cookies.join("; ");
    wait("device listed in the console", Duration::from_secs(10), || {
        http(api, &cookie, "GET", "/api/v1/machines", None).1.contains("heeler-multi")
    });
    let run = |config: Value| {
        let (code, body, _) = http(
            api,
            &cookie,
            "POST",
            &format!("/api/v1/machines/{MACHINE_ID}/networks"),
            Some(&json!({ "config": config, "save": true })),
        );
        assert_eq!(code, 200, "run: {body}");
    };
    const X: &str = "aaaaaaaa-0000-4000-8000-000000000001";
    const Y: &str = "aaaaaaaa-0000-4000-8000-000000000002";
    const Z: &str = "aaaaaaaa-0000-4000-8000-000000000003";
    run(web_network(X, "web-x", "10.144.160.9", 21240));
    run(web_network(Y, "web-y", "10.144.170.9", 21241));
    wait("web-x and web-y online", Duration::from_secs(40), || {
        web_online(KEY, "web-x", "10.144.160.2") && web_online(KEY, "web-y", "10.144.170.2")
    });
    let s = status(KEY);
    assert_eq!(s["mode"], json!("web"));
    assert_eq!(web_networks(KEY).len(), 2, "{s}");
    assert_eq!(s["web"]["failures"], json!([]), "{s}");

    // A manual key beside the session is unaffected by it.
    expect_banner("net-b", None, "10.144.200.2", "b");

    // Unnamed dials select by exact peer address, own subnet, hostname.
    expect_banner(KEY, None, "10.144.160.2", "x");
    expect_banner(KEY, None, "10.144.170.2", "y");
    expect_banner(KEY, None, "peer-x", "x");
    expect_banner(KEY, None, "PEER-Y.et.net", "y");
    let (fd, err) = connect(KEY, None, "10.144.160.77", 1500);
    assert!(fd < 0, "nobody listens at 10.144.160.77");
    eprintln!("  ok: own-subnet address without a peer selects web-x and fails there: {fd} {err}");
    let (fd, err) = connect(KEY, None, "192.0.2.1", 1000);
    assert_eq!(fd, HEELER_ET_ERR_UNRESOLVED, "{err}");
    // Named dials stay in the named network.
    expect_banner(KEY, Some("web-y"), "10.144.170.2", "y");
    expect_banner(KEY, Some(X), "10.144.160.2", "x");
    let (fd, err) = connect(KEY, Some("web-y"), "peer-x", 1000);
    assert_eq!(fd, HEELER_ET_ERR_UNRESOLVED, "peer-x resolved through web-y: {err}");
    let (fd, err) = connect(KEY, Some("web-q"), "10.144.160.2", 1000);
    assert_eq!(fd, HEELER_ET_ERR_UNRESOLVED, "{err}");
    eprintln!("  ok: named dials never leave their network");

    // A third network on web-x's subnet, with a peer at the same address.
    let z_before = z.accepted.load(Ordering::SeqCst);
    run(web_network(Z, "web-z", "10.144.160.9", 21242));
    wait("web-z online", Duration::from_secs(40), || web_online(KEY, "web-z", "10.144.160.2"));
    let (fd, err) = connect(KEY, None, "10.144.160.2", 1000);
    assert_eq!(fd, HEELER_ET_ERR_AMBIGUOUS, "{err}");
    assert!(err.contains("\"web-x\"") && err.contains("\"web-z\""), "{err}");
    eprintln!("  ok: same address on two networks is ambiguous: {err}");
    assert_eq!(z.accepted.load(Ordering::SeqCst), z_before, "an ambiguous dial reached web-z");
    expect_banner(KEY, Some("web-z"), "10.144.160.2", "z");
    expect_banner(KEY, Some("web-x"), "10.144.160.2", "x");
    expect_banner(KEY, None, "peer-z", "z");

    // Deleting one network in the console leaves the others.
    let (code, _, _) = http(api, &cookie, "DELETE", &format!("/api/v1/machines/{MACHINE_ID}/networks/{X}"), None);
    assert_eq!(code, 200);
    wait("web-x stopped by the console", Duration::from_secs(15), || {
        web_networks(KEY).iter().all(|network| network["network_name"] != json!("web-x"))
    });
    expect_banner(KEY, None, "10.144.160.2", "z");
    expect_banner(KEY, None, "10.144.170.2", "y");
    let (fd, err) = connect(KEY, Some("web-x"), "10.144.160.2", 1000);
    assert_eq!(fd, HEELER_ET_ERR_UNRESOLVED, "{err}");

    unsafe { heeler_et_web_stop(key.as_ptr()) };
    assert_eq!(status(KEY), json!({ "running": false }));
    expect_banner("net-b", None, "10.144.200.2", "b");
    rt.block_on(async {
        for peer in [&x, &y, &z] {
            peer.node.stop().await;
        }
    });
    let _context = rt.enter();
    drop((x, y, z));
}

fn main() {
    let t0 = Instant::now();
    let rt = tokio::runtime::Runtime::new().expect("runtime");
    let (a, b, c_peer) = rt.block_on(async {
        (
            peer("multi-a", "peer-a", "10.144.144.2", 21210, "a").await,
            peer("multi-b", "peer-b", "10.144.200.2", 21211, "b").await,
            peer("multi-c", "peer-c", "10.144.144.2", 21212, "c").await,
        )
    });
    let baseline = footprint();

    eprintln!("== 1. two manual networks on different subnets");
    let toml_a = device_toml("multi-a", "10.144.144.10/24", 21210);
    let toml_b = device_toml("multi-b", "10.144.200.10/24", 21211);
    assert_eq!(start("net-a", &toml_a), (0, String::new()));
    let one = footprint();
    assert_eq!(start("net-b", &toml_b), (0, String::new()));
    wait_online("net-a", "10.144.144.10", "10.144.144.2");
    wait_online("net-b", "10.144.200.10", "10.144.200.2");
    let two = footprint();
    for (key, network) in [("net-a", "multi-a"), ("net-b", "multi-b")] {
        let s = status(key);
        assert_eq!((s["mode"].clone(), s["network_name"].clone()), (json!("manual"), json!(network)), "{s}");
        assert_eq!(s["peer_count"], json!(1), "{key} sees only its own peer: {s}");
        assert_eq!(s["ipv4_prefix"], json!(24));
    }
    expect_banner("net-a", None, "10.144.144.2", "a");
    expect_banner("net-b", None, "10.144.200.2", "b");
    expect_banner("net-a", None, "peer-a", "a");
    expect_banner("net-b", Some("multi-b"), "peer-b.et.net", "b");
    let (fd, err) = connect("net-a", None, "peer-b", 1000);
    assert_eq!(fd, HEELER_ET_ERR_UNRESOLVED, "net-a resolved net-b's peer: {err}");
    let (fd, err) = connect("net-a", Some("multi-b"), "10.144.200.2", 1000);
    assert_eq!(fd, HEELER_ET_ERR_UNRESOLVED, "{err}");
    let b_before = b.accepted.load(Ordering::SeqCst);
    let (fd, err) = connect("net-a", None, "10.144.200.2", 2000);
    assert!(fd < 0, "net-a reached net-b's peer");
    assert_eq!(b.accepted.load(Ordering::SeqCst), b_before, "net-b's peer saw a dial made through net-a");
    eprintln!("  ok: net-a cannot resolve or reach net-b's peer ({fd} {err})");
    // Nor can net-b's peer reach net-a through this device, which is on both.
    let a_before = a.accepted.load(Ordering::SeqCst);
    for target in ["10.144.144.2:22", "10.144.144.10:22"] {
        let target = target.parse().expect("address");
        let reached = rt.block_on(b.node.data_plane_tcp_connect(target, Duration::from_secs(2)));
        assert!(reached.is_err(), "net-b's peer reached {target} through this device");
    }
    assert_eq!(a.accepted.load(Ordering::SeqCst), a_before, "net-a's peer saw a dial from net-b");
    eprintln!("  ok: net-b's peer cannot reach net-a through this device");

    eprintln!("== 2. two manual networks with the same subnet and addresses");
    let toml_c = device_toml("multi-c", "10.144.144.10/24", 21212);
    assert_eq!(start("net-c", &toml_c), (0, String::new()));
    wait_online("net-c", "10.144.144.10", "10.144.144.2");
    let (a_before, c_before) = (a.accepted.load(Ordering::SeqCst), c_peer.accepted.load(Ordering::SeqCst));
    for _ in 0..5 {
        expect_banner("net-a", None, "10.144.144.2", "a");
        expect_banner("net-c", None, "10.144.144.2", "c");
    }
    expect_banner("net-c", None, "peer-c", "c");
    let (fd, _) = connect("net-c", None, "peer-a", 1000);
    assert_eq!(fd, HEELER_ET_ERR_UNRESOLVED);
    assert_eq!(a.accepted.load(Ordering::SeqCst) - a_before, 5, "net-a's peer saw net-c's dials");
    assert_eq!(c_peer.accepted.load(Ordering::SeqCst) - c_before, 6, "net-c's peer saw net-a's dials");

    eprintln!("== 3. independent lifecycles");
    stop("net-a");
    assert_eq!(status("net-a"), json!({ "running": false }));
    let (fd, err) = connect("net-a", None, "10.144.144.2", 1000);
    assert_eq!(fd, HEELER_ET_ERR_NOT_RUNNING, "{err}");
    expect_banner("net-c", None, "10.144.144.2", "c");
    expect_banner("net-b", None, "10.144.200.2", "b");
    assert_eq!(start("net-a", &toml_a), (0, String::new()));
    wait_online("net-a", "10.144.144.10", "10.144.144.2");
    expect_banner("net-a", None, "10.144.144.2", "a");
    // An unchanged start is a no-op; a changed one replaces only its key.
    let t = Instant::now();
    assert_eq!(start("net-c", &toml_c), (0, String::new()));
    assert!(t.elapsed() < Duration::from_secs(1), "an unchanged start restarted");
    assert_eq!(start("net-c", &device_toml("multi-c", "10.144.144.11/24", 21212)), (0, String::new()));
    wait_online("net-c", "10.144.144.11", "10.144.144.2");
    expect_banner("net-c", None, "10.144.144.2", "c");
    expect_banner("net-a", None, "10.144.144.2", "a");
    // A refused configuration leaves the key's network as it was.
    let (rc, err) = start("net-a", "listeners = [\"tcp://0.0.0.0:1\"]\n[network_identity]\nnetwork_name = \"x\"\nnetwork_secret = \"y\"\n");
    assert_eq!(rc, HEELER_ET_ERR, "{err}");
    expect_banner("net-a", None, "10.144.144.2", "a");

    eprintln!(
        "footprint (threads, RSS KiB): before {baseline:?}, 1 network {one:?}, 2 online {two:?}, 3 online {:?}",
        footprint()
    );

    match std::env::var("EASYTIER_WEB") {
        Ok(binary) => config_server(&binary, &rt),
        Err(_) => eprintln!("== 4. SKIPPED: set EASYTIER_WEB to an easytier-web binary"),
    }

    heeler_et_stop_all();
    for key in ["net-a", "net-b", "net-c", "web"] {
        assert_eq!(status(key), json!({ "running": false }), "{key} survived stop_all");
    }
    rt.block_on(async {
        for peer in [&a, &b, &c_peer] {
            peer.node.stop().await;
        }
    });
    let _context = rt.enter();
    drop((a, b, c_peer));
    println!("MULTI OK ({:?})", t0.elapsed());
}
