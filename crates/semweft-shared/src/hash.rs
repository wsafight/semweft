/// Stable BLAKE3 content hash, encoded as lowercase hexadecimal.
pub fn digest(bytes: impl AsRef<[u8]>) -> String {
    blake3::hash(bytes.as_ref()).to_hex().to_string()
}

/// Namespaced identity with unambiguous framing between string components.
pub fn identity(prefix: &str, parts: &[&str]) -> String {
    format!("{prefix}_{}", digest(serde_json::to_vec(parts).unwrap()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_hashes_preserve_the_persisted_format() {
        assert_eq!(
            digest("abc"),
            "6437b3ac38465133ffb63b75273a8db548c558465d79db03fd359c6cd5bd9d85"
        );
        assert_eq!(digest("same"), digest(String::from("same")));
        assert_ne!(digest(""), digest("a"));
    }

    #[test]
    fn identities_do_not_collide_across_partitions() {
        assert_eq!(identity("sym", &["a", "b"]), identity("sym", &["a", "b"]));
        assert_ne!(identity("sym", &["a", "b"]), identity("sym", &["ab"]));
        assert_ne!(identity("sym", &["a::b"]), identity("sym", &["a", "b"]));
        assert_ne!(identity("sym", &["a"]), identity("other", &["a"]));
        assert_eq!(
            identity("sym", &["a", "b"]),
            format!("sym_{}", digest(br#"["a","b"]"#))
        );
    }
}
