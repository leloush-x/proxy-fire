//! File watching with debounce (catalog/config hot-reload).

use notify::{Event, RecursiveMode, Watcher};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Watch `path` (or its parent dir when the file does not exist yet) and call
/// `on_event` on write/create/rename events, debounced. The returned handle
/// stops the watcher when dropped.
pub struct WatchHandle {
    _watcher: notify::RecommendedWatcher,
    done: Option<mpsc::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for WatchHandle {
    fn drop(&mut self) {
        if let Some(done) = self.done.take() {
            let _ = done.send(());
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

pub fn watch_file(
    path: &Path,
    debounce: Duration,
    mut on_event: impl FnMut(PathBuf) + Send + 'static,
) -> Result<WatchHandle, notify::Error> {
    let expanded = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    };
    let watch_dir = if expanded
        .parent()
        .map(|p| p.as_os_str().is_empty())
        .unwrap_or(true)
    {
        PathBuf::from(".")
    } else {
        expanded.parent().unwrap().to_path_buf()
    };
    std::fs::create_dir_all(&watch_dir)?;

    let (tx, rx) = mpsc::channel::<notify::Result<Event>>();
    let mut watcher = notify::recommended_watcher(move |res| {
        let _ = tx.send(res);
    })?;
    watcher.watch(&watch_dir, RecursiveMode::NonRecursive)?;

    let (done_tx, done_rx) = mpsc::channel::<()>();
    let thread = std::thread::spawn(move || {
        let mut pending: Option<Instant> = None;
        let base = expanded.file_name().map(|f| f.to_owned());
        loop {
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(Ok(event)) => {
                    let relevant = event.paths.iter().any(|p| p.file_name() == base.as_deref());
                    let interesting = matches!(
                        event.kind,
                        notify::EventKind::Create(_) | notify::EventKind::Modify(_)
                    );
                    if relevant && interesting {
                        pending = Some(Instant::now());
                    }
                }
                Ok(Err(e)) => tracing::warn!("watch error: {e}"),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if let Some(t) = pending {
                        if t.elapsed() >= debounce {
                            on_event(expanded.clone());
                            pending = None;
                        }
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            }
            if done_rx.try_recv().is_ok() {
                return;
            }
        }
    });

    Ok(WatchHandle {
        _watcher: watcher,
        done: Some(done_tx),
        thread: Some(thread),
    })
}
