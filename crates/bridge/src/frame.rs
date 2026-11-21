//! Internal IPC framing: a 4-byte little-endian payload length, then one UTF-8 JSON `IpcMessage`.
//!
//! The 8 MiB limit (`MAX_FRAME_BYTES`) applies to the payload only and is checked on the header
//! before any payload byte is buffered. A header above the limit cannot be resynchronised, so
//! callers must answer once and close the connection.
//!
//! A binary frame (68i) sets bit 31 of the header ([`BINARY_FLAG`]); its payload is raw bytes under the same limit. The host
//! sends binary frames only to a gui connection that asked `render.pixels`, right before that request's response, which they
//! belong to. Only a decoder made with [`FrameDecoder::with_binary`] accepts them; any other refuses the header.

use std::io::{ErrorKind, Read, Write};

use designllm_contracts::{DltError, DltResult, ErrorCode, MAX_FRAME_BYTES};

/// Length header size in bytes.
pub const HEADER_BYTES: usize = 4;
/// Header bit of a binary frame. The length is the header without it.
pub const BINARY_FLAG: u32 = 1 << 31;

/// One frame payload of a decoder that accepts binary frames.
#[derive(Debug, PartialEq, Eq)]
pub enum Frame {
    Json(Vec<u8>),
    Binary(Vec<u8>),
}

/// The header of a binary frame of `len` bytes. Refuses an empty or oversized payload.
pub fn binary_header(len: usize) -> DltResult<[u8; HEADER_BYTES]> {
    check_length(len)?;
    Ok((len as u32 | BINARY_FLAG).to_le_bytes())
}

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
    binary: bool,
    /// A binary frame whose body is still arriving, and its length: its bytes go straight into it, so a large frame is copied
    /// once instead of through `buffer` (68i).
    body: Option<(Vec<u8>, usize)>,
}

impl FrameDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// A decoder that also accepts binary frames ([`FrameDecoder::push_frames`]).
    pub fn with_binary() -> Self {
        FrameDecoder { buffer: Vec::new(), binary: true, body: None }
    }

    /// Appends bytes and returns the complete JSON payloads now available, in order. A binary frame is an error.
    pub fn push(&mut self, bytes: &[u8]) -> DltResult<Vec<Vec<u8>>> {
        self.push_frames(bytes)?
            .into_iter()
            .map(|frame| match frame {
                Frame::Json(payload) => Ok(payload),
                Frame::Binary(_) => Err(frame_error("binary frame on a JSON-only stream")),
            })
            .collect()
    }

    /// Appends bytes and returns the complete frames now available, in order. A binary header is refused before its body
    /// is buffered unless the decoder was made [`FrameDecoder::with_binary`].
    pub fn push_frames(&mut self, mut bytes: &[u8]) -> DltResult<Vec<Frame>> {
        let mut frames = Vec::new();
        if let Some((body, len)) = self.body.as_mut() {
            let take = (*len - body.len()).min(bytes.len());
            body.extend_from_slice(&bytes[..take]);
            bytes = &bytes[take..];
            if body.len() < *len {
                return Ok(frames);
            }
            if let Some((body, _)) = self.body.take() {
                frames.push(Frame::Binary(body));
            }
        }
        self.buffer.extend_from_slice(bytes);
        let mut offset = 0usize;
        loop {
            let rest = &self.buffer[offset..];
            if rest.len() < HEADER_BYTES {
                break;
            }
            let header = u32::from_le_bytes([rest[0], rest[1], rest[2], rest[3]]);
            let binary = header & BINARY_FLAG != 0;
            if binary && !self.binary {
                return Err(frame_error("binary frames are not accepted on this stream; connection will be closed"));
            }
            let len = (header & !BINARY_FLAG) as usize;
            check_length(len)?;
            if rest.len() < HEADER_BYTES + len {
                if binary && rest.len() > HEADER_BYTES {
                    let mut body = Vec::with_capacity(len);
                    body.extend_from_slice(&rest[HEADER_BYTES..]);
                    self.body = Some((body, len));
                    offset = self.buffer.len();
                }
                break;
            }
            let payload = &rest[HEADER_BYTES..HEADER_BYTES + len];
            if binary {
                frames.push(Frame::Binary(payload.to_vec()));
            } else {
                check_utf8(payload)?;
                frames.push(Frame::Json(payload.to_vec()));
            }
            offset += HEADER_BYTES + len;
        }
        self.buffer.drain(..offset);
        Ok(frames)
    }

    /// Bytes received but not yet forming a complete frame.
    pub fn buffered_bytes(&self) -> usize {
        self.buffer.len() + self.body.as_ref().map_or(0, |(body, _)| body.len())
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
    fn binary_frames_reach_only_a_decoder_that_accepts_them() {
        let mut wire = binary_header(3).unwrap().to_vec();
        wire.extend_from_slice(&[0xFF, 0, 1]);
        wire.extend(encode_frame(br#"{"a":1}"#).unwrap());
        let mut decoder = FrameDecoder::with_binary();
        let mut frames = Vec::new();
        for byte in &wire {
            frames.extend(decoder.push_frames(&[*byte]).expect("byte-wise push"));
        }
        assert_eq!(frames, vec![Frame::Binary(vec![0xFF, 0, 1]), Frame::Json(br#"{"a":1}"#.to_vec())]);
        // A body filled straight into its own buffer, with pushes that end it and start the next frame at once.
        let big: Vec<u8> = (0..1000u32).map(|n| n as u8).collect();
        let mut stream = encode_frame(br#"{"a":0}"#).unwrap();
        stream.extend(binary_header(big.len()).unwrap());
        stream.extend(&big);
        stream.extend(binary_header(2).unwrap());
        stream.extend([7, 8]);
        stream.extend(encode_frame(br#"{"a":1}"#).unwrap());
        let mut decoder = FrameDecoder::with_binary();
        let frames: Vec<Frame> = stream.chunks(7).flat_map(|piece| decoder.push_frames(piece).expect("push")).collect();
        assert_eq!(frames, vec![Frame::Json(br#"{"a":0}"#.to_vec()), Frame::Binary(big), Frame::Binary(vec![7, 8]), Frame::Json(br#"{"a":1}"#.to_vec())]);
        assert_eq!(decoder.buffered_bytes(), 0);
        let mut json_only = FrameDecoder::new();
        assert!(json_only.push(&wire[..HEADER_BYTES]).is_err(), "refused at the header");
        assert_eq!(json_only.buffered_bytes(), HEADER_BYTES, "no body bytes were accumulated");
        assert!(binary_header(0).is_err() && binary_header(MAX_FRAME_BYTES + 1).is_err());
        let mut over = FrameDecoder::with_binary();
        assert!(over.push_frames(&((MAX_FRAME_BYTES as u32 + 1) | BINARY_FLAG).to_le_bytes()).is_err());
    }

    #[test]
    fn decoder_refuses_oversized_header_before_buffering_the_body() {
        let mut decoder = FrameDecoder::new();
        let header = ((MAX_FRAME_BYTES as u32) + 1).to_le_bytes();
        assert!(decoder.push(&header).is_err());
        assert_eq!(decoder.buffered_bytes(), 4, "no body bytes were accumulated");
    }
}
