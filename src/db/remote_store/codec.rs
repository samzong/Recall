use std::io::{Read, Write};

use anyhow::{Result, ensure};

use super::{OBJECT_LIMIT, ZSTD_LEVEL};

const WINDOW_LOG_MAX: u32 = 31;

pub(crate) fn decode_zstd(body: &[u8], prefix: Option<&[u8]>) -> Result<Vec<u8>> {
    let mut decoder = match prefix {
        Some(prefix) => zstd::stream::read::Decoder::with_ref_prefix(body, prefix)?,
        None => zstd::stream::read::Decoder::with_buffer(body)?,
    };
    decoder.window_log_max(WINDOW_LOG_MAX)?;
    let mut raw = Vec::new();
    decoder.take(OBJECT_LIMIT as u64 + 1).read_to_end(&mut raw)?;
    ensure!(raw.len() <= OBJECT_LIMIT, "remote object exceeds 512 MiB");
    Ok(raw)
}

pub(crate) fn encode_delta(raw: &[u8], prefix: &[u8]) -> Result<Vec<u8>> {
    let span = (prefix.len() + raw.len()).max(1) as u64;
    let window = (u64::BITS - (span - 1).leading_zeros()).clamp(10, 30);
    let mut encoder =
        zstd::stream::write::Encoder::with_ref_prefix(Vec::new(), ZSTD_LEVEL, prefix)?;
    encoder.window_log(window)?;
    encoder.long_distance_matching(true)?;
    encoder.set_pledged_src_size(Some(raw.len() as u64))?;
    encoder.include_contentsize(true)?;
    encoder.write_all(raw)?;
    Ok(encoder.finish()?)
}
