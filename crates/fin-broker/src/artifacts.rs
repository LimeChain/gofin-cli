//! Bounded immutable artifact cache. No filesystem paths arrive over IPC.
use fin_domain::{FinError, Result};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
const MAX_BYTES: usize = 32 * 1024 * 1024;
const CHUNK_BYTES: usize = 256 * 1024;
const TTL: Duration = Duration::from_secs(300);
struct Blob {
    bytes: Vec<u8>,
    expires: Instant,
}
#[derive(Default)]
pub(crate) struct ArtifactCache {
    blobs: BTreeMap<String, Blob>,
    in_flight: usize,
}
/// Reserve capacity before downloading; cancellation releases the slot too.
pub(crate) struct Reservation {
    cache: Arc<Mutex<ArtifactCache>>,
    active: bool,
}
impl Reservation {
    pub fn begin(cache: Arc<Mutex<ArtifactCache>>) -> Result<Self> {
        {
            let mut locked = cache.lock().map_err(|_| missing())?;
            locked.blobs.retain(|_, b| b.expires > Instant::now());
            if locked.blobs.len() + locked.in_flight >= 2 {
                return Err(FinError::new(
                    "ARTIFACT_CACHE_LIMIT",
                    "read",
                    "The bounded export cache is full.",
                    "Wait for an active handle to expire, then retry the read.",
                ));
            }
            locked.in_flight += 1;
        }
        Ok(Self {
            cache,
            active: true,
        })
    }
    pub fn publish(mut self, provider: &str, content_type: &str, bytes: Vec<u8>) -> Result<Value> {
        let mut cache = self.cache.lock().map_err(|_| missing())?;
        cache.in_flight -= 1;
        self.active = false;
        cache.insert(provider, content_type, bytes)
    }
}
impl Drop for Reservation {
    fn drop(&mut self) {
        if self.active
            && let Ok(mut cache) = self.cache.lock()
        {
            cache.in_flight = cache.in_flight.saturating_sub(1);
        }
    }
}
fn missing() -> FinError {
    FinError::new(
        "ARTIFACT_HANDLE_UNAVAILABLE",
        "read",
        "This artifact handle is unknown or expired.",
        "Restart the export read; it never dispatches a financial action.",
    )
}
impl ArtifactCache {
    pub fn insert(&mut self, provider: &str, content_type: &str, bytes: Vec<u8>) -> Result<Value> {
        self.blobs.retain(|_, b| b.expires > Instant::now());
        if bytes.len() > MAX_BYTES || self.blobs.len() + self.in_flight >= 2 {
            return Err(FinError::new(
                "ARTIFACT_CACHE_LIMIT",
                "read",
                "The bounded export cache is full or this export exceeds 32 MiB.",
                "Wait five minutes for previous export handles to expire, then read a smaller export.",
            ));
        }
        let id = uuid::Uuid::new_v4().to_string();
        let result = json!({"artifactId":id,"provider":provider,"contentType":content_type,"size":bytes.len(),"sha256":hex::encode(Sha256::digest(&bytes)),"chunkBytes":CHUNK_BYTES,"expiresInSeconds":TTL.as_secs()});
        self.blobs.insert(
            id,
            Blob {
                bytes,
                expires: Instant::now() + TTL,
            },
        );
        Ok(result)
    }
    pub fn release(&mut self, id: &str) -> Value {
        self.blobs.remove(id);
        json!({"released":true})
    }
    pub fn chunk(&mut self, id: &str, offset: u64) -> Result<Value> {
        self.blobs.retain(|_, b| b.expires > Instant::now());
        let blob = self.blobs.get(id).ok_or_else(missing)?;
        let start = usize::try_from(offset).map_err(|_| missing())?;
        if start > blob.bytes.len() {
            return Err(missing());
        }
        let end = start.saturating_add(CHUNK_BYTES).min(blob.bytes.len());
        Ok(
            json!({"artifactId":id,"offset":start,"hex":hex::encode(&blob.bytes[start..end]),"nextOffset":end,"eof":end==blob.bytes.len()}),
        )
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn immutable_bounded_chunks_and_expiry() {
        let mut cache = ArtifactCache::default();
        let metadata = cache
            .insert("kraken", "application/zip", vec![7; 300_000])
            .unwrap();
        let id = metadata["artifactId"].as_str().unwrap();
        let first = cache.chunk(id, 0).unwrap();
        assert_eq!(first["hex"].as_str().unwrap().len(), CHUNK_BYTES * 2);
        assert_eq!(first["eof"], false);
        let last = cache.chunk(id, CHUNK_BYTES as u64).unwrap();
        assert_eq!(last["nextOffset"], 300_000);
        assert_eq!(last["eof"], true);
        assert!(cache.chunk(id, u64::MAX).is_err());
        assert!(cache.chunk("guessed", 0).is_err());
        cache.blobs.get_mut(id).unwrap().expires = Instant::now();
        assert_eq!(
            cache.chunk(id, 0).unwrap_err().code,
            "ARTIFACT_HANDLE_UNAVAILABLE"
        );
    }
    #[test]
    fn in_flight_downloads_and_cancellation_respect_total_capacity() {
        let cache = Arc::new(Mutex::new(ArtifactCache::default()));
        let first = Reservation::begin(cache.clone()).unwrap();
        let second = Reservation::begin(cache.clone()).unwrap();
        assert!(Reservation::begin(cache.clone()).is_err());
        drop(second);
        let next = Reservation::begin(cache.clone()).unwrap();
        let metadata = first.publish("kraken", "text/csv", vec![1]).unwrap();
        drop(next);
        let next = Reservation::begin(cache.clone()).unwrap();
        next.publish("kraken", "text/csv", vec![2]).unwrap();
        assert!(Reservation::begin(cache.clone()).is_err());
        assert_eq!(
            cache
                .lock()
                .unwrap()
                .chunk(metadata["artifactId"].as_str().unwrap(), 0)
                .unwrap()["hex"],
            "01"
        );
    }
    #[test]
    fn active_handles_are_not_evicted_or_overwritten() {
        let mut cache = ArtifactCache::default();
        let a = cache.insert("kraken", "text/csv", vec![1]).unwrap();
        cache.insert("kraken", "text/csv", vec![2]).unwrap();
        assert_eq!(
            cache
                .insert("kraken", "text/csv", vec![3])
                .unwrap_err()
                .code,
            "ARTIFACT_CACHE_LIMIT"
        );
        assert_eq!(
            cache.chunk(a["artifactId"].as_str().unwrap(), 0).unwrap()["hex"],
            "01"
        );
    }
}
