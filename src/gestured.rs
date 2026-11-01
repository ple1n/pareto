use std::{
    fs, io,
    os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt},
    os::unix::net::UnixStream as StdUnixStream,
    path::PathBuf,
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use evdev::{EventSummary, KeyCode};
use futures::{
    StreamExt,
    stream::{self, BoxStream},
};
use inotify::{Inotify, WatchMask};
use tokio::{
    io::AsyncWriteExt,
    net::{UnixListener, UnixStream},
    sync::{broadcast, mpsc},
};

const COMBO_WINDOW: Duration = Duration::from_millis(500);
const RUNTIME_DIR: &str = "/run/pareto";

pub async fn run() -> Result<()> {
    let uid = unsafe { libc::getuid() };
    let gid = unsafe { libc::getgid() };
    let socket_path = socket_path(uid);
    prepare_runtime_dir()?;
    remove_stale_socket(&socket_path, uid)?;

    let listener = UnixListener::bind(&socket_path)
        .with_context(|| format!("failed to bind gesture socket {}", socket_path.display()))?;
    let _socket_cleanup = SocketCleanup(socket_path.clone());
    fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o600))?;
    if unsafe { libc::chown(path_cstring(&socket_path)?.as_ptr(), uid, gid) } != 0 {
        return Err(std::io::Error::last_os_error()).context("failed to set gesture socket owner");
    }

    let (events_tx, _) = broadcast::channel::<()>(16);
    tokio::spawn(accept_clients(listener, events_tx.clone()));

    let mut device_changes = monitor_input_changes()?;
    let mut events = enumerate_events();
    let mut combo = ComboDetector::default();
    let mut reported_no_clients = false;
    loop {
        if events.is_empty() {
            device_changes
                .recv()
                .await
                .ok_or_else(|| anyhow::anyhow!("input-device monitor stopped"))?;
            tokio::time::sleep(Duration::from_millis(100)).await;
            events = enumerate_events();
            continue;
        }

        tokio::select! {
            event = events.next() => {
                match event {
                    Some(Ok(event)) => {
                        let EventSummary::Key(_, code, value) = event.destructure() else {
                            continue;
                        };
                        if combo.process(code, value, Instant::now()) {
                            match events_tx.send(()) {
                                Ok(_) => reported_no_clients = false,
                                Err(_) if !reported_no_clients => {
                                    eprintln!("no gesture clients are connected");
                                    reported_no_clients = true;
                                }
                                Err(_) => {}
                            }
                        }
                    }
                    Some(Err(error)) => {
                        eprintln!("input device stream failed: {error}");
                        events = enumerate_events();
                    }
                    None => events = enumerate_events(),
                }
            }
            changed = device_changes.recv() => {
                changed.ok_or_else(|| anyhow::anyhow!("input-device monitor stopped"))?;
                tokio::time::sleep(Duration::from_millis(100)).await;
                events = enumerate_events();
            }
        }
    }
}

fn enumerate_events() -> stream::SelectAll<BoxStream<'static, io::Result<evdev::InputEvent>>> {
    let mut devices = Vec::new();
    for (path, device) in evdev::enumerate() {
        let keys = device.supported_keys();
        if !keys.is_some_and(|keys| {
            keys.contains(KeyCode::KEY_LEFTCTRL) && keys.contains(KeyCode::KEY_LEFTMETA)
        }) {
            continue;
        }

        match device.into_event_stream() {
            Ok(events) => devices.push(events.boxed()),
            Err(error) => {
                eprintln!("cannot monitor input device {}: {error}", path.display());
            }
        }
    }
    stream::select_all(devices)
}

fn monitor_input_changes() -> Result<mpsc::UnboundedReceiver<()>> {
    let (sender, receiver) = mpsc::unbounded_channel();
    thread::Builder::new()
        .name("pareto-input-monitor".into())
        .spawn(move || {
            let result = (|| -> Result<()> {
                let mut inotify = Inotify::init()?;
                inotify.watches().add(
                    "/dev/input/",
                    WatchMask::CREATE
                        | WatchMask::DELETE
                        | WatchMask::MOVED_TO
                        | WatchMask::MOVED_FROM,
                )?;
                let mut buffer = [0u8; 4096];
                loop {
                    inotify.read_events_blocking(&mut buffer)?;
                    if sender.send(()).is_err() {
                        return Ok(());
                    }
                }
            })();
            if let Err(error) = result {
                eprintln!("input-device monitor failed: {error:#}");
            }
        })
        .context("failed to start input-device monitor")?;
    Ok(receiver)
}

async fn accept_clients(listener: UnixListener, events: broadcast::Sender<()>) {
    loop {
        let (stream, _) = match listener.accept().await {
            Ok(connection) => connection,
            Err(error) => {
                eprintln!("gesture socket accept failed: {error}");
                continue;
            }
        };
        let receiver = events.subscribe();
        tokio::spawn(send_events(stream, receiver));
    }
}

async fn send_events(mut stream: UnixStream, mut events: broadcast::Receiver<()>) {
    loop {
        match events.recv().await {
            Ok(()) => {
                if let Err(error) = stream.write_all(b"toggle\n").await {
                    eprintln!("gesture client disconnected: {error}");
                    return;
                }
            }
            Err(broadcast::error::RecvError::Lagged(count)) => {
                eprintln!("gesture client missed {count} events");
            }
            Err(broadcast::error::RecvError::Closed) => return,
        }
    }
}

fn socket_path(uid: libc::uid_t) -> PathBuf {
    PathBuf::from(RUNTIME_DIR).join(format!("gesture-{uid}.sock"))
}

fn prepare_runtime_dir() -> Result<()> {
    let path = PathBuf::from(RUNTIME_DIR);
    match fs::create_dir(&path) {
        Ok(()) => {
            fs::set_permissions(&path, fs::Permissions::from_mode(0o711))?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error).context("failed to create gesture runtime directory"),
    }

    let metadata = fs::symlink_metadata(&path)
        .with_context(|| format!("failed to inspect {}", path.display()))?;
    if !metadata.is_dir() || metadata.uid() != 0 || metadata.permissions().mode() & 0o022 != 0 {
        bail!(
            "gesture runtime directory must be a root-owned, non-world-writable directory: {}",
            path.display()
        );
    }
    Ok(())
}

fn remove_stale_socket(path: &std::path::Path, uid: libc::uid_t) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_socket() && metadata.uid() == uid => {
            match StdUnixStream::connect(path) {
                Ok(_) => bail!("gesture daemon is already running for uid {uid}"),
                Err(error) if error.kind() == std::io::ErrorKind::ConnectionRefused => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("failed to check existing gesture socket {}", path.display())
                    });
                }
            }
            fs::remove_file(path)
                .with_context(|| format!("failed to remove stale socket {}", path.display()))?;
        }
        Ok(_) => bail!(
            "refusing to replace non-socket gesture path {}",
            path.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| format!("failed to inspect {}", path.display()));
        }
    }
    Ok(())
}

fn path_cstring(path: &std::path::Path) -> Result<std::ffi::CString> {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::CString::new(path.as_os_str().as_bytes())
        .context("gesture socket path contains a null byte")
}

struct SocketCleanup(PathBuf);

impl Drop for SocketCleanup {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_file(&self.0) {
            if error.kind() != std::io::ErrorKind::NotFound {
                eprintln!(
                    "failed to remove gesture socket {}: {error}",
                    self.0.display()
                );
            }
        }
    }
}

#[derive(Default)]
struct ComboDetector {
    previous_key: Option<(KeyCode, Instant)>,
    active: bool,
}

impl ComboDetector {
    fn process(&mut self, code: KeyCode, value: i32, now: Instant) -> bool {
        let is_combo_key = code == KeyCode::KEY_LEFTCTRL || code == KeyCode::KEY_LEFTMETA;
        if !is_combo_key {
            return false;
        }
        if value == 0 {
            self.active = false;
            return false;
        }
        if value != 1 {
            return false;
        }

        let detected = self.previous_key.is_some_and(|(previous, time)| {
            previous != code && now.duration_since(time) <= COMBO_WINDOW && !self.active
        });
        if detected {
            self.active = true;
        }
        self.previous_key = Some((code, now));
        detected
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_left_ctrl_and_left_meta_form_a_gesture() {
        let mut detector = ComboDetector::default();
        let now = Instant::now();

        assert!(!detector.process(KeyCode::KEY_LEFTCTRL, 1, now));
        assert!(detector.process(KeyCode::KEY_LEFTMETA, 1, now + Duration::from_millis(100)));
        assert!(!detector.process(KeyCode::KEY_LEFTCTRL, 1, now + Duration::from_millis(200)));
    }

    #[test]
    fn combo_window_expires_and_release_allows_next_gesture() {
        let mut detector = ComboDetector::default();
        let now = Instant::now();

        assert!(!detector.process(KeyCode::KEY_LEFTCTRL, 1, now));
        assert!(!detector.process(KeyCode::KEY_LEFTMETA, 1, now + Duration::from_millis(501)));
        assert!(!detector.process(KeyCode::KEY_LEFTCTRL, 0, now + Duration::from_secs(1)));
        assert!(!detector.process(KeyCode::KEY_LEFTMETA, 0, now + Duration::from_secs(1)));
        assert!(!detector.process(KeyCode::KEY_LEFTCTRL, 1, now + Duration::from_secs(2)));
        assert!(detector.process(
            KeyCode::KEY_LEFTMETA,
            1,
            now + Duration::from_secs(2) + Duration::from_millis(100)
        ));
    }
}
