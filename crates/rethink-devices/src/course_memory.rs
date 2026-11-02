//! Course memory for laundry handlers whose appliance reports its installed download course
//! only while it runs: one JSON object on the persistent volume, keyed by device id, so the
//! installed course survives a restart. Same file and format as upstream's RH16_T_KR dryer
//! (PR #182), which the S5MPC styler (PR #173) shares.

use rethink_util::sync::Mutex;
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::{Map, Value};
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

const DEFAULT_FILE: &str = "/app/data/rethink-course-memory.json";

/// Handlers run on several threads; this keeps their read-modify-writes of the file apart.
static FILE_LOCK: Mutex<()> = Mutex::new(());

/// One device's entry in the course memory file.
pub struct CourseMemory {
    file: PathBuf,
    id: String,
    last_saved: Mutex<Option<String>>,
}

impl CourseMemory {
    pub fn new(file: PathBuf, id: &str) -> Self {
        Self {
            file,
            id: id.to_string(),
            last_saved: Mutex::new(None),
        }
    }

    /// `RETHINK_MEMORY_FILE`, else the file on the `/app/data` volume.
    pub fn default_file() -> PathBuf {
        std::env::var_os("RETHINK_MEMORY_FILE")
            .map_or_else(|| PathBuf::from(DEFAULT_FILE), PathBuf::from)
    }

    /// This device's entry. A missing or torn file reads as no entry.
    pub fn load<T: DeserializeOwned>(&self) -> Option<T> {
        match read_all(&self.file)?.remove(&self.id)? {
            entry @ Value::Object(_) => serde_json::from_value(entry).ok(),
            _ => None,
        }
    }

    /// Store `entry` unless it is what this handler stored last, so repeated status frames do
    /// not rewrite the file. A failure is logged and retried on the next call.
    pub fn save<T: Serialize>(&self, entry: &T) {
        let Ok(entry) = serde_json::to_value(entry) else {
            return;
        };
        let serialized = entry.to_string();
        let mut last = self.last_saved.lock();
        if last.as_deref() == Some(serialized.as_str()) {
            return;
        }
        match self.write(entry) {
            Ok(()) => *last = Some(serialized),
            Err(err) => tracing::warn!("course memory save failed for {}: {err}", self.id),
        }
    }

    /// Written to a temporary file and renamed over the old one, so a crash mid-write cannot
    /// leave a torn file.
    fn write(&self, entry: Value) -> io::Result<()> {
        let _guard = FILE_LOCK.lock();
        let mut all = read_all(&self.file).unwrap_or_default();
        all.insert(self.id.clone(), entry);
        let mut tmp = self.file.clone().into_os_string();
        tmp.push(format!(".{}.tmp", std::process::id()));
        let tmp = PathBuf::from(tmp);
        let result = write_synced(&tmp, Value::Object(all).to_string().as_bytes())
            .and_then(|()| fs::rename(&tmp, &self.file));
        if result.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        result
    }
}

fn read_all(file: &Path) -> Option<Map<String, Value>> {
    serde_json::from_str(&fs::read_to_string(file).ok()?).ok()
}

fn write_synced(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = File::create(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn each_device_keeps_its_own_entry_in_the_shared_file() {
        let file = std::env::temp_dir().join(format!(
            "rethink-course-memory-shared-{}.json",
            std::process::id()
        ));
        let _ = fs::remove_file(&file);
        CourseMemory::new(file.clone(), "dryer").save(&json!({"downloadedCourse": "Refresh"}));
        CourseMemory::new(file.clone(), "styler").save(&json!({"smart": 3}));
        let load = |id: &str| CourseMemory::new(file.clone(), id).load::<Value>();
        assert_eq!(load("dryer"), Some(json!({"downloadedCourse": "Refresh"})));
        assert_eq!(load("styler"), Some(json!({"smart": 3})));
        assert_eq!(load("washer"), None);
        let _ = fs::remove_file(&file);
    }
}
