use std::path::Path;
use std::sync::Mutex;
use std::time::SystemTime;

/// ponytail: one cache struct instead of two identical Mutex+SystemTime patterns.
/// Invalidation: file mtime check. Thread-safe via Mutex.
pub struct FileCache<T: Clone> {
    inner: Mutex<Option<(T, SystemTime)>>,
}

impl<T: Clone> FileCache<T> {
    pub const fn new() -> Self {
        Self { inner: Mutex::new(None) }
    }

    /// Return cached value if file hasn't changed, else run `load` and cache it.
    pub fn get_or_load(&self, path: &Path, load: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
        if let Ok(meta) = std::fs::metadata(path) {
            if let Ok(modified) = meta.modified() {
                let cache = self.inner.lock().unwrap();
                if let Some((ref val, cached_time)) = *cache {
                    if modified == cached_time {
                        return Ok(val.clone());
                    }
                }
            }
        }
        let val = load()?;
        if let Ok(meta) = std::fs::metadata(path) {
            if let Ok(modified) = meta.modified() {
                *self.inner.lock().unwrap() = Some((val.clone(), modified));
            }
        }
        Ok(val)
    }

    pub fn invalidate(&self) {
        *self.inner.lock().unwrap() = None;
    }
}
