//! Detects open files changing on disk behind the editor's back.
//!
//! A background thread stats the watched files a couple of times per second
//! and only wakes the UI when one of them differs from what the editor last
//! saw, so an idle editor stays idle. Polling (rather than inotify) also works
//! on network and FUSE mounts, and copes with editors that save by renaming.

use crossbeam_channel::{Receiver, Sender};
use eframe::egui;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, SystemTime};

const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Cheap fingerprint of a file's on-disk state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiskStamp {
    mtime: Option<SystemTime>,
    len: u64,
    /// Changes when a file is replaced by renaming another over it.
    ino: u64,
}

impl DiskStamp {
    /// `None` if the file doesn't exist (or can't be stat'ed).
    pub fn of(path: &Path) -> Option<Self> {
        let m = std::fs::metadata(path).ok()?;
        #[cfg(unix)]
        let ino = std::os::unix::fs::MetadataExt::ino(&m);
        #[cfg(not(unix))]
        let ino = 0;
        Some(Self {
            mtime: m.modified().ok(),
            len: m.len(),
            ino,
        })
    }
}

type Watched = HashMap<PathBuf, Option<DiskStamp>>;

pub struct FileWatcher {
    watched: Arc<Mutex<Watched>>,
    /// Paths whose on-disk stamp no longer matches the one last synced.
    pub rx: Receiver<PathBuf>,
}

impl FileWatcher {
    pub fn new(ctx: egui::Context) -> Self {
        let watched = Arc::new(Mutex::new(Watched::new()));
        let (tx, rx) = crossbeam_channel::unbounded();
        let weak = Arc::downgrade(&watched);
        std::thread::Builder::new()
            .name("file-watcher".into())
            .spawn(move || poll_loop(weak, tx, ctx))
            .expect("spawn file watcher");
        Self { watched, rx }
    }

    /// Replace the watch list with `files`: each path with the stamp the
    /// editor last observed for it. Cheap when nothing changed.
    pub fn sync<'a>(&self, files: impl Iterator<Item = (&'a PathBuf, Option<DiskStamp>)>) {
        let files: Vec<_> = files.collect();
        let mut w = self.watched.lock().unwrap();
        if files.len() == w.len() && files.iter().all(|(p, s)| w.get(*p) == Some(s)) {
            return;
        }
        w.clear();
        w.extend(files.into_iter().map(|(p, s)| (p.clone(), s)));
    }
}

fn poll_loop(watched: Weak<Mutex<Watched>>, tx: Sender<PathBuf>, ctx: egui::Context) {
    loop {
        std::thread::sleep(POLL_INTERVAL);
        // The watcher (and with it the app) is gone.
        let Some(watched) = watched.upgrade() else {
            return;
        };
        let snapshot: Vec<(PathBuf, Option<DiskStamp>)> = watched
            .lock()
            .unwrap()
            .iter()
            .map(|(p, s)| (p.clone(), *s))
            .collect();
        drop(watched);
        let mut changed = false;
        for (path, expected) in snapshot {
            if DiskStamp::of(&path) != expected {
                if tx.send(path).is_err() {
                    return;
                }
                changed = true;
            }
        }
        if changed {
            ctx.request_repaint();
        }
    }
}
