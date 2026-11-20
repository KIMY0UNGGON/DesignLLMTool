//! RFC 4648 base64: the standard alphabet, padded, no line breaks. The one implementation for embedded asset payloads,
//! `asset.read` chunks, and data URIs. Decoding is strict, and its errors are neutral: each caller turns them into its own
//! error code and text.

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Length of the padded text that encodes `bytes` bytes, saturating at `u64::MAX`.
pub fn encoded_len(bytes: u64) -> u64 {
    bytes.div_ceil(3).saturating_mul(4)
}

/// The padded text of `bytes`.
pub fn encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    encode_into(bytes, &mut out);
    out
}

/// Appends the padded text of `bytes` to `out`.
pub fn encode_into(bytes: &[u8], out: &mut String) {
    for chunk in bytes.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = u32::from(chunk.get(1).copied().unwrap_or(0));
        let b2 = u32::from(chunk.get(2).copied().unwrap_or(0));
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(char::from(ALPHABET[(n >> 18) as usize & 63]));
        out.push(char::from(ALPHABET[(n >> 12) as usize & 63]));
        out.push(if chunk.len() > 1 { char::from(ALPHABET[(n >> 6) as usize & 63]) } else { '=' });
        out.push(if chunk.len() > 2 { char::from(ALPHABET[n as usize & 63]) } else { '=' });
    }
}

/// Why a text is not strict padded base64. Checked per quartet, in text order: the length first, then each character.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeError {
    /// The text length is not a multiple of 4.
    Length,
    /// `=` outside the last two places of the last quartet.
    MisplacedPadding,
    /// A character after `=` in the same quartet.
    DataAfterPadding,
    /// A byte outside the standard alphabet (this includes `-`, `_`, white space, and non-ASCII text).
    InvalidCharacter,
}

/// Decodes strict padded base64. Bits left over in the last quartet are ignored, as RFC 4648 section 3.5 allows.
pub fn decode(text: &str) -> Result<Vec<u8>, DecodeError> {
    let bytes = text.as_bytes();
    if bytes.len() % 4 != 0 {
        return Err(DecodeError::Length);
    }
    let quads = bytes.len() / 4;
    let mut out = Vec::with_capacity(quads * 3);
    for (index, quad) in bytes.chunks_exact(4).enumerate() {
        let mut vals = [0u32; 4];
        let mut pad = 0usize;
        for (i, &c) in quad.iter().enumerate() {
            if c == b'=' {
                if index + 1 != quads || i < 2 {
                    return Err(DecodeError::MisplacedPadding);
                }
                pad += 1;
                continue;
            }
            if pad > 0 {
                return Err(DecodeError::DataAfterPadding);
            }
            vals[i] = match c {
                b'A'..=b'Z' => u32::from(c - b'A'),
                b'a'..=b'z' => u32::from(c - b'a') + 26,
                b'0'..=b'9' => u32::from(c - b'0') + 52,
                b'+' => 62,
                b'/' => 63,
                _ => return Err(DecodeError::InvalidCharacter),
            };
        }
        let n = (vals[0] << 18) | (vals[1] << 12) | (vals[2] << 6) | vals[3];
        out.push((n >> 16) as u8);
        if pad < 2 {
            out.push((n >> 8) as u8);
        }
        if pad < 1 {
            out.push(n as u8);
        }
    }
    Ok(out)
}

/// Why an embedded payload does not match its declared reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PayloadError {
    /// The text is not strict base64.
    Base64(DecodeError),
    /// The text decodes to `actual` bytes, not the declared length.
    Length { actual: usize },
    /// The decoded bytes have the declared length and another sha256.
    Hash,
}

/// Decodes an embedded payload and checks it against the declared length and then the declared sha256 (lowercase hex).
pub fn decode_verified(text: &str, declared_bytes: u64, declared_sha256: &str) -> Result<Vec<u8>, PayloadError> {
    let data = decode(text).map_err(PayloadError::Base64)?;
    if data.len() as u64 != declared_bytes {
        return Err(PayloadError::Length { actual: data.len() });
    }
    if crate::sha256(&data) != declared_sha256 {
        return Err(PayloadError::Hash);
    }
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    const RFC_4648: [(&str, &str); 7] =
        [("", ""), ("f", "Zg=="), ("fo", "Zm8="), ("foo", "Zm9v"), ("foob", "Zm9vYg=="), ("fooba", "Zm9vYmE="), ("foobar", "Zm9vYmFy")];

    #[test]
    fn rfc_4648_vectors_encode_append_and_decode() {
        for (plain, text) in RFC_4648 {
            assert_eq!(encode(plain.as_bytes()), text);
            let mut appended = String::from("x");
            encode_into(plain.as_bytes(), &mut appended);
            assert_eq!(appended, format!("x{text}"));
            assert_eq!(decode(text).unwrap(), plain.as_bytes());
            assert_eq!(encoded_len(plain.len() as u64), text.len() as u64);
        }
        assert_eq!(encode(&[0xff, 0xfe, 0xfd, 0x00]), "//79AA==");
        assert_eq!(encode(b"\xfb\xff"), "+/8=");
        assert_eq!(encoded_len(u64::MAX), u64::MAX);
    }

    #[test]
    fn every_byte_value_round_trips() {
        let all: Vec<u8> = (0..=255).collect();
        for end in 0..all.len() {
            assert_eq!(decode(&encode(&all[..end])).unwrap(), &all[..end]);
        }
    }

    #[test]
    fn malformed_text_names_the_first_problem() {
        let cases = [
            ("Zm9", DecodeError::Length),
            ("Zm9vYmFy\n", DecodeError::Length),
            ("Zm9vYmFy=", DecodeError::Length),
            ("=m9vYmFy", DecodeError::MisplacedPadding),
            ("Zm=vYmFy", DecodeError::MisplacedPadding),
            ("Zg==Zg==", DecodeError::MisplacedPadding),
            ("Z===", DecodeError::MisplacedPadding),
            ("Zm9vYm=y", DecodeError::DataAfterPadding),
            ("Zg=A", DecodeError::DataAfterPadding),
            ("-_8=", DecodeError::InvalidCharacter),
            ("Zm9v mFy", DecodeError::InvalidCharacter),
            ("Zm9*", DecodeError::InvalidCharacter),
            ("\u{ac00}A", DecodeError::InvalidCharacter),
        ];
        for (text, want) in cases {
            assert_eq!(decode(text), Err(want), "{text:?}");
        }
        assert_eq!(decode("Zm9vYm==").unwrap(), b"foob", "leftover bits in the last quartet are ignored");
    }

    #[test]
    fn verified_payload_checks_base64_then_length_then_hash() {
        let hash = crate::sha256(b"foobar");
        assert_eq!(decode_verified("Zm9vYmFy", 6, &hash).unwrap(), b"foobar");
        assert_eq!(decode_verified("Zm9vYmF*", 6, &hash), Err(PayloadError::Base64(DecodeError::InvalidCharacter)));
        assert_eq!(decode_verified("Zm9vYg==", 6, &hash), Err(PayloadError::Length { actual: 4 }));
        assert_eq!(decode_verified("Zm9vYmFx", 6, &hash), Err(PayloadError::Hash));
        assert_eq!(decode_verified("", 0, &crate::sha256(b"")).unwrap(), b"");
    }
}
