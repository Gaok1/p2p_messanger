use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum TransferDirection {
    Outgoing,
    Incoming,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TransferRecord {
    pub id: u64,
    pub peer_id: String,
    pub direction: TransferDirection,
    pub path: PathBuf,
    pub size: u64,
    pub offset: u64,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

#[derive(Default, Serialize, Deserialize)]
struct JournalFile {
    transfers: Vec<TransferRecord>,
}

/// Journal simples (snapshot JSON) para retomar transferências após queda/crash.
///
/// Propriedades desejadas:
/// - Persistente
/// - Escrita atômica via write->rename
/// - Chave estável (`id`) baseada em (peer_id, direção, path, size)
pub struct TransferJournal {
    path: PathBuf,
    records: HashMap<u64, TransferRecord>,
    dirty: bool,
    last_flush_ms: u64,
    min_flush_interval_ms: u64,
}

impl TransferJournal {
    pub fn empty(path: PathBuf) -> Self {
        Self {
            path,
            records: HashMap::new(),
            dirty: false,
            last_flush_ms: 0,
            min_flush_interval_ms: 500,
        }
    }

    pub fn load(path: PathBuf) -> io::Result<Self> {
        let mut journal = Self::empty(path);

        match std::fs::read_to_string(&journal.path) {
            Ok(content) => {
                let file: JournalFile = serde_json::from_str(&content).unwrap_or_default();
                for rec in file.transfers {
                    journal.records.insert(rec.id, rec);
                }
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(err),
        }

        Ok(journal)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn pending_outgoing_for_peer(&self, peer_id: &str) -> Vec<PathBuf> {
        let mut paths: Vec<_> = self
            .records
            .values()
            .filter(|r| r.direction == TransferDirection::Outgoing && r.peer_id == peer_id)
            .map(|r| r.path.clone())
            .collect();
        paths.sort();
        paths.dedup();
        paths
    }

    pub fn upsert_progress(
        &mut self,
        peer_id: &str,
        direction: TransferDirection,
        path: &Path,
        size: u64,
        offset: u64,
    ) {
        let id = stable_transfer_id(peer_id, direction, path, size);
        let now = now_ms();
        let rec = self.records.entry(id).or_insert_with(|| TransferRecord {
            id,
            peer_id: peer_id.to_string(),
            direction,
            path: path.to_path_buf(),
            size,
            offset,
            created_at_ms: now,
            updated_at_ms: now,
        });

        rec.offset = offset.min(size);
        rec.updated_at_ms = now;
        self.dirty = true;
        self.flush_if_due();
    }

    pub fn remove(
        &mut self,
        peer_id: &str,
        direction: TransferDirection,
        path: &Path,
        size: u64,
    ) {
        let id = stable_transfer_id(peer_id, direction, path, size);
        if self.records.remove(&id).is_some() {
            self.dirty = true;
            self.flush_if_due_force();
        }
    }

    pub fn flush_if_due(&mut self) {
        if !self.dirty {
            return;
        }
        let now = now_ms();
        if now.saturating_sub(self.last_flush_ms) < self.min_flush_interval_ms {
            return;
        }
        let _ = self.flush();
    }

    pub fn flush_if_due_force(&mut self) {
        if !self.dirty {
            return;
        }
        let _ = self.flush();
    }

    pub fn flush(&mut self) -> io::Result<()> {
        if !self.dirty {
            return Ok(());
        }

        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let mut transfers: Vec<_> = self.records.values().cloned().collect();
        transfers.sort_by_key(|r| (r.peer_id.clone(), r.created_at_ms, r.id));
        let file = JournalFile { transfers };
        let json = serde_json::to_string_pretty(&file)?;

        let tmp = self.path.with_extension("tmp");
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, &self.path)?;

        self.dirty = false;
        self.last_flush_ms = now_ms();
        Ok(())
    }
}

fn stable_transfer_id(peer_id: &str, direction: TransferDirection, path: &Path, size: u64) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    peer_id.hash(&mut h);
    direction.hash(&mut h);
    path.to_string_lossy().hash(&mut h);
    size.hash(&mut h);
    h.finish()
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
