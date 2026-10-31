//! Versioned physical encoding; object identities always hash decoded bytes.
use anyhow::{Result, bail, ensure};
use serde::Serialize;
use std::time::Instant;
const MAGIC: &[u8] = b"SWO\xff\x01";
const HEADER: usize = 14;
const MAX_DECODED: usize = 512 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Codec {
    Raw,
    Lz4,
    Deflate,
}

pub fn encode(bytes: &[u8], codec: Codec) -> Vec<u8> {
    if matches!(codec, Codec::Raw) {
        return bytes.to_vec();
    }
    let payload = match codec {
        Codec::Lz4 => lz4_flex::block::compress(bytes),
        Codec::Deflate => miniz_oxide::deflate::compress_to_vec(bytes, 6),
        Codec::Raw => unreachable!(),
    };
    let mut encoded = Vec::with_capacity(HEADER + payload.len());
    encoded.extend_from_slice(MAGIC);
    encoded.push(if matches!(codec, Codec::Lz4) { 1 } else { 2 });
    encoded.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
    encoded.extend_from_slice(&payload);
    encoded
}

pub fn decode(bytes: &[u8]) -> Result<Vec<u8>> {
    if !bytes.starts_with(b"SWO\xff") {
        return Ok(bytes.to_vec());
    }
    ensure!(
        bytes.len() >= HEADER && bytes.starts_with(MAGIC),
        "index_store_error: invalid object encoding"
    );
    let length = u64::from_le_bytes(bytes[6..14].try_into().unwrap());
    ensure!(
        length <= MAX_DECODED as u64,
        "index_store_error: decoded object exceeds limit"
    );
    let length = length as usize;
    let decoded = match bytes[5] {
        1 => lz4_flex::block::decompress(&bytes[HEADER..], length)
            .map_err(|e| anyhow::anyhow!("index_store_error: invalid LZ4 object: {e}"))?,
        2 => miniz_oxide::inflate::decompress_to_vec_with_limit(&bytes[HEADER..], length)
            .map_err(|e| anyhow::anyhow!("index_store_error: invalid Deflate object: {e:?}"))?,
        _ => bail!("index_store_error: unknown object codec"),
    };
    ensure!(
        decoded.len() == length,
        "index_store_error: object length mismatch"
    );
    Ok(decoded)
}

pub(crate) fn preferred(bytes: &[u8]) -> Vec<u8> {
    if bytes.len() > MAX_DECODED {
        return bytes.to_vec();
    }
    let encoded = encode(bytes, Codec::Lz4);
    if encoded.len() + 32 < bytes.len() {
        encoded
    } else {
        bytes.to_vec()
    }
}

#[derive(Serialize)]
pub struct Measurement {
    pub codec: Codec,
    pub objects: usize,
    pub logical_bytes: usize,
    pub encoded_bytes: usize,
    pub encode_ms: f64,
    pub decode_ms: f64,
}
/// Same logical corpus for all codecs, including envelope overhead and roundtrip validation.
pub fn compare(objects: &[Vec<u8>]) -> Result<Vec<Measurement>> {
    [Codec::Raw, Codec::Lz4, Codec::Deflate]
        .into_iter()
        .map(|codec| {
            let mut result = Measurement {
                codec,
                objects: objects.len(),
                logical_bytes: 0,
                encoded_bytes: 0,
                encode_ms: 0.,
                decode_ms: 0.,
            };
            for bytes in objects {
                let start = Instant::now();
                let encoded = encode(bytes, codec);
                result.encode_ms += start.elapsed().as_secs_f64() * 1000.;
                let start = Instant::now();
                let decoded = decode(&encoded)?;
                result.decode_ms += start.elapsed().as_secs_f64() * 1000.;
                ensure!(
                    decoded == *bytes,
                    "index_store_error: codec roundtrip mismatch"
                );
                result.logical_bytes += bytes.len();
                result.encoded_bytes += encoded.len();
            }
            Ok(result)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn codecs_roundtrip_legacy_and_reject_bad_headers_lengths_and_payloads() {
        for bytes in [vec![], b"fn main() {}".to_vec(), vec![b'x'; 10000]] {
            for codec in [Codec::Raw, Codec::Lz4, Codec::Deflate] {
                assert_eq!(decode(&encode(&bytes, codec)).unwrap(), bytes);
            }
        }
        for codec in [Codec::Lz4, Codec::Deflate] {
            let encoded = encode(&vec![b'x'; 10000], codec);
            for end in 4..encoded.len() {
                assert!(decode(&encoded[..end]).is_err());
            }
            let mut encoded = encoded;
            encoded[6..14].copy_from_slice(&u64::MAX.to_le_bytes());
            assert!(decode(&encoded).is_err());
        }
    }
}
