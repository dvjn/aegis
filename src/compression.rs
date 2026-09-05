use flate2::read::GzDecoder;
use std::io::Read;

const MAX_DECOMPRESSED_BYTES: u64 = 32 * 1024 * 1024;

// RFC 1952 section 2.3.1.
const GZIP_MAGIC: [u8; 2] = [0x1f, 0x8b];
// RFC 8878 section 3.1.1.
const ZSTD_MAGIC: [u8; 4] = [0x28, 0xb5, 0x2f, 0xfd];

pub(crate) fn decode_body(body: &[u8]) -> Vec<u8> {
    let decoded = if body.starts_with(&GZIP_MAGIC) {
        read_bounded(GzDecoder::new(body))
    } else if body.starts_with(&ZSTD_MAGIC) {
        zstd::stream::read::Decoder::new(body)
            .ok()
            .and_then(read_bounded)
    } else {
        return body.to_vec();
    };
    decoded.unwrap_or_else(|| body.to_vec())
}

// Brotli defines no magic number, so it cannot be sniffed: RFC 7932.
pub(crate) fn decode_brotli_unsniffable(body: &[u8]) -> Option<Vec<u8>> {
    read_bounded(brotli::Decompressor::new(body, 4096))
}

pub(crate) fn decode_declared(encoding: &str, body: &[u8]) -> Option<Vec<u8>> {
    match encoding.trim().to_ascii_lowercase().as_str() {
        "" | "identity" => Some(body.to_vec()),
        "gzip" | "x-gzip" => read_bounded(GzDecoder::new(body)),
        "zstd" => zstd::stream::read::Decoder::new(body)
            .ok()
            .and_then(read_bounded),
        "br" => decode_brotli_unsniffable(body),
        _ => None,
    }
}

fn read_bounded(reader: impl Read) -> Option<Vec<u8>> {
    let mut decoded = Vec::new();
    let one_past_the_cap = MAX_DECOMPRESSED_BYTES + 1;
    reader
        .take(one_past_the_cap)
        .read_to_end(&mut decoded)
        .ok()?;
    (decoded.len() as u64 <= MAX_DECOMPRESSED_BYTES).then_some(decoded)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn plain_bodies_pass_through() {
        assert_eq!(decode_body(b"{\"a\":1}"), b"{\"a\":1}");
        assert_eq!(decode_body(b""), b"");
    }

    #[test]
    fn gzip_and_zstd_bodies_round_trip() {
        assert_eq!(decode_body(&gzip(b"hello")), b"hello");
        assert_eq!(
            decode_body(&zstd::encode_all(&b"hello"[..], 0).unwrap()),
            b"hello"
        );
    }

    #[test]
    fn a_body_that_only_looks_compressed_is_returned_unchanged() {
        let bogus = [0x28, 0xb5, 0x2f, 0xfd, 0x00, 0x01];
        assert_eq!(decode_body(&bogus), bogus);
    }

    #[test]
    fn a_decompression_bomb_is_rejected() {
        let bomb = zstd::encode_all(
            vec![0u8; MAX_DECOMPRESSED_BYTES as usize + 1024].as_slice(),
            0,
        )
        .unwrap();
        assert!(
            bomb.len() < 1024 * 1024,
            "the fixture must stay small so the cap, not the input size, is what rejects it"
        );
        assert_eq!(decode_body(&bomb), bomb, "an oversized decode is refused");
    }

    pub(crate) fn brotli(body: &[u8]) -> Vec<u8> {
        let mut encoded = Vec::new();
        brotli::BrotliCompress(&mut &body[..], &mut encoded, &Default::default()).unwrap();
        encoded
    }

    pub(crate) fn gzip(body: &[u8]) -> Vec<u8> {
        use flate2::{Compression, write::GzEncoder};
        use std::io::Write;

        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(body).unwrap();
        encoder.finish().unwrap()
    }
}
