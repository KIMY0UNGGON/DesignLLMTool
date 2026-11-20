//! RFC 4648 base64 (standard alphabet, padded) for `asset.read` chunks: the shared implementation in contracts.
pub use designllm_contracts::base64::encode;

#[cfg(test)]
mod tests {
    #[test]
    fn rfc4648_vectors() {
        for (input, expected) in [("", ""), ("f", "Zg=="), ("fo", "Zm8="), ("foo", "Zm9v"), ("foob", "Zm9vYg=="), ("fooba", "Zm9vYmE="), ("foobar", "Zm9vYmFy")] {
            assert_eq!(super::encode(input.as_bytes()), expected);
        }
        assert_eq!(super::encode(&[0xff, 0xfe, 0xfd, 0x00]), "//79AA==");
    }
}
