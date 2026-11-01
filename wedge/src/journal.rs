// Copyright (c) 2026 Jody Scott
// SPDX-License-Identifier: Apache-2.0

//! The node's journal of live faults: `<dir>/<fleet>/<fault_id>.json` on a hostPath, written (and
//! synced) before a fault is applied and removed once it is reverted. It is what makes the revert the
//! wedge's own: a wedge that starts reverts every entry it finds before it serves, so a fault outlives
//! neither its caller nor the wedge that applied it.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// What a revert needs, recorded before the fault is applied. Nothing in it came from the caller but the
/// tag and the numbers: the netns, cgroup and pids are the ones the wedge resolved itself.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub fleet: String,
    pub round: String,
    pub fault_id: String,
    /// `FaultKind` as its i32 wire value.
    pub kind: i32,
    pub namespace: String,
    pub pod: String,
    pub container: String,
    pub service: String,
    pub start_unix_ms: i64,
    pub end_unix_ms: i64,
    /// The trace_id of the call that started the fault, so its revert record carries it too.
    pub trace_id: String,
    pub effect: Effect,
}

/// The applied effect, as the revert finds it again.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Effect {
    /// A netem qdisc on the pod's interface, in the network namespace `/proc/<pid>/ns/net` whose inode
    /// was `netns_inode` when it was applied (a pid reused by another process has another netns).
    Netem { pid: u32, netns_inode: u64 },
    /// The stress process, identified by pid and its start time (field 22 of /proc/<pid>/stat), so a pid
    /// reused by another process is never signalled.
    Stress { pid: u32, start_ticks: u64 },
}

#[derive(Clone, Debug)]
pub struct Journal {
    dir: PathBuf,
}

impl Journal {
    /// The journal of one fleet on this node.
    pub fn open(root: &Path, fleet: &str) -> std::io::Result<Journal> {
        let dir = root.join(fleet);
        fs::create_dir_all(&dir)?;
        Ok(Journal { dir })
    }

    fn path(&self, fault_id: &str) -> PathBuf {
        self.dir.join(format!("{fault_id}.json"))
    }

    /// Records an entry durably (written to a temporary file, synced, renamed, the directory synced).
    pub fn write(&self, entry: &Entry) -> std::io::Result<()> {
        let tmp = self.dir.join(format!(".{}.tmp", entry.fault_id));
        let mut f = fs::File::create(&tmp)?;
        f.write_all(&serde_json::to_vec(entry)?)?;
        f.sync_all()?;
        fs::rename(&tmp, self.path(&entry.fault_id))?;
        fs::File::open(&self.dir)?.sync_all()
    }

    /// Removes an entry; one that is gone is success.
    pub fn remove(&self, fault_id: &str) -> std::io::Result<()> {
        match fs::remove_file(self.path(fault_id)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }

    /// Every entry, oldest first. An unreadable one is logged and skipped (it is kept on disk for a human).
    pub fn entries(&self) -> std::io::Result<Vec<Entry>> {
        let mut out = Vec::new();
        for item in fs::read_dir(&self.dir)? {
            let path = item?.path();
            if path.extension().is_none_or(|e| e != "json") {
                continue;
            }
            match fs::read(&path).map_err(|e| e.to_string()).and_then(|b| serde_json::from_slice::<Entry>(&b).map_err(|e| e.to_string())) {
                Ok(entry) => out.push(entry),
                Err(e) => tracing::error!(path = %path.display(), error = %e, "unreadable journal entry"),
            }
        }
        out.sort_by_key(|e| e.start_unix_ms);
        Ok(out)
    }
}

/// A fault id is a journal file name and part of its records' event_id, so it must be a plain token of
/// at most 40 bytes.
pub fn valid_fault_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 40 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, start: i64) -> Entry {
        Entry {
            fleet: "prod".into(),
            round: "r1".into(),
            fault_id: id.into(),
            kind: 3,
            namespace: "tac-prod".into(),
            pod: "stratum-0".into(),
            container: "stratum".into(),
            service: "stratum".into(),
            start_unix_ms: start,
            end_unix_ms: start + 60_000,
            trace_id: "t1".into(),
            effect: Effect::Netem { pid: 42, netns_inode: 4026531992 },
        }
    }

    #[test]
    fn write_list_remove() {
        let dir = tempfile::tempdir().unwrap();
        let j = Journal::open(dir.path(), "prod").unwrap();
        j.write(&entry("b", 2)).unwrap();
        j.write(&entry("a", 1)).unwrap();
        let ids: Vec<_> = j.entries().unwrap().into_iter().map(|e| e.fault_id).collect();
        assert_eq!(ids, ["a", "b"]);
        j.remove("a").unwrap();
        j.remove("a").unwrap();
        assert_eq!(j.entries().unwrap(), vec![entry("b", 2)]);
        // Another fleet's journal on the same node is another directory.
        assert!(Journal::open(dir.path(), "dev").unwrap().entries().unwrap().is_empty());
    }

    #[test]
    fn fault_ids_are_plain_tokens() {
        assert!(valid_fault_id("r12-f1_x"));
        assert!(!valid_fault_id("../etc"));
        assert!(!valid_fault_id(""));
        assert!(valid_fault_id(&"a".repeat(40)));
        assert!(!valid_fault_id(&"a".repeat(41)));
    }
}
