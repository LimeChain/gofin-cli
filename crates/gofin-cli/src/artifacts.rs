//! Private, non-overwriting local delivery of bounded broker artifacts.
use super::Client;
use gofin_domain::{GoFinError, Result};
use gofin_protocol::BrokerRequest;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{io::Write, path::Path};

pub(crate) fn new_request_id() -> String {
    uuid::Uuid::new_v4().to_string()
}
fn invalid() -> GoFinError {
    GoFinError::new(
        "INVALID_ARTIFACT",
        "read",
        "The broker artifact transfer was inconsistent or incomplete.",
        "Retry the read; no output file was published.",
    )
}
fn local() -> GoFinError {
    GoFinError::new(
        "ARTIFACT_OUTPUT_UNAVAILABLE",
        "read",
        "The output path cannot be created privately without overwriting an existing file.",
        "Choose a new filename in an existing directory you own.",
    )
}
pub(crate) async fn download(
    client: &dyn Client,
    operation: String,
    input: Value,
    path: &Path,
) -> Result<Value> {
    if path.file_name().is_none() || path.symlink_metadata().is_ok() {
        return Err(local());
    }
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut temporary = tempfile::Builder::new()
        .prefix(".gofin-export-")
        .tempfile_in(parent)
        .map_err(|_| local())?;
    let metadata = client
        .call(BrokerRequest::ReadArtifact {
            operation: operation.clone(),
            input,
        })
        .await?;
    let id = metadata["artifactId"]
        .as_str()
        .filter(|s| !s.is_empty() && s.len() <= 128)
        .ok_or_else(invalid)?
        .to_owned();
    let result = async {
    let size = metadata["size"].as_u64().filter(|n| *n <= 32 * 1024 * 1024).ok_or_else(invalid)?;
    let expected = metadata["sha256"].as_str().filter(|s| s.len() == 64).ok_or_else(invalid)?;
    let mut digest = Sha256::new();
    let mut offset = 0u64;
    loop {
        let chunk = client.call(BrokerRequest::ArtifactChunk { artifact_id: id.clone(), offset }).await?;
        if chunk["artifactId"] != id || chunk["offset"] != offset { return Err(invalid()); }
        let encoded = chunk["hex"].as_str().filter(|s| s.len() <= 2 * 256 * 1024).ok_or_else(invalid)?;
        let bytes = hex::decode(encoded).map_err(|_| invalid())?;
        let next = offset.checked_add(bytes.len() as u64).ok_or_else(invalid)?;
        if next > size || chunk["nextOffset"] != next || chunk["eof"].as_bool() != Some(next == size) || (bytes.is_empty() && next != size) { return Err(invalid()); }
        temporary.write_all(&bytes).map_err(|_| local())?;
        digest.update(&bytes);
        offset = next;
        if offset == size { break; }
    }
    if hex::encode(digest.finalize()) != expected { return Err(invalid()); }
    temporary.as_file().sync_all().map_err(|_| local())?;
    temporary.persist_noclobber(path).map_err(|_| local())?;
    Ok(json!({"provider":metadata["provider"],"operation":operation,"output":path,"bytes":size,"sha256":expected,"contentType":metadata["contentType"]}))
    }.await;
    let _ = client
        .call(BrokerRequest::ReleaseArtifact { artifact_id: id })
        .await;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    struct Fixture {
        bytes: Vec<u8>,
        corrupt: bool,
    }
    #[async_trait]
    impl Client for Fixture {
        async fn execute(&self, _: gofin_domain::OperationRequest) -> Result<Value> {
            panic!("artifact read must never execute");
        }
        async fn call(&self, request: BrokerRequest) -> Result<Value> {
            match request {
                BrokerRequest::ReadArtifact { .. } => Ok(
                    json!({"artifactId":"synthetic-export","size":self.bytes.len(),"sha256":hex::encode(Sha256::digest(&self.bytes)),"provider":"kraken","contentType":"application/zip"}),
                ),
                BrokerRequest::ArtifactChunk {
                    artifact_id,
                    offset,
                } => {
                    let start = offset as usize;
                    let end = (start + 256 * 1024).min(self.bytes.len());
                    Ok(
                        json!({"artifactId":artifact_id,"offset":offset,"hex":if self.corrupt { hex::encode(vec![0;end-start]) } else { hex::encode(&self.bytes[start..end]) },"nextOffset":end,"eof":end == self.bytes.len()}),
                    )
                }
                BrokerRequest::ReleaseArtifact { .. } => Ok(json!({"released":true})),
                _ => panic!("unexpected fixture request"),
            }
        }
    }
    #[tokio::test]
    async fn exact_large_binary_is_private_and_never_overwritten() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("report.zip");
        let fixture = Fixture {
            bytes: vec![0xa5; 300_000],
            corrupt: false,
        };
        let result = download(&fixture, "kraken.retrieve-export".into(), json!({}), &path)
            .await
            .unwrap();
        assert_eq!(result["bytes"], 300_000);
        assert_eq!(std::fs::read(&path).unwrap(), fixture.bytes);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        assert_eq!(
            download(&fixture, "kraken.retrieve-export".into(), json!({}), &path)
                .await
                .unwrap_err()
                .code,
            "ARTIFACT_OUTPUT_UNAVAILABLE"
        );
    }
    #[tokio::test]
    async fn corrupt_complete_blob_publishes_no_partial_or_temp_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("report.csv");
        assert!(
            download(
                &Fixture {
                    bytes: vec![1; 8],
                    corrupt: true
                },
                "krakenfutures.account-log-csv".into(),
                json!({}),
                &path
            )
            .await
            .is_err()
        );
        assert!(!path.exists());
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn symlink_output_is_never_followed() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("original");
        let path = directory.path().join("report");
        std::fs::write(&target, b"original").unwrap();
        std::os::unix::fs::symlink(&target, &path).unwrap();
        assert!(
            download(
                &Fixture {
                    bytes: vec![1],
                    corrupt: false
                },
                "kraken.retrieve-export".into(),
                json!({}),
                &path
            )
            .await
            .is_err()
        );
        assert_eq!(std::fs::read(&target).unwrap(), b"original");
    }
}
