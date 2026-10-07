// SPDX-License-Identifier: LGPL-3.0-or-later
//
// A standalone EasyTier peer for manual and Simulator end-to-end checks:
// joins network "heeler-e2e" (secret "s3cret") as 10.144.144.2 / "peer-b",
// listens for EasyTier peers on tcp://127.0.0.1:21010, and forwards overlay
// TCP port 22 to an upstream address (default 127.0.0.1:22, the local sshd).
//
//   cargo run --release --example peer -- [upstream]

use std::time::Duration;

use easytier::common::config::TomlConfigLoader;
use easytier::instance::factory::create_native_instance;

fn main() -> anyhow::Result<()> {
    let upstream = std::env::args().nth(1).unwrap_or_else(|| "127.0.0.1:22".to_owned());
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async move {
        let config = TomlConfigLoader::new_from_str(
            r#"
instance_name = "peer-b"
hostname = "peer-b"
ipv4 = "10.144.144.2/24"
listeners = ["tcp://127.0.0.1:21010"]
[network_identity]
network_name = "heeler-e2e"
network_secret = "s3cret"
[flags]
no_tun = true
"#,
        )?;
        let peer = create_native_instance(config)?;
        peer.start().await?;
        let mut listener = peer.data_plane_tcp_bind(22, Duration::from_secs(5)).await?;
        println!("READY 10.144.144.2:22 -> {upstream}");
        while let Ok((mut overlay, from)) = listener.accept().await {
            eprintln!("accepted {from}");
            let upstream = upstream.clone();
            tokio::spawn(async move {
                match tokio::net::TcpStream::connect(&upstream).await {
                    Ok(mut local) => {
                        let _ = tokio::io::copy_bidirectional(&mut overlay, &mut local).await;
                    }
                    Err(error) => eprintln!("upstream {upstream}: {error}"),
                }
            });
        }
        anyhow::Ok(())
    })
}
