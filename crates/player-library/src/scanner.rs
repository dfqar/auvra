use crate::{Database, error, metadata};
use player_domain::*;
use std::{
    fs::{self, ReadDir},
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

pub(crate) struct Scanner {
    pub progress: ScanProgress,
    pub epoch: u64,
    scan_id: u64,
    stack: Vec<ReadDir>,
    partial: bool,
    issue: Option<String>,
}
impl Scanner {
    pub fn start(root: &LibraryRoot, epoch: u64, db: &Database) -> Result<Self, PlayerError> {
        if !root.authorized {
            return Err(PlayerError::new(
                ErrorCode::PermissionLost,
                "Choose the folder to restore access",
            ));
        }
        let entries = fs::read_dir(&root.path).map_err(error)?;
        let scan_id = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(error)?
            .as_nanos()
            .min(i64::MAX as u128) as u64;
        db.status(&root.id, "scanning", None)?;
        Ok(Self {
            progress: ScanProgress {
                root_id: root.id.clone(),
                ..Default::default()
            },
            epoch,
            scan_id,
            stack: vec![entries],
            partial: false,
            issue: None,
        })
    }
    pub fn step(&mut self, db: &mut Database) -> Result<bool, PlayerError> {
        let Some(entries) = self.stack.last_mut() else {
            return Ok(true);
        };
        let entry = match entries.next() {
            None => {
                self.stack.pop();
                return Ok(self.stack.is_empty());
            }
            Some(Err(e)) => {
                self.partial = true;
                self.issue = Some(e.to_string());
                self.progress.failed += 1;
                return Ok(false);
            }
            Some(Ok(entry)) => entry,
        };
        self.progress.visited += 1;
        match entry.file_type() {
            Ok(kind) if kind.is_symlink() => return Ok(false),
            Ok(kind) if kind.is_dir() => {
                if self.stack.len() >= 64 {
                    self.partial = true;
                    self.issue = Some("Folder depth exceeds the scan limit".into());
                } else {
                    match fs::read_dir(entry.path()) {
                        Ok(entries) => self.stack.push(entries),
                        Err(e) => {
                            self.partial = true;
                            self.issue = Some(e.to_string());
                            self.progress.failed += 1;
                        }
                    }
                }
                return Ok(false);
            }
            Ok(kind) if kind.is_file() => {}
            Ok(_) => return Ok(false),
            Err(e) => {
                self.partial = true;
                self.issue = Some(e.to_string());
                self.progress.failed += 1;
                return Ok(false);
            }
        }
        let path = entry.path();
        if !path
            .extension()
            .and_then(|s| s.to_str())
            .is_some_and(crate::is_supported_extension)
        {
            return Ok(false);
        }
        if let Err(e) = self.file(path, db) {
            if e.code == ErrorCode::DatabaseError {
                return Err(e);
            }
            self.partial = true;
            self.issue = Some(e.message);
            self.progress.failed += 1;
        }
        Ok(false)
    }
    fn file(&mut self, path: PathBuf, db: &mut Database) -> Result<(), PlayerError> {
        let path_text = path
            .to_str()
            .ok_or_else(|| error("File name is not valid UTF-8"))?;
        let info = fs::symlink_metadata(&path).map_err(error)?;
        if !info.file_type().is_file() {
            return Err(error("File changed type while scanning; rescan to retry"));
        }
        let file = metadata::fingerprint(&info)?;
        let cached = db.cached(path_text)?;
        if let Some((track, previous)) = &cached
            && previous == &file
            && track.issue.is_none()
            && track
                .item
                .artwork
                .as_ref()
                .is_none_or(|p| std::path::Path::new(p).is_file())
        {
            db.seen(&self.progress.root_id, track.item.id.as_str(), self.scan_id)?;
            self.progress.unchanged += 1;
            return Ok(());
        }
        let track = match metadata::inspect(&path, &db.cache) {
            Ok((track, warning)) => {
                if let Some(warning) = warning {
                    self.issue = Some(warning);
                    self.progress.failed += 1;
                }
                let after = metadata::fingerprint(&fs::symlink_metadata(&path).map_err(error)?)?;
                if after != file {
                    return Err(error(
                        "File changed while its metadata was being read; rescan to retry",
                    ));
                }
                self.progress.imported += 1;
                track
            }
            Err(e) => {
                self.progress.failed += 1;
                let mut track = if let Some((track, _)) = cached {
                    track
                } else {
                    metadata::skeleton(&path)?
                };
                metadata::finish(&mut track, &path)?;
                track.issue = Some(e.message.chars().take(512).collect());
                track
            }
        };
        db.upsert(&track, &file, &self.progress.root_id, self.scan_id)
    }
    pub fn finish(self, db: &mut Database) -> Result<ScanProgress, PlayerError> {
        db.finish_scan(
            &self.progress.root_id,
            self.scan_id,
            !self.partial,
            self.issue.as_deref(),
        )?;
        Ok(self.progress)
    }
}
