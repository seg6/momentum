//! Sennheiser SPP framing for GAIA packets.
//!
//! Wire format on RFCOMM channel 1:
//!
//! ```text
//! +------+---------+-------+----------------+-------------------+
//! | 0xff | version | flags | payload length | GAIA packet (≥4B) |
//! +------+---------+-------+----------------+-------------------+
//! ```
//!
//! - `version` is one of `{1, 3, 4}` in observed firmware.
//! - `flags`'s `0x02` bit selects 16-bit payload length when set.
//! - `payload length` does **not** include the 4-byte GAIA header
//!   (`vendor_id_be:u16 || command_id_be:u16`). The full GAIA packet that
//!   follows is `4 + payload_length` bytes long.
//! - We only emit `version=3, flags=0, 8-bit length` frames; we accept all
//!   variants on the parse side.

use crate::{Error, Result};

const MARKER: u8 = 0xff;
const WRITE_VERSION: u8 = 0x03;
const WRITE_FLAGS: u8 = 0x00;
const FLAG_LONG_LENGTH: u8 = 0x02;
const SUPPORTED_FLAGS: u8 = 0x03;
const GAIA_HEADER_LEN: usize = 4;
const SHORT_HEADER_LEN: usize = 4;
const LONG_HEADER_LEN: usize = 5;
const MAX_SHORT_PAYLOAD: usize = u8::MAX as usize;

const fn is_supported_version(version: u8) -> bool {
    matches!(version, 1 | 3 | 4)
}

/// Frame a GAIA packet for SPP transmission, writing into `out`.
///
/// `out` is appended to (not cleared) so it can be reused across calls.
pub fn write_frame(packet: &[u8], out: &mut Vec<u8>) -> Result<()> {
    if packet.len() < GAIA_HEADER_LEN {
        return Err(Error::ShortPacket);
    }
    let payload_len = packet.len() - GAIA_HEADER_LEN;
    if payload_len > MAX_SHORT_PAYLOAD {
        return Err(Error::FrameTooLarge(payload_len));
    }
    out.reserve(SHORT_HEADER_LEN + packet.len());
    out.extend_from_slice(&[MARKER, WRITE_VERSION, WRITE_FLAGS, payload_len as u8]);
    out.extend_from_slice(packet);
    Ok(())
}

/// Parse one complete SPP frame at the start of `data` and return the GAIA
/// payload. Returns the number of bytes consumed.
pub fn parse_frame(data: &[u8]) -> Result<(Vec<u8>, usize)> {
    let header = parse_header(data)?;
    let HeaderInfo {
        payload_start,
        payload_len,
        frame_len,
    } = header;
    if data.len() < frame_len {
        return Err(Error::IncompleteFrame);
    }
    Ok((
        data[payload_start..payload_start + payload_len].to_vec(),
        frame_len,
    ))
}

/// Pull one complete frame out of a streaming buffer.
///
/// - `Ok(Some(payload))` when a full frame was decoded and drained.
/// - `Ok(None)` when more bytes are needed.
/// - `Err(_)` on protocol violations; the buffer is left untouched so the
///   caller can decide how to recover.
pub fn extract_frame(buffer: &mut Vec<u8>) -> Result<Option<Vec<u8>>> {
    let Some(start) = buffer.iter().position(|byte| *byte == MARKER) else {
        buffer.clear();
        return Ok(None);
    };
    if start > 0 {
        buffer.drain(..start);
    }
    let header = match parse_header(buffer) {
        Ok(header) => header,
        Err(Error::ShortFrame | Error::IncompleteFrame) => return Ok(None),
        Err(Error::UnsupportedFrameVersion(_)) => {
            // Skip this marker byte and try again next call.
            buffer.drain(..1);
            return Ok(None);
        }
        Err(other) => return Err(other),
    };
    if buffer.len() < header.frame_len {
        return Ok(None);
    }
    let payload = buffer[header.payload_start..header.payload_start + header.payload_len].to_vec();
    buffer.drain(..header.frame_len);
    Ok(Some(payload))
}

struct HeaderInfo {
    payload_start: usize,
    payload_len: usize,
    frame_len: usize,
}

fn parse_header(data: &[u8]) -> Result<HeaderInfo> {
    if data.is_empty() || data[0] != MARKER {
        return Err(Error::MissingFrameMarker);
    }
    if data.len() < SHORT_HEADER_LEN {
        return Err(Error::ShortFrame);
    }
    let version = data[1];
    let flags = data[2];
    if !is_supported_version(version) {
        return Err(Error::UnsupportedFrameVersion(version));
    }
    if flags & !SUPPORTED_FLAGS != 0 {
        return Err(Error::UnsupportedFrameFlags(flags));
    }

    let (payload_len, payload_start) = if flags & FLAG_LONG_LENGTH == 0 {
        (data[3] as usize, SHORT_HEADER_LEN)
    } else {
        if data.len() < LONG_HEADER_LEN {
            return Err(Error::ShortFrame);
        }
        (
            u16::from_be_bytes([data[3], data[4]]) as usize,
            LONG_HEADER_LEN,
        )
    };

    // GAIA packet that follows is `4 (vendor+command) + payload_len` bytes.
    let gaia_len = GAIA_HEADER_LEN + payload_len;
    Ok(HeaderInfo {
        payload_start,
        payload_len: gaia_len,
        frame_len: payload_start + gaia_len,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_and_parse_round_trip() {
        let packet = [0x04, 0x95, 0x1a, 0x05];
        let mut framed = Vec::new();
        write_frame(&packet, &mut framed).unwrap();
        assert_eq!(framed, [0xff, 0x03, 0x00, 0x00, 0x04, 0x95, 0x1a, 0x05]);
        let (payload, used) = parse_frame(&framed).unwrap();
        assert_eq!(payload, packet);
        assert_eq!(used, framed.len());
    }

    #[test]
    fn write_frame_with_params() {
        let packet = [0x04, 0x95, 0x1a, 0x04, 0x01];
        let mut framed = Vec::new();
        write_frame(&packet, &mut framed).unwrap();
        assert_eq!(
            framed,
            [0xff, 0x03, 0x00, 0x01, 0x04, 0x95, 0x1a, 0x04, 0x01]
        );
        let (payload, used) = parse_frame(&framed).unwrap();
        assert_eq!(payload, packet);
        assert_eq!(used, framed.len());
    }

    #[test]
    fn extract_resyncs_on_garbage_prefix() {
        let mut buf = vec![0x00, 0x00, 0xff, 0x03, 0x00, 0x00, 0x04, 0x95, 0x1a, 0x05];
        let payload = extract_frame(&mut buf).unwrap().unwrap();
        assert_eq!(payload, [0x04, 0x95, 0x1a, 0x05]);
        assert!(buf.is_empty());
    }

    #[test]
    fn extract_returns_none_on_partial_frame() {
        let mut buf = vec![0xff, 0x03, 0x00, 0x01, 0x04, 0x95];
        assert!(extract_frame(&mut buf).unwrap().is_none());
        assert_eq!(buf, vec![0xff, 0x03, 0x00, 0x01, 0x04, 0x95]);
    }
}
