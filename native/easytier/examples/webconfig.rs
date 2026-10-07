// SPDX-License-Identifier: LGPL-3.0-or-later
//
// macOS end-to-end check of the config-server (EasyTier Web) mode of the C ABI
// against a real easytier-web, built from the same verified EasyTier tree:
//
//   (cd vendor/easytier && cargo build --release -p easytier-web)
//   EASYTIER_WEB=vendor/easytier/target/release/easytier-web \
//       cargo run --locked --release --example webconfig
//
// A peer runs in this process through the EasyTier API (10.144.150.2, listener
// tcp://127.0.0.1:21110, a banner on overlay port 22). The device is driven
// only through heeler_et_web_start / heeler_et_status_json /
// heeler_et_tcp_connect_fd, and the console only through easytier-web's REST
// API as its default user "user". Checked, over udp:// and ws:// with
// encryption required, and over udp:// without (it still upgrades):
//
// - the device registers under its machine ID and hostname;
// - a network with the console's default listeners runs with them dropped,
//   and a dial through it reads the peer's banner;
// - a second network with the same name is refused and reported as failed
//   (examples/multi runs two different networks from one server);
// - config patches and other management RPCs are refused;
// - the device reconnects after its session restarts and gets its network
//   back; deleting the network in the console stops it.
//
// Then the device refuses a wss:// server with a self-signed certificate,
// and against a tcp:// server that never offers encryption (it does not
// answer the feature probe) never sends its token when encryption is
// required, and sends it in clear text when it is not.

use std::{
    ffi::{CStr, CString, c_char, c_int},
    io::{Read, Write},
    net::TcpStream,
    os::fd::FromRawFd,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

use easytier::common::config::TomlConfigLoader;
use easytier::instance::factory::create_native_instance;
use heeler_easytier::*;
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;

const BANNER: &[u8] = b"SSH-2.0-heeler-webconfig\r\n";
const MACHINE_ID: &str = "6a1f0e44-6c1e-4f43-9b7f-0123456789ab";
const NETWORK_ID: &str = "11111111-2222-3333-4444-555555555555";
const OTHER_ID: &str = "99999999-2222-3333-4444-555555555555";
/// md5("user"): the console sends the MD5 of the password.
const USER_PASSWORD: &str = "ee11cbb19052e40b07aac0ca060c23ee";
const KEY: &std::ffi::CStr = c"webconfig";

struct Server {
    child: Child,
    api: u16,
    _dir: TempDir,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct TempDir(std::path::PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn start_server(binary: &str, protocol: &str, config_port: u16, api: u16) -> Server {
    let dir = std::env::temp_dir().join(format!("heeler-webconfig-{protocol}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    let log = std::fs::File::create(dir.join("easytier-web.log")).expect("log");
    let child = Command::new(binary)
        .args(["-d", dir.join("et.db").to_str().expect("path")])
        .args(["-c", &config_port.to_string(), "-p", protocol, "-a", &api.to_string()])
        .args(["--api-server-addr", "127.0.0.1", "--console-log-level", "info"])
        .stdout(Stdio::null())
        .stderr(log)
        .spawn()
        .expect("easytier-web starts (set EASYTIER_WEB)");
    let server = Server { child, api, _dir: TempDir(dir) };
    wait("easytier-web API", Duration::from_secs(30), || TcpStream::connect(("127.0.0.1", api)).is_ok());
    server
}

/// A minimal HTTP/1.1 client for the console's REST API.
struct Console {
    api: u16,
    cookie: String,
}

impl Console {
    fn request(&self, method: &str, path: &str, body: Option<&Value>) -> (u16, String, Vec<String>) {
        let mut stream = TcpStream::connect(("127.0.0.1", self.api)).expect("API");
        let body = body.map(Value::to_string).unwrap_or_default();
        write!(
            stream,
            "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\nCookie: {}\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            self.cookie,
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
        let body = if head.to_ascii_lowercase().contains("transfer-encoding: chunked") {
            dechunk(body)
        } else {
            body.to_owned()
        };
        (status, body, cookies)
    }

    fn login(api: u16) -> Console {
        let mut console = Console { api, cookie: String::new() };
        let (status, body, cookies) = console.request(
            "POST",
            "/api/v1/auth/login",
            Some(&json!({ "username": "user", "password": USER_PASSWORD })),
        );
        assert_eq!(status, 200, "login: {body}");
        console.cookie = cookies.join("; ");
        console
    }

    fn run_network(&self, config: Value) -> (u16, String) {
        let (status, body, _) = self.request(
            "POST",
            &format!("/api/v1/machines/{MACHINE_ID}/networks"),
            Some(&json!({ "config": config, "save": true })),
        );
        (status, body)
    }

    fn proxy_rpc(&self, service: &str, method: &str, payload: Value) -> (u16, String) {
        let (status, body, _) = self.request(
            "POST",
            &format!("/api/v1/machines/{MACHINE_ID}/proxy-rpc"),
            Some(&json!({ "service_name": service, "method_name": method, "payload": payload })),
        );
        (status, body)
    }
}

fn dechunk(body: &str) -> String {
    let mut out = String::new();
    let mut rest = body;
    while let Some((size, tail)) = rest.split_once("\r\n") {
        let Ok(size) = usize::from_str_radix(size.trim(), 16) else { break };
        if size == 0 || tail.len() < size {
            break;
        }
        out.push_str(&tail[..size]);
        rest = tail[size..].trim_start_matches("\r\n");
    }
    out
}

fn network(id: &str, name: &str, listeners: &[&str]) -> Value {
    json!({
        "instance_id": id,
        "network_name": name,
        "network_secret": "s3cret",
        "dhcp": false,
        "virtual_ipv4": "10.144.150.9",
        "network_length": 24,
        "networking_method": 1,
        "peer_urls": ["tcp://127.0.0.1:21110"],
        "listener_urls": listeners,
    })
}

fn status() -> Value {
    let mut buf = vec![0 as c_char; 16384];
    let n = unsafe { heeler_et_status_json(KEY.as_ptr(), buf.as_mut_ptr(), buf.len()) };
    assert!(n >= 0 && (n as usize) < buf.len(), "status_json returned {n}");
    serde_json::from_str(&unsafe { CStr::from_ptr(buf.as_ptr()) }.to_string_lossy()).expect("status JSON")
}

fn web_start(url: &str) -> (c_int, String) {
    web_start_with(url, true)
}

fn web_start_with(url: &str, secure: bool) -> (c_int, String) {
    let url = CString::new(url).expect("url");
    let machine = CString::new(MACHINE_ID).expect("id");
    let host = CString::new("heeler-webconfig").expect("hostname");
    let mut err = vec![0 as c_char; 512];
    let rc = unsafe { heeler_et_web_start(KEY.as_ptr(), url.as_ptr(), machine.as_ptr(), host.as_ptr(), c_int::from(secure), err.as_mut_ptr(), err.len()) };
    (rc, unsafe { CStr::from_ptr(err.as_ptr()) }.to_string_lossy().into_owned())
}

fn dial_banner() -> Vec<u8> {
    let host = CString::new("10.144.150.2").expect("host");
    let mut err = vec![0 as c_char; 512];
    let fd = unsafe { heeler_et_tcp_connect_fd(KEY.as_ptr(), std::ptr::null(), host.as_ptr(), 22, 5000, err.as_mut_ptr(), err.len()) };
    assert!(fd >= 0, "dial failed: {}", unsafe { CStr::from_ptr(err.as_ptr()) }.to_string_lossy());
    let stream = unsafe { std::os::unix::net::UnixStream::from_raw_fd(fd) };
    stream.set_nonblocking(false).expect("blocking");
    stream.set_read_timeout(Some(Duration::from_secs(5))).expect("timeout");
    let mut buf = vec![0u8; BANNER.len()];
    (&stream).read_exact(&mut buf).expect("banner");
    buf
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
    panic!("timed out waiting for {what}; status {}", status());
}

fn web(key: &str) -> Value {
    status()["web"][key].clone()
}

/// The session's networks.
fn networks() -> Vec<Value> {
    web("networks").as_array().cloned().unwrap_or_default()
}

/// Running with its address and a route to the peer.
fn online() -> bool {
    let s = status();
    s["running"] == json!(true)
        && networks().iter().any(|network| {
            network["ipv4"] == json!("10.144.150.9")
                && network["ipv4_prefix"] == json!(24)
                && network["peers"]
                    .as_array()
                    .is_some_and(|peers| peers.iter().any(|peer| peer["ipv4"] == json!("10.144.150.2")))
        })
}

fn exercise(binary: &str, protocol: &str, config_port: u16, api: u16, secure: bool) {
    eprintln!("== {protocol}:// config server, encryption required: {secure}");
    let server = start_server(binary, protocol, config_port, api);
    let url = format!("{protocol}://127.0.0.1:{config_port}/user");
    let (rc, err) = web_start_with(&url, secure);
    assert_eq!(rc, 0, "web_start: {err}");
    wait("connected to the config server", Duration::from_secs(20), || web("connected") == json!(true));
    assert_eq!(status()["mode"], json!("web"));
    assert_eq!(web("machine_id"), json!(MACHINE_ID));

    let console = Console::login(server.api);
    wait("device listed in the console", Duration::from_secs(10), || {
        let (_, body, _) = console.request("GET", "/api/v1/machines", None);
        body.contains("heeler-webconfig") && body.contains("\"user_token\":\"user\"")
    });

    // The console's default listeners are dropped, not refused.
    let (code, body) = console.run_network(network(
        NETWORK_ID,
        "heeler-web",
        &["tcp://0.0.0.0:11010", "udp://0.0.0.0:11010", "wg://0.0.0.0:11011"],
    ));
    assert_eq!(code, 200, "run: {body}");
    wait("network online", Duration::from_secs(30), online);
    assert_eq!(networks().len(), 1);
    assert_eq!(networks()[0]["instance_id"], json!(NETWORK_ID));
    assert_eq!(networks()[0]["network_name"], json!("heeler-web"));
    assert_eq!(dial_banner(), BANNER);
    eprintln!("  ok: dial read the peer's banner");

    // A second network of the same name is refused and reported as failed.
    let (code, body) = console.run_network(network(OTHER_ID, "heeler-web", &[]));
    assert_ne!(code, 200, "a second network of the same name ran: {body}");
    assert!(body.contains("already runs an EasyTier network named"), "{body}");
    let failures = web("failures");
    assert_eq!(failures[0]["instance_id"], json!(OTHER_ID), "{failures}");
    assert!(online());
    let (code, _, _) = console.request("DELETE", &format!("/api/v1/machines/{MACHINE_ID}/networks/{OTHER_ID}"), None);
    assert_eq!(code, 200);
    wait("refused network forgotten", Duration::from_secs(10), || web("failures") == json!([]));

    // Hot patches and other management RPCs never reach the network.
    let (code, body) = console.proxy_rpc(
        "api.config.ConfigRpcService",
        "PatchConfig",
        json!({
            "instance": { "id": NETWORK_ID },
            "patch": { "port_forwards": [{ "action": 0, "cfg": {
                "bind_addr": "0.0.0.0:2222", "dst_addr": "10.144.150.2:22", "socket_type": 0 } }] },
        }),
    );
    assert_ne!(code, 200, "patch accepted: {body}");
    let (code, body) = console.proxy_rpc("api.instance.PortForwardManageRpcService", "ListPortForward", json!({}));
    assert_ne!(code, 200, "port-forward RPC served: {body}");
    let (code, body) = console.proxy_rpc("api.logger.LoggerRpcService", "GetLoggerConfig", json!({}));
    assert_ne!(code, 200, "logger RPC served: {body}");
    eprintln!("  ok: patch and management RPCs refused");

    // The interface addresses stay on the device.
    let (code, body, _) =
        console.request("GET", &format!("/api/v1/machines/{MACHINE_ID}/networks/info"), Some(&json!({})));
    assert_eq!(code, 200, "{body}");
    assert!(body.contains("heeler-webconfig") && !body.contains("interface_ipv4s"), "{body}");

    // A new session gets its network back from the server.
    unsafe { heeler_et_web_stop(KEY.as_ptr()) };
    assert_eq!(status(), json!({ "running": false }));
    let (rc, err) = web_start_with(&url, secure);
    assert_eq!(rc, 0, "web_start: {err}");
    wait("network restored after reconnect", Duration::from_secs(30), online);
    assert_eq!(dial_banner(), BANNER);

    // Deleting the network in the console stops it.
    let (code, _, _) = console.request("DELETE", &format!("/api/v1/machines/{MACHINE_ID}/networks/{NETWORK_ID}"), None);
    assert_eq!(code, 200);
    wait("network stopped by the console", Duration::from_secs(10), || {
        let s = status();
        s["running"] == json!(false) && s["web"]["networks"] == json!([])
    });
    unsafe { heeler_et_web_stop(KEY.as_ptr()) };
    drop(server);
}

/// Accepts one TLS connection with a fresh self-signed certificate for
/// 127.0.0.1 and reports how its handshake ended.
async fn self_signed_tls_server(port: u16) -> tokio::sync::oneshot::Receiver<String> {
    let certified = rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_owned()]).expect("certificate");
    let key = rustls::pki_types::PrivatePkcs8KeyDer::from(certified.signing_key.serialize_der());
    let config = rustls::ServerConfig::builder_with_provider(std::sync::Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .expect("versions")
    .with_no_client_auth()
    .with_single_cert(vec![certified.cert.der().clone()], key.into())
    .expect("server config");
    let acceptor = tokio_rustls::TlsAcceptor::from(std::sync::Arc::new(config));
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await.expect("bind");
    let (done, result) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        // The device retries every second; three attempts show it keeps
        // refusing. A refusing client drops the connection after the
        // server's certificate (rustls sends no alert through
        // tokio-rustls here), so the server sees an EOF, never a handshake.
        let mut seen = Vec::new();
        while let Ok((stream, _)) = listener.accept().await {
            seen.push(match acceptor.accept(stream).await {
                Ok(_) => "handshake completed".to_owned(),
                Err(error) => format!("{error:?}"),
            });
            if seen.len() >= 3 || seen.last().is_some_and(|outcome| outcome.contains("completed")) {
                let _ = done.send(seen.join("; "));
                return;
            }
        }
    });
    result
}

/// A tcp:// "config server" that accepts every connection and never answers,
/// so the device's feature probe times out (a server without encryption).
/// Returns how many connections it saw and whether `token` ever arrived in
/// the clear, after `listen_for`.
async fn silent_server(port: u16, token: &'static str, listen_for: Duration) -> (usize, bool) {
    use std::sync::{Arc, Mutex};
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await.expect("bind");
    let received = Arc::new(Mutex::new((0usize, Vec::<u8>::new())));
    let accepting = {
        let received = received.clone();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                received.lock().expect("lock").0 += 1;
                let received = received.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    while let Ok(n) = tokio::io::AsyncReadExt::read(&mut stream, &mut buf).await {
                        if n == 0 {
                            break;
                        }
                        received.lock().expect("lock").1.extend_from_slice(&buf[..n]);
                    }
                });
            }
        })
    };
    tokio::time::sleep(listen_for).await;
    accepting.abort();
    let received = received.lock().expect("lock");
    let seen = received.1.windows(token.len()).any(|window| window == token.as_bytes());
    (received.0, seen)
}

fn main() {
    let binary = std::env::var("EASYTIER_WEB").expect("set EASYTIER_WEB to an easytier-web binary");
    let t0 = Instant::now();

    let rt = tokio::runtime::Runtime::new().expect("runtime");
    let _peer = rt.block_on(async {
        let config = TomlConfigLoader::new_from_str(
            r#"
instance_name = "peer-b"
hostname = "peer-b"
ipv4 = "10.144.150.2/24"
listeners = ["tcp://127.0.0.1:21110"]
[network_identity]
network_name = "heeler-web"
network_secret = "s3cret"
[flags]
no_tun = true
"#,
        )
        .expect("peer config");
        let peer = create_native_instance(config).expect("peer");
        peer.start().await.expect("peer starts");
        let mut listener = peer.data_plane_tcp_bind(22, Duration::from_secs(5)).await.expect("bind 22");
        tokio::spawn(async move {
            while let Ok((mut stream, from)) = listener.accept().await {
                eprintln!("  peer accepted {from}");
                tokio::spawn(async move {
                    let _ = stream.write_all(BANNER).await;
                    let _ = stream.flush().await;
                    // Hold the stream until the device closes it.
                    let mut buf = [0u8; 64];
                    while let Ok(n) = tokio::io::AsyncReadExt::read(&mut stream, &mut buf).await {
                        if n == 0 {
                            break;
                        }
                    }
                });
            }
        });
        peer
    });

    exercise(&binary, "udp", 22520, 11711, true);
    exercise(&binary, "ws", 22521, 11712, true);
    exercise(&binary, "udp", 22523, 11713, false);

    eprintln!("== wss:// server with a self-signed certificate");
    let alert = rt.block_on(self_signed_tls_server(22522));
    let (rc, err) = web_start("wss://127.0.0.1:22522/user");
    assert_eq!(rc, 0, "web_start: {err}");
    let alert = rt.block_on(async { tokio::time::timeout(Duration::from_secs(20), alert).await });
    let alert = alert.expect("the device tried wss://").expect("handshake result");
    assert!(!alert.contains("completed"), "handshakes: {alert}");
    assert_eq!(web("connected"), json!(false));
    eprintln!("  ok: the device refused the certificate ({alert})");
    unsafe { heeler_et_web_stop(KEY.as_ptr()) };

    // Unsupported transports are refused before anything starts.
    let (rc, err) = web_start("ws://example.invalid/user");
    assert_eq!(rc, 0, "{err}");
    let (rc, err) = web_start("http://127.0.0.1:1/user");
    assert_ne!(rc, 0);
    assert!(err.contains("udp://, tcp://, ws:// or wss://"), "{err}");
    let (rc, _) = web_start("quic://127.0.0.1:1/user");
    assert_ne!(rc, 0);
    unsafe { heeler_et_stop(KEY.as_ptr()) };

    // A server without encryption: required, the token never leaves in the
    // clear and the device keeps retrying; not required, it does.
    const TOKEN: &str = "cleartext-probe-token";
    for (secure, port) in [(true, 22524), (false, 22525)] {
        eprintln!("== tcp:// server without encryption, encryption required: {secure}");
        let probe = rt.spawn(silent_server(port, TOKEN, Duration::from_secs(9)));
        let (rc, err) = web_start_with(&format!("tcp://127.0.0.1:{port}/{TOKEN}"), secure);
        assert_eq!(rc, 0, "web_start: {err}");
        let (connections, leaked) = rt.block_on(probe).expect("probe");
        unsafe { heeler_et_web_stop(KEY.as_ptr()) };
        if secure {
            assert!(!leaked, "the token was sent in clear text");
            assert!(connections >= 2, "the device did not retry: {connections} connection(s)");
            eprintln!("  ok: no token in clear text over {connections} connections");
        } else {
            assert!(leaked, "the token never arrived without encryption");
            eprintln!("  ok: the session ran in clear text ({connections} connection(s))");
        }
    }
    assert_eq!(status(), json!({ "running": false }));
    assert_eq!(status(), json!({ "running": false }));

    eprintln!("PASS ({:?})", t0.elapsed());
}
