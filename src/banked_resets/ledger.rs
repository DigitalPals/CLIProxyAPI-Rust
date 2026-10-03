//! A locked, durable journal. An unfinished entry always blocks a new spend.
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow, ensure};
use chrono::{DateTime, Utc};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Operation {
    pub request_id: String,
    pub account: String,
    pub provider: String,
    pub grant_id: String,
    pub clears: Vec<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub status: String,
    pub message: String,
}
impl Operation {
    pub fn unsettled(&self) -> bool {
        matches!(self.status.as_str(), "pending" | "unknown")
    }
    pub fn retryable(&self) -> bool {
        self.unsettled() && (Utc::now() - self.created_at).num_seconds() < 600
    }
}

#[derive(Default, Serialize, Deserialize)]
pub struct Journal {
    pub operations: BTreeMap<String, Operation>,
}
impl Journal {
    pub fn unresolved(&self) -> Option<&Operation> {
        self.operations.values().find(|o| o.unsettled())
    }
    pub fn latest(&self) -> Option<&Operation> {
        self.unresolved().or_else(|| self.operations.values().max_by_key(|o| o.created_at))
    }
}

pub struct Ledger {
    _lock: File,
    path: PathBuf,
    pub journal: Journal,
}

fn private_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
}
fn sync_dir(path: &Path) -> Result<()> {
    #[cfg(unix)]
    File::open(path)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}
impl Ledger {
    pub fn open(root: &Path, identity: &str) -> Result<Self> {
        let dir = root.join(".banked-resets");
        fs::create_dir_all(&dir).map_err(|_| anyhow!("Cannot create reset journal directory"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
        }
        sync_dir(root)?;
        let name = hex::encode(Sha256::digest(identity.as_bytes()));
        let lock = private_options()
            .open(dir.join(format!("{name}.lock")))
            .map_err(|_| anyhow!("Cannot open reset journal lock"))?;
        lock.try_lock_exclusive().map_err(|_| anyhow!("Another reset operation is running; refresh shortly"))?;
        let path = dir.join(format!("{name}.json"));
        let journal = match File::open(&path) {
            Ok(file) => {
                ensure!(file.metadata()?.len() <= 16 << 20, "Reset journal is too large; operator review required");
                let mut text = String::new();
                file.take((16 << 20) + 1).read_to_string(&mut text)?;
                serde_json::from_str(&text)
                    .map_err(|_| anyhow!("Reset journal is unreadable; operator review required"))?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Journal::default(),
            Err(_) => return Err(anyhow!("Cannot read reset journal; operator review required")),
        };
        for (key, op) in &journal.operations {
            ensure!(
                key == &op.request_id
                    && uuid::Uuid::parse_str(key).is_ok()
                    && !op.account.is_empty()
                    && matches!(op.provider.as_str(), "claude" | "codex")
                    && matches!(
                        op.status.as_str(),
                        "pending"
                            | "unknown"
                            | "applied"
                            | "already_used"
                            | "refused"
                            | "reconciled_used"
                            | "reconciled_unused"
                    ),
                "Reset journal contains an invalid operation; operator review required"
            );
        }
        Ok(Self { _lock: lock, path, journal })
    }
    pub fn save(&self) -> Result<()> {
        let dir = self.path.parent().unwrap();
        let temp = dir.join(format!("{}.tmp", uuid::Uuid::new_v4()));
        let result = (|| -> Result<()> {
            let mut options = private_options();
            options.create_new(true);
            let mut file = options.open(&temp)?;
            file.write_all(&serde_json::to_vec(&self.journal)?)?;
            file.sync_all()?;
            // Windows cannot rename over an existing file. Keep a durable replacement
            // via MoveFileExW, rather than deleting the old journal first.
            #[cfg(not(windows))]
            fs::rename(&temp, &self.path)?;
            #[cfg(windows)]
            replace_windows(&temp, &self.path)?;
            sync_dir(dir)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(temp);
        }
        result.map_err(|_| anyhow!("Cannot persist reset operation; do not submit another reset"))
    }
}

#[cfg(windows)]
fn replace_windows(from: &Path, to: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn MoveFileExW(from: *const u16, to: *const u16, flags: u32) -> i32;
    }
    let from: Vec<u16> = from.as_os_str().encode_wide().chain(Some(0)).collect();
    let to: Vec<u16> = to.as_os_str().encode_wide().chain(Some(0)).collect();
    // MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH
    ensure!(unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), 1 | 8) } != 0, "Journal replacement failed");
    Ok(())
}

/// For an unavailable provider after restart, still display a locally unresolved spend.
/// These snapshots are informational; mutations always take the identity lock.
pub fn last_operation(root: &Path, provider: crate::accounts::Provider, account: &str) -> Result<Option<Operation>> {
    let dir = root.join(".banked-resets");
    if !dir.exists() {
        return Ok(None);
    }
    let mut latest: Option<Operation> = None;
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.extension().is_none_or(|e| e != "json") {
            continue;
        }
        let file = File::open(path)?;
        ensure!(file.metadata()?.len() <= 16 << 20, "Reset journal too large");
        let journal: Journal = serde_json::from_reader(file.take((16 << 20) + 1))?;
        for operation in journal.operations.values().filter(|o| o.account == account && o.provider == provider.as_str())
        {
            if latest.as_ref().is_none_or(|old| {
                (!old.unsettled() && operation.unsettled())
                    || (old.unsettled() == operation.unsettled() && old.created_at < operation.created_at)
            }) {
                latest = Some(operation.clone());
            }
        }
    }
    Ok(latest)
}
