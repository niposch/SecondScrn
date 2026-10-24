//! Persisted list of paired peers.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::{Fingerprint, SecurityError, write_atomic};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrustedPeer {
    pub fingerprint: Fingerprint,
    pub name: String,
    pub model: String,
    /// Unix seconds.
    pub paired_at: u64,
    pub last_seen: Option<u64>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct TrustFile {
    version: u32,
    peers: Vec<TrustedPeer>,
}

/// Fingerprint-keyed set of paired peers, saved to a JSON file.
#[derive(Debug)]
pub struct TrustStore {
    path: Option<PathBuf>,
    peers: BTreeMap<Fingerprint, TrustedPeer>,
}

pub fn now_unix() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

impl TrustStore {
    pub fn in_memory() -> Self {
        TrustStore { path: None, peers: BTreeMap::new() }
    }

    pub fn load(path: &Path) -> Result<Self, SecurityError> {
        let peers = match std::fs::read(path) {
            Ok(bytes) => {
                let file: TrustFile = serde_json::from_slice(&bytes)
                    .map_err(|e| SecurityError::CorruptIdentity(format!("trust store: {e}")))?;
                file.peers.into_iter().map(|p| (p.fingerprint, p)).collect()
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(e) => return Err(e.into()),
        };
        Ok(TrustStore { path: Some(path.to_path_buf()), peers })
    }

    fn save(&self) -> Result<(), SecurityError> {
        if let Some(path) = &self.path {
            let file = TrustFile { version: 1, peers: self.peers.values().cloned().collect() };
            write_atomic(path, &serde_json::to_vec_pretty(&file).expect("serialisable"))?;
        }
        Ok(())
    }

    pub fn get(&self, fp: &Fingerprint) -> Option<&TrustedPeer> {
        self.peers.get(fp)
    }

    pub fn is_trusted(&self, fp: &Fingerprint) -> bool {
        self.peers.contains_key(fp)
    }

    pub fn peers(&self) -> impl Iterator<Item = &TrustedPeer> {
        self.peers.values()
    }

    pub fn add(&mut self, peer: TrustedPeer) -> Result<(), SecurityError> {
        self.peers.insert(peer.fingerprint, peer);
        self.save()
    }

    /// Revokes a peer. Returns whether it was present.
    pub fn remove(&mut self, fp: &Fingerprint) -> Result<bool, SecurityError> {
        let removed = self.peers.remove(fp).is_some();
        if removed {
            self.save()?;
        }
        Ok(removed)
    }

    pub fn touch(&mut self, fp: &Fingerprint) -> Result<(), SecurityError> {
        if let Some(p) = self.peers.get_mut(fp) {
            p.last_seen = Some(now_unix());
            self.save()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(b: u8) -> TrustedPeer {
        TrustedPeer {
            fingerprint: Fingerprint([b; 32]),
            name: format!("dev{b}"),
            model: "m".into(),
            paired_at: 1,
            last_seen: None,
        }
    }

    #[test]
    fn persists_adds_and_revocations() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trust.json");
        let mut store = TrustStore::load(&path).unwrap();
        store.add(peer(1)).unwrap();
        store.add(peer(2)).unwrap();
        store.touch(&Fingerprint([1; 32])).unwrap();

        let mut reloaded = TrustStore::load(&path).unwrap();
        assert!(reloaded.is_trusted(&Fingerprint([1; 32])));
        assert!(reloaded.get(&Fingerprint([1; 32])).unwrap().last_seen.is_some());
        assert!(reloaded.remove(&Fingerprint([2; 32])).unwrap());
        assert!(!reloaded.remove(&Fingerprint([2; 32])).unwrap());

        let again = TrustStore::load(&path).unwrap();
        assert!(!again.is_trusted(&Fingerprint([2; 32])));
        assert_eq!(again.peers().count(), 1);
    }
}
