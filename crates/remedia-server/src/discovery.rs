//! Local, same-user instance discovery. This is an authenticated loopback-only
//! status endpoint, not a second WebSocket API and not a process manager.
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rand::random;
use remedia_server::ServerHandle;
use serde::{Deserialize, Serialize};
use serde_json::json;
use subtle::ConstantTimeEq;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio::time::timeout;

#[derive(Clone, Debug, Deserialize, Serialize)]
struct InstanceRecord {
    pid: u32,
    id: String,
    address: SocketAddr,
    control: SocketAddr,
    secret: String,
}

pub struct Registration {
    path: PathBuf,
    task: JoinHandle<()>,
}

impl Drop for Registration {
    fn drop(&mut self) {
        self.task.abort();
        let _ = fs::remove_file(&self.path);
    }
}

fn registry_dir() -> io::Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "HOME/USERPROFILE is unavailable")
        })?;
    Ok(PathBuf::from(home).join(".remedia").join("run"))
}

fn private_registry_directory(path: &Path) -> io::Result<()> {
    fs::create_dir_all(path)?;
    if fs::symlink_metadata(path)?.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "runtime directory is a symlink",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
        if fs::metadata(path)?.permissions().mode() & 0o077 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "runtime directory must be private",
            ));
        }
    }
    Ok(())
}

fn write_registration(path: &Path, record: &InstanceRecord) -> io::Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(&serde_json::to_vec(record).map_err(io::Error::other)?)?;
    file.sync_all()
}

pub async fn register(server: Arc<ServerHandle>) -> io::Result<Registration> {
    let directory = registry_dir()?;
    private_registry_directory(&directory)?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let id = format!("{:032x}", random::<u128>());
    let record = InstanceRecord {
        pid: std::process::id(),
        id: id.clone(),
        address: server.local_addr(),
        control: listener.local_addr()?,
        secret: format!("{:032x}", random::<u128>()),
    };
    let path = directory.join(format!("{id}.json"));
    write_registration(&path, &record)?;
    let task = tokio::spawn(async move {
        let gate = Arc::new(tokio::sync::Semaphore::new(8));
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let Ok(permit) = Arc::clone(&gate).try_acquire_owned() else {
                continue;
            };
            let server = Arc::clone(&server);
            let secret = record.secret.clone();
            tokio::spawn(async move {
                let _permit = permit;
                let _ = timeout(
                    Duration::from_secs(2),
                    serve_status(stream, &secret, &server),
                )
                .await;
            });
        }
    });
    Ok(Registration { path, task })
}

async fn serve_status(
    mut stream: TcpStream,
    secret: &str,
    server: &ServerHandle,
) -> io::Result<()> {
    let mut supplied = vec![0; secret.len()];
    stream.read_exact(&mut supplied).await?;
    if !bool::from(supplied.as_slice().ct_eq(secret.as_bytes())) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "invalid status token",
        ));
    }
    let response = json!({
        "pid": std::process::id(),
        "address": server.local_addr(),
        "stats": server.statistics(),
    });
    stream
        .write_all(&serde_json::to_vec(&response).map_err(io::Error::other)?)
        .await?;
    stream.shutdown().await
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn status_files_are_private_and_do_not_overwrite_existing_files() {
        let dir = std::env::temp_dir().join(format!(
            "remedia-instance-test-{}-{}",
            std::process::id(),
            random::<u64>()
        ));
        private_registry_directory(&dir).unwrap();
        let record = InstanceRecord {
            pid: 1,
            id: "test".into(),
            address: "127.0.0.1:8787".parse().unwrap(),
            control: "127.0.0.1:10000".parse().unwrap(),
            secret: "a".repeat(32),
        };
        let path = dir.join("test.json");
        write_registration(&path, &record).unwrap();
        assert!(write_registration(&path, &record).is_err());
        let decoded: InstanceRecord = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(decoded.id, "test");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(&dir).unwrap().permissions().mode() & 0o077, 0);
            assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o077, 0);
        }
        fs::remove_dir_all(dir).unwrap();
    }
}
