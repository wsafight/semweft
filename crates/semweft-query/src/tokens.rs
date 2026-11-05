//! Offline token counting for the complete, compact JSON response.
use anyhow::Result;
use serde_json::{Value, json};
use tiktoken_rs::{cl100k_base_singleton, o200k_base_singleton};

pub const CL100K: &str = "cl100k_base@tiktoken-rs-0.12.1";
pub const O200K: &str = "o200k_base@tiktoken-rs-0.12.1";

pub fn supported(id: &str) -> bool {
    matches!(id, CL100K | O200K)
}

pub fn count(value: &Value) -> Result<u64> {
    let text = serde_json::to_string(value)?;
    Ok(match value["token_count"]["tokenizer"].as_str() {
        Some(CL100K) => cl100k_base_singleton().encode_ordinary(&text).len(),
        Some(O200K) => o200k_base_singleton().encode_ordinary(&text).len(),
        _ => text.len(),
    } as u64)
}

pub fn metadata(requested: &str, budget: u64) -> Value {
    let exact = supported(requested);
    json!({"mode":if exact {"exact"} else {"estimated"},
        "tokenizer":if exact {requested} else {"estimated"},"requested_tokenizer":requested,
        "method":if exact {"BPE over complete compact JSON, including metadata"} else {"compact JSON UTF-8 byte count"},
        "reason":if exact {"offline embedded vocabulary"} else if requested=="estimated" {"configured estimate"} else {"unsupported tokenizer; estimated fallback"},
        "total":0,"budget":budget})
}

pub fn finalize(value: &mut Value) -> Result<()> {
    // The count itself is serialized too. Most counts converge immediately;
    // neutral padding handles rare BPE cycles without excluding metadata.
    for padding in 0..32 {
        if padding > 0 {
            value["token_count"]["serialization_padding"] = json!(" ".repeat(padding));
        }
        for _ in 0..16 {
            let measured = count(value)?;
            if value["token_count"]["total"].as_u64() == Some(measured) {
                return Ok(());
            }
            value["token_count"]["total"] = json!(measured);
        }
    }
    anyhow::bail!("internal_error: serialized token count did not stabilize")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supported_tokenizers_match_the_embedded_vocabularies() {
        assert!(supported(CL100K));
        assert!(supported(O200K));
        assert!(!supported("estimated"));
        assert!(!supported("cl100k_base"));
        assert!(!supported("o200k_base@other"));
        assert!(!supported(""));
        assert!(CL100K.contains("tiktoken-rs-0.12.1"));
        assert!(O200K.contains("tiktoken-rs-0.12.1"));
    }

    #[test]
    fn metadata_distinguishes_exact_estimated_and_unsupported_modes() {
        let exact = metadata(CL100K, 4096);
        assert_eq!(exact["mode"], "exact");
        assert_eq!(exact["tokenizer"], CL100K);
        assert_eq!(exact["requested_tokenizer"], CL100K);
        assert_eq!(exact["budget"], 4096);
        assert_eq!(exact["total"], 0);

        let estimated = metadata("estimated", 1024);
        assert_eq!(estimated["mode"], "estimated");
        assert_eq!(estimated["tokenizer"], "estimated");
        assert_eq!(estimated["reason"], "configured estimate");
        assert!(estimated["method"].as_str().unwrap().contains("byte count"));

        let fallback = metadata("unknown", 8);
        assert_eq!(fallback["mode"], "estimated");
        assert_eq!(fallback["tokenizer"], "estimated");
        assert!(
            fallback["reason"]
                .as_str()
                .unwrap()
                .contains("unsupported tokenizer")
        );
    }

    #[test]
    fn count_uses_serialized_bytes_when_no_exact_tokenizer_is_selected() {
        let value = json!({"a": [1, 2, 3], "text": "hello"});
        let expected = serde_json::to_string(&value).unwrap().len() as u64;
        assert_eq!(count(&value).unwrap(), expected);
    }

    #[test]
    fn exact_counters_measure_the_complete_compact_json() {
        for tokenizer in [CL100K, O200K] {
            let value = json!({"token_count": {"tokenizer": tokenizer}, "text": "hello world"});
            let text = serde_json::to_string(&value).unwrap();
            let counted = count(&value).unwrap();
            assert!(counted > 0, "{tokenizer}");
            assert!(counted <= text.chars().count() as u64 + 1, "{tokenizer}");
        }
    }

    #[test]
    fn finalize_stabilizes_the_self_describing_total() {
        let mut value = json!({
            "output": {"symbols": ["a", "b", "c"]},
            "token_count": metadata(CL100K, 4096),
        });
        finalize(&mut value).unwrap();
        let total = value["token_count"]["total"].as_u64().unwrap();
        assert!(total > 0);
        assert_eq!(count(&value).unwrap(), total);
        // Running again is idempotent.
        let stable = value.clone();
        finalize(&mut value).unwrap();
        assert_eq!(value, stable);
    }
}
