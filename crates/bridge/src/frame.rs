//! Internal IPC framing: a 4-byte little-endian payload length, then one UTF-8 JSON `IpcMessage`.
//!
//! The 8 MiB limit (`MAX_FRAME_BYTES`) applies to the payload only and is checked on the header
//! before any payload byte is buffered. A header above the limit cannot be resynchronised, so
//! callers must answer once and close the connection.

use std::io::{ErrorKind, Read, Write};

use designllm_contracts::{DltError, DltResult, ErrorCode, MAX_FRAME_BYTES};

/// Length header size in bytes.
pub const HEADER_BYTES: usize = 4;

fn frame_error(message: impl Into<String>) -> DltError {
    DltError::new(ErrorCode::InvalidArgument, message)
}

fn check_length(len: usize) -> DltResult<()> {
    if len == 0 {
        return Err(frame_error("zero-length frame is not allowed"));
    }
    if len > MAX_FRAME_BYTES {
        return Err(frame_error(format!(
            "frame of {len} bytes exceeds the {MAX_FRAME_BYTES}-byte limit; connection will be closed"
        )));
    }
    Ok(())
}

fn check_utf8(payload: &[u8]) -> DltResult<()> {
    std::str::from_utf8(payload)
        .map(|_| ())
        .map_err(|_| frame_error("frame payload is not valid UTF-8"))
}

/// Builds header + payload. Refuses empty, oversized, and non-UTF-8 payloads before producing bytes.
pub fn encode_frame(payload: &[u8]) -> DltResult<Vec<u8>> {
    check_length(payload.len())?;
    check_utf8(payload)?;
    let mut out = Vec::with_capacity(HEADER_BYTES + payload.len());
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(payload);
    Ok(out)
}

/// Writes one frame and flushes. Nothing is written when the payload is refused.
pub fn write_frame<W: Write>(writer: &mut W, payload: &[u8]) -> DltResult<()> {
    let bytes = encode_frame(payload)?;
    writer.write_all(&bytes)?;
    writer.flush()?;
    Ok(())
}

/// Blocking read of one frame. `Ok(None)` is a clean end of stream at a frame boundary.
pub fn read_frame<R: Read>(reader: &mut R) -> DltResult<Option<Vec<u8>>> {
    let mut header = [0u8; HEADER_BYTES];
    let mut got = 0usize;
    while got < HEADER_BYTES {
        match reader.read(&mut header[got..]) {
            Ok(0) => {
                if got == 0 {
                    return Ok(None);
                }
                return Err(DltError::new(ErrorCode::Io, "stream ended inside a frame header"));
            }
            Ok(n) => got += n,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        }
    }
    let len = u32::from_le_bytes(header) as usize;
    check_length(len)?;
    let mut payload = vec![0u8; len];
    reader.read_exact(&mut payload)?;
    check_utf8(&payload)?;
    Ok(Some(payload))
}

/// Incremental decoder for transports that deliver bytes in arbitrary chunks (overlapped reads).
///
/// `push` returns every complete frame. A bad header is an error: the stream is then unusable and
/// the caller must close it.
#[derive(Debug, Default)]
pub struct FrameDecoder {
    buffer: Vec<u8>,
}

impl FrameDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends bytes and returns the complete payloads now available, in order.
    pub fn push(&mut self, bytes: &[u8]) -> DltResult<Vec<Vec<u8>>> {
        self.buffer.extend_from_slice(bytes);
        let mut frames = Vec::new();
        let mut offset = 0usize;
        loop {
            let rest = &self.buffer[offset..];
            if rest.len() < HEADER_BYTES {
                break;
            }
            let len = u32::from_le_bytes([rest[0], rest[1], rest[2], rest[3]]) as usize;
            check_length(len)?;
            if rest.len() < HEADER_BYTES + len {
                break;
            }
            let payload = &rest[HEADER_BYTES..HEADER_BYTES + len];
            check_utf8(payload)?;
            frames.push(payload.to_vec());
            offset += HEADER_BYTES + len;
        }
        self.buffer.drain(..offset);
        Ok(frames)
    }

    /// Bytes received but not yet forming a complete frame.
    pub fn buffered_bytes(&self) -> usize {
        self.buffer.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn max_payload() -> Vec<u8> {
        let mut p = b"{\"x\":\"".to_vec();
        p.resize(MAX_FRAME_BYTES - 2, b'a');
        p.extend_from_slice(b"\"}");
        p
    }

    #[test]
    fn header_is_little_endian_payload_length_without_itself() {
        let bytes = encode_frame(b"{}").expect("encode");
        assert_eq!(bytes, vec![2, 0, 0, 0, b'{', b'}']);
    }

    #[test]
    fn exact_limit_is_accepted_and_one_byte_over_is_refused() {
        let payload = max_payload();
        assert_eq!(payload.len(), MAX_FRAME_BYTES);
        let mut out = Vec::new();
        write_frame(&mut out, &payload).expect("8 MiB payload is allowed");
        assert_eq!(&out[..4], &(MAX_FRAME_BYTES as u32).to_le_bytes());
        let back = read_frame(&mut Cursor::new(out)).expect("read").expect("one frame");
        assert_eq!(back, payload);

        let mut over = vec![b'a'; MAX_FRAME_BYTES + 1];
        over[0] = b'{';
        let mut sink = Vec::new();
        assert!(write_frame(&mut sink, &over).is_err());
        assert!(sink.is_empty(), "a refused payload must not produce bytes");
    }

    #[test]
    fn over_limit_header_is_refused_without_reading_the_body() {
        let header = ((MAX_FRAME_BYTES as u32) + 1).to_le_bytes();
        let err = read_frame(&mut Cursor::new(header.to_vec())).expect_err("refused");
        assert_eq!(err.code, ErrorCode::InvalidArgument);
        assert!(err.message.contains("exceeds"));
    }

    #[test]
    fn zero_length_and_invalid_utf8_are_refused_both_ways() {
        assert!(encode_frame(b"").is_err());
        assert!(read_frame(&mut Cursor::new(vec![0u8, 0, 0, 0])).is_err());
        assert!(encode_frame(&[0xFF]).is_err());
        assert!(read_frame(&mut Cursor::new(vec![1u8, 0, 0, 0, 0xFF])).is_err());
    }

    #[test]
    fn truncated_frames_are_io_errors_and_clean_eof_is_none() {
        assert!(read_frame(&mut Cursor::new(Vec::<u8>::new())).expect("clean").is_none());
        assert_eq!(read_frame(&mut Cursor::new(vec![10u8, 0])).unwrap_err().code, ErrorCode::Io);
        assert_eq!(read_frame(&mut Cursor::new(vec![10u8, 0, 0, 0, b'{'])).unwrap_err().code, ErrorCode::Io);
    }

    #[test]
    fn decoder_handles_split_and_coalesced_chunks() {
        let mut wire = encode_frame(br#"{"a":1}"#).unwrap();
        wire.extend(encode_frame(br#"{"b":2}"#).unwrap());
        let mut decoder = FrameDecoder::new();
        let mut frames = Vec::new();
        for byte in &wire {
            frames.extend(decoder.push(&[*byte]).expect("byte-wise push"));
        }
        assert_eq!(frames, vec![br#"{"a":1}"#.to_vec(), br#"{"b":2}"#.to_vec()]);
        assert_eq!(decoder.buffered_bytes(), 0);
    }

    #[test]
    fn decoder_refuses_oversized_header_before_buffering_the_body() {
        let mut decoder = FrameDecoder::new();
        let header = ((MAX_FRAME_BYTES as u32) + 1).to_le_bytes();
        assert!(decoder.push(&header).is_err());
        assert_eq!(decoder.buffered_bytes(), 4, "no body bytes were accumulated");
    }
}
