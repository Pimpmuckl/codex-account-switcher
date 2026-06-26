use std::io::{Read, Write};

use anyhow::{Context, Result, bail};
use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;

use crate::model::SnapshotBlob;

pub(super) const SNAPSHOT_ENCODING_V1_MAGIC: &[u8] = b"cas-snapshot-v1\n";

pub(super) fn encode_snapshot(snapshot: &SnapshotBlob) -> Result<Vec<u8>> {
    let serialized = serde_json::to_vec(snapshot).context("failed to serialize snapshot")?;
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder
        .write_all(&serialized)
        .context("failed to compress snapshot")?;
    let compressed = encoder.finish().context("failed to finalize snapshot")?;
    let mut encoded = Vec::with_capacity(SNAPSHOT_ENCODING_V1_MAGIC.len() + compressed.len());
    encoded.extend_from_slice(SNAPSHOT_ENCODING_V1_MAGIC);
    encoded.extend_from_slice(&compressed);
    Ok(encoded)
}

pub(super) fn decode_snapshot(encoded: &[u8]) -> Result<SnapshotBlob> {
    let Some(compressed) = encoded.strip_prefix(SNAPSHOT_ENCODING_V1_MAGIC) else {
        bail!("unsupported stored snapshot encoding; re-save that account");
    };
    let mut decoder = GzDecoder::new(compressed);
    let mut serialized = Vec::new();
    decoder
        .read_to_end(&mut serialized)
        .context("failed to decompress stored snapshot")?;
    serde_json::from_slice(&serialized).context("failed to parse stored snapshot")
}
