// SPDX-License-Identifier: LGPL-3.0-or-later
//
// Peers for HeelerOverlay's gated EasyTierMultiLiveTests (iOS Simulator,
// which shares the Mac's loopback). Three EasyTier networks, one peer each,
// every peer serving its own banner on overlay port 22:
//
//   network          secret      peer address   hostname  listener              banner
//   heeler-live-a    live-a      10.144.144.2   peer-a    tcp://127.0.0.1:21310 SSH-2.0-heeler-live-a
//   heeler-live-b    live-b      10.144.144.2   peer-b    tcp://127.0.0.1:21311 SSH-2.0-heeler-live-b
//   heeler-live-c    live-c      10.144.150.2   peer-c    tcp://127.0.0.1:21312 SSH-2.0-heeler-live-c
//
// a and b deliberately share the subnet and the peer address. With
// `--web <easytier-web>` it also runs a config server (udp://127.0.0.1:22550,
// console API on 127.0.0.1:11750, user "user") and, once the device
// 5c0f0e44-6c1e-4f43-9b7f-0123456789ef connects, assigns it heeler-live-a
// (as 10.144.144.9/24) and heeler-live-c (as 10.144.150.9/24). The device
// is recognised by its hostname, heeler-live-device (the console reports
// machine IDs as integer parts).
//
//   cargo run --release --example multi-peer [-- --web /path/to/easytier-web]

use std::{
    io::{Read, Write},
    net::TcpStream,
    process::{Command, Stdio},
    time::Duration,
};

use easytier::common::config::TomlConfigLoader;
use easytier::instance::factory::create_native_instance;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const MACHINE_ID: &str = "5c0f0e44-6c1e-4f43-9b7f-0123456789ef";
/// md5("user"): the console sends the MD5 of the password.
const USER_PASSWORD: &str = "ee11cbb19052e40b07aac0ca060c23ee";
const PEERS: [(&str, &str, &str, u16); 3] = [
    ("a", "10.144.144.2", "peer-a", 21310),
    ("b", "10.144.144.2", "peer-b", 21311),
    ("c", "10.144.150.2", "peer-c", 21312),
];

fn http(api: u16, cookie: &str, method: &str, path: &str, body: Option<&Value>) -> (u16, String, Vec<String>) {
    let Ok(mut stream) = TcpStream::connect(("127.0.0.1", api)) else { return (0, String::new(), Vec::new()) };
    let body = body.map(Value::to_string).unwrap_or_default();
    let _ = write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\nCookie: {cookie}\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let mut response = String::new();
    let _ = stream.read_to_string(&mut response);
    let (head, body) = response.split_once("\r\n\r\n").unwrap_or((&response, ""));
    let status = head.split(' ').nth(1).and_then(|code| code.parse().ok()).unwrap_or(0);
    let cookies = head
        .lines()
        .filter_map(|line| line.strip_prefix("set-cookie: ").or_else(|| line.strip_prefix("Set-Cookie: ")))
        .map(|cookie| cookie.split(';').next().unwrap_or_default().to_owned())
        .collect();
    (status, body.to_owned(), cookies)
}

/// Runs easytier-web and assigns the live device its two networks once it
/// shows up; keeps the server running until the process ends.
fn serve_console(binary: String) {
    let (config_port, api) = (22550u16, 11750u16);
    let dir = std::env::temp_dir().join(format!("heeler-live-web-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    let log = std::fs::File::create(dir.join("easytier-web.log")).expect("log");
    let mut child = Command::new(binary)
        .args(["-d", dir.join("et.db").to_str().expect("path")])
        .args(["-c", &config_port.to_string(), "-p", "udp", "-a", &api.to_string()])
        .args(["--api-server-addr", "127.0.0.1", "--console-log-level", "info"])
        .stdout(Stdio::null())
        .stderr(log)
        .spawn()
        .expect("easytier-web starts");
    std::thread::spawn(move || {
        let cookie = loop {
            let (code, _, cookies) = http(
                api,
                "",
                "POST",
                "/api/v1/auth/login",
                Some(&json!({ "username": "user", "password": USER_PASSWORD })),
            );
            if code == 200 {
                break cookies.join("; ");
            }
            std::thread::sleep(Duration::from_millis(250));
        };
        println!("READY config server udp://127.0.0.1:{config_port}/user, machine {MACHINE_ID}");
        let mut assigned = false;
        loop {
            let (_, machines, _) = http(api, &cookie, "GET", "/api/v1/machines", None);
            let present = machines.contains("\"hostname\":\"heeler-live-device\"");
            if present && !assigned {
                for (id, network, ipv4, listener) in [
                    ("bbbbbbbb-0000-4000-8000-00000000000a", "heeler-live-a", "10.144.144.9", 21310),
                    ("bbbbbbbb-0000-4000-8000-00000000000c", "heeler-live-c", "10.144.150.9", 21312),
                ] {
                    let config = json!({
                        "instance_id": id,
                        "network_name": network,
                        "network_secret": network.trim_start_matches("heeler-"),
                        "dhcp": false,
                        "virtual_ipv4": ipv4,
                        "network_length": 24,
                        "networking_method": 1,
                        "peer_urls": [format!("tcp://127.0.0.1:{listener}")],
                    });
                    let (code, body, _) = http(
                        api,
                        &cookie,
                        "POST",
                        &format!("/api/v1/machines/{MACHINE_ID}/networks"),
                        Some(&json!({ "config": config, "save": true })),
                    );
                    println!("assigned {network}: {code} {body}");
                }
                assigned = true;
            }
            if child.try_wait().ok().flatten().is_some() {
                eprintln!("easytier-web exited; see {}", dir.display());
                return;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    });
}

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let mut web = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--web" => web = args.next(),
            other => anyhow::bail!("unknown argument {other}"),
        }
    }
    let rt = tokio::runtime::Runtime::new()?;
    let _peers = rt.block_on(async {
        let mut peers = Vec::new();
        for (name, ipv4, hostname, port) in PEERS {
            let config = TomlConfigLoader::new_from_str(&format!(
                r#"
instance_name = "{hostname}"
hostname = "{hostname}"
ipv4 = "{ipv4}/24"
listeners = ["tcp://127.0.0.1:{port}"]
[network_identity]
network_name = "heeler-live-{name}"
network_secret = "live-{name}"
[flags]
no_tun = true
"#
            ))?;
            let peer = create_native_instance(config)?;
            peer.start().await?;
            let mut listener = peer.data_plane_tcp_bind(22, Duration::from_secs(5)).await?;
            let banner = format!("SSH-2.0-heeler-live-{name}\r\n");
            tokio::spawn(async move {
                while let Ok((mut stream, from)) = listener.accept().await {
                    eprintln!("{hostname} accepted {from}");
                    let banner = banner.clone();
                    tokio::spawn(async move {
                        if stream.write_all(banner.as_bytes()).await.is_ok() {
                            let mut sink = [0u8; 256];
                            while matches!(stream.read(&mut sink).await, Ok(n) if n > 0) {}
                        }
                    });
                }
            });
            println!("READY heeler-live-{name} {ipv4} {hostname} tcp://127.0.0.1:{port}");
            peers.push(peer);
        }
        anyhow::Ok(peers)
    })?;
    if let Some(binary) = web {
        serve_console(binary);
    }
    rt.block_on(std::future::pending::<()>());
    Ok(())
}
