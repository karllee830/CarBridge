// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! An address by DHCP on one interface, from busybox's `udhcpc`, and nothing else: no default route, no DNS servers.

use std::io;
use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::process::{Child, Command};

/// Directory of the client's script and lease files: on tmpfs.
const DIR: &str = "/var/run/carbridge";

/// udhcpc calls this with the event as `$1` and the lease in the environment. It sets the address alone and writes
/// the lease to the file named by `$CARBRIDGE_LEASE`.
const SCRIPT: &str = r#"#!/bin/sh
case "$1" in
    deconfig)
        ip -4 addr flush dev "$interface"
        rm -f "$CARBRIDGE_LEASE"
        ;;
    bound|renew)
        ip -4 addr flush dev "$interface"
        ip -4 addr add "$ip/$mask" broadcast + dev "$interface"
        echo "$ip $mask ${serverid:-} ${router%% *}" > "$CARBRIDGE_LEASE.new" && mv "$CARBRIDGE_LEASE.new" "$CARBRIDGE_LEASE"
        ;;
esac
exit 0
"#;

/// The lease the interface holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Lease {
    pub address: Ipv4Addr,
    pub prefix: u8,
    /// The DHCP server.
    pub server: Option<Ipv4Addr>,
    /// The first router the server gave; no route goes through it.
    pub router: Option<Ipv4Addr>,
}

/// A running DHCP client. Dropping it kills the client without releasing the lease; call [`DhcpClient::stop`] to
/// release it and remove the address.
pub struct DhcpClient {
    iface: String,
    child: Child,
    lease_path: PathBuf,
}

impl DhcpClient {
    /// Starts asking for an address on `iface`; it keeps asking until it gets one and renews it.
    pub async fn start(iface: &str) -> io::Result<DhcpClient> {
        tokio::fs::create_dir_all(DIR).await?;
        let script = PathBuf::from(DIR).join("dhcp.script");
        write_file(&script, SCRIPT, 0o700).await?;
        let lease_path = PathBuf::from(DIR).join(format!("dhcp-{iface}.lease"));
        let _ = tokio::fs::remove_file(&lease_path).await;
        let child = Command::new("udhcpc")
            // foreground, release on exit, retry every 2 s forever, log to syslog
            .args([
                "-f", "-R", "-t", "0", "-T", "2", "-A", "2", "-S", "-i", iface, "-s",
            ])
            .arg(&script)
            .env("CARBRIDGE_LEASE", &lease_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()?;
        Ok(DhcpClient {
            iface: iface.to_owned(),
            child,
            lease_path,
        })
    }

    pub fn iface(&self) -> &str {
        &self.iface
    }

    /// The current lease, once there is one.
    pub async fn lease(&self) -> io::Result<Option<Lease>> {
        match tokio::fs::read_to_string(&self.lease_path).await {
            Ok(text) => Ok(parse_lease(&text)),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err),
        }
    }

    /// Waits up to `limit` for a lease.
    pub async fn wait_for_lease(&self, limit: Duration) -> io::Result<Option<Lease>> {
        let end = tokio::time::Instant::now() + limit;
        loop {
            if let Some(lease) = self.lease().await? {
                return Ok(Some(lease));
            }
            if tokio::time::Instant::now() >= end {
                return Ok(None);
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    /// Releases the lease, removes the address and ends the client.
    pub async fn stop(mut self) -> io::Result<()> {
        if let Some(pid) = self.child.id() {
            // udhcpc releases the lease and deconfigures the interface on SIGTERM.
            let _ = Command::new("kill").arg(pid.to_string()).status().await;
            if tokio::time::timeout(Duration::from_secs(3), self.child.wait())
                .await
                .is_err()
            {
                self.child.kill().await?;
            }
        }
        let _ = Command::new("ip")
            .args(["-4", "addr", "flush", "dev", &self.iface])
            .status()
            .await;
        let _ = tokio::fs::remove_file(&self.lease_path).await;
        Ok(())
    }
}

fn parse_lease(text: &str) -> Option<Lease> {
    let mut fields = text.split_whitespace();
    let address = fields.next()?.parse().ok()?;
    let prefix = fields.next()?.parse().ok().filter(|&p: &u8| p <= 32)?;
    let server = fields.next().and_then(|s| s.parse().ok());
    let router = fields.next().and_then(|s| s.parse().ok());
    Some(Lease {
        address,
        prefix,
        server,
        router,
    })
}

async fn write_file(path: &PathBuf, content: &str, mode: u32) -> io::Result<()> {
    let tmp = path.with_extension("new");
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(mode)
        .open(&tmp)
        .await?;
    file.write_all(content.as_bytes()).await?;
    file.sync_all().await?;
    tokio::fs::rename(&tmp, path).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leases() {
        assert_eq!(
            parse_lease("192.168.49.117 24 192.168.49.1 192.168.49.1\n"),
            Some(Lease {
                address: Ipv4Addr::new(192, 168, 49, 117),
                prefix: 24,
                server: Some(Ipv4Addr::new(192, 168, 49, 1)),
                router: Some(Ipv4Addr::new(192, 168, 49, 1)),
            })
        );
        let bare = parse_lease("10.0.0.2 8").unwrap();
        assert_eq!((bare.server, bare.router), (None, None));
        assert_eq!(parse_lease("10.0.0.2 33"), None);
        assert_eq!(parse_lease(""), None);
    }
}
