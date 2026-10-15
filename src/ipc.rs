//! CLI <-> daemon: a Unix stream socket in the runtime directory. One request
//! line (`toggle`, `status`, `refresh`), one JSON reply line, then close.
//! The directory is 0700 and the socket 0600: only the owner can drive it.
use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub fn socket_path(runtime: &Path) -> PathBuf {
    runtime.join("diktor.sock")
}

/// Binds the daemon socket. A live daemon answering on it is an error; a
/// stale file left by a crash is replaced.
pub fn bind(path: &Path) -> anyhow::Result<UnixListener> {
    if let Some(directory) = path.parent() {
        std::fs::create_dir_all(directory)?;
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))?;
    }
    if path.exists() {
        anyhow::ensure!(
            UnixStream::connect(path).is_err(),
            "another diktor daemon is already running on {}",
            path.display()
        );
        std::fs::remove_file(path)?;
    }
    let listener = UnixListener::bind(path)?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

/// Serves requests until the listener fails; `handle` maps a request to its reply.
pub fn serve(listener: UnixListener, handle: impl Fn(&str) -> Value + Send + Sync + 'static) {
    let handle = std::sync::Arc::new(handle);
    for stream in listener.incoming() {
        let Ok(stream) = stream else {
            continue;
        };
        let handle = std::sync::Arc::clone(&handle);
        // A slow client must not block the next toggle.
        std::thread::spawn(move || {
            let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
            let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
            let mut line = String::new();
            let mut reader = BufReader::new(&stream);
            if reader.read_line(&mut line).is_err() {
                return;
            }
            log::debug!("IPC request {:?}", line.trim());
            let reply = handle(line.trim());
            let mut writer = &stream;
            let _ = writeln!(writer, "{reply}");
        });
    }
}

/// One request to the running daemon; `Err` when none answers.
pub fn request(path: &Path, command: &str) -> anyhow::Result<Value> {
    let stream = UnixStream::connect(path)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    let mut writer = &stream;
    writeln!(writer, "{command}")?;
    let mut line = String::new();
    BufReader::new(&stream).read_line(&mut line)?;
    Ok(serde_json::from_str(&line)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_daemon_is_refused_and_a_stale_socket_is_replaced() {
        let directory = std::env::temp_dir().join(format!("diktor-ipc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        let path = socket_path(&directory);
        let listener = bind(&path).unwrap();
        assert!(bind(&path).is_err());
        std::thread::spawn(move || {
            serve(listener, |request| serde_json::json!({ "echo": request }))
        });
        let reply = request(&path, "status").unwrap();
        assert_eq!(reply["echo"], "status");
        // A crashed daemon leaves the file behind with nobody listening.
        let stale = directory.join("stale.sock");
        drop(UnixListener::bind(&stale).unwrap());
        assert!(request(&stale, "status").is_err());
        assert!(bind(&stale).is_ok());
        std::fs::remove_dir_all(directory).unwrap();
    }
}
