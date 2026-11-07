//! CLI-only, FFmpeg-independent instance discovery client.
use std::fs;
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

#[derive(Debug, Deserialize)]
struct InstanceRecord {
    pid: u32,
    address: SocketAddr,
    control: SocketAddr,
    secret: String,
}

fn registry_dir() -> io::Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME/USERPROFILE is unavailable"))?;
    Ok(PathBuf::from(home).join(".remedia").join("run"))
}

fn private_registry_directory(path: &Path) -> io::Result<()> {
    if fs::symlink_metadata(path)?.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "runtime directory is a symlink",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if fs::metadata(path)?.permissions().mode() & 0o077 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "runtime directory is not private",
            ));
        }
    }
    Ok(())
}

async fn read_instance(record: InstanceRecord) -> Option<Value> {
    if !record.control.ip().is_loopback() || !record.control.is_ipv4() || record.secret.len() != 32 {
        return None;
    }
    let future = async {
        let mut stream = TcpStream::connect(record.control).await.ok()?;
        stream.write_all(record.secret.as_bytes()).await.ok()?;
        let mut buffer = Vec::new();
        stream.take(16_384).read_to_end(&mut buffer).await.ok()?;
        let response: Value = serde_json::from_slice(&buffer).ok()?;
        if response.get("pid")?.as_u64()? != u64::from(record.pid)
            || response.get("address")?.as_str()? != record.address.to_string()
        {
            return None;
        }
        Some(response)
    };
    timeout(Duration::from_millis(700), future).await.ok().flatten()
}

/// Discover living server instances for the current OS user without loading FFmpeg.
pub async fn list() -> io::Result<Vec<Value>> {
    let directory = registry_dir()?;
    if !directory.exists() {
        return Ok(Vec::new());
    }
    private_registry_directory(&directory)?;
    let mut results = Vec::new();
    for entry in fs::read_dir(directory)?.take(1024) {
        let entry = entry?;
        if !entry.file_type()?.is_file() || entry.path().extension().is_none_or(|s| s != "json") {
            continue;
        }
        let Ok(bytes) = fs::read(entry.path()) else {
            continue;
        };
        if bytes.len() > 4096 {
            continue;
        }
        let Ok(record) = serde_json::from_slice::<InstanceRecord>(&bytes) else {
            continue;
        };
        if let Some(snapshot) = read_instance(record).await {
            results.push(snapshot);
        }
    }
    results.sort_by_key(|value| value.get("pid").and_then(Value::as_u64));
    Ok(results)
}

pub fn print_instances(instances: &[Value], json_output: bool) -> Result<(), Box<dyn std::error::Error>> {
    if json_output {
        println!("{}", serde_json::to_string_pretty(instances)?);
        return Ok(());
    }
    if instances.is_empty() {
        println!("No running Remedia servers found for this user.");
        return Ok(());
    }
    println!(
        "{:<9} {:<23} {:>8} {:>12} {:>11}  HEALTH",
        "PID", "LISTEN", "PLAYERS", "CONNECTIONS", "RSS (MiB)"
    );
    for instance in instances {
        let stats = &instance["stats"];
        let pid = instance["pid"].as_u64().unwrap_or(0);
        let addr = instance["address"].as_str().unwrap_or("?");
        let players = stats["players"]["active"].as_u64().unwrap_or(0);
        let max_players = stats["players"]["maximum"].as_u64().unwrap_or(0);
        let connections = stats["connections"]["active"].as_u64().unwrap_or(0);
        let max_connections = stats["connections"]["maximum"].as_u64().unwrap_or(0);
        let rss = stats["memory"]["residentBytes"]
            .as_u64()
            .map(|b| format!("{:.1}", b as f64 / 1_048_576.0))
            .unwrap_or_else(|| "n/a".into());
        let health = format!(
            "{}/{}",
            stats["processingHealth"].as_str().unwrap_or("?"),
            stats["transportHealth"].as_str().unwrap_or("?")
        );
        println!(
            "{pid:<9} {addr:<23} {players:>3}/{max_players:<4} {connections:>5}/{max_connections:<6} {rss:>11}  {health}"
        );
    }
    Ok(())
}
