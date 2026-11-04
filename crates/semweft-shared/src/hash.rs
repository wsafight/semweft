/// Stable BLAKE3 content hash, encoded as lowercase hexadecimal.
pub fn digest(bytes: impl AsRef<[u8]>) -> String {
    blake3::hash(bytes.as_ref()).to_hex().to_string()
}

/// Hash of a value's compact JSON form without materializing the JSON text.
/// Equals `digest(serde_json::to_vec(value))`.
pub fn digest_json<T: serde::Serialize + ?Sized>(value: &T) -> serde_json::Result<String> {
    struct Sink(blake3::Hasher);
    impl std::io::Write for Sink {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.update(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut sink = Sink(blake3::Hasher::new());
    serde_json::to_writer(&mut sink, value)?;
    Ok(sink.0.finalize().to_hex().to_string())
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
    fn streamed_json_digest_equals_hashing_the_serialized_bytes() {
        let value = serde_json::json!({"a":[1,2,{"b":"c\u{e9}"}],"z":null});
        assert_eq!(
            digest_json(&value).unwrap(),
            digest(serde_json::to_vec(&value).unwrap())
        );
        let tuple = ("file", vec![("x", 1u8)], 2.5f64);
        assert_eq!(
            digest_json(&tuple).unwrap(),
            digest(serde_json::to_vec(&tuple).unwrap())
        );
        // Large enough to cross the writer's internal chunk boundaries.
        let big = vec!["0123456789abcdef"; 50_000];
        assert_eq!(
            digest_json(&big).unwrap(),
            digest(serde_json::to_vec(&big).unwrap())
        );
        assert_ne!(digest_json(&1).unwrap(), digest_json(&2).unwrap());
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
