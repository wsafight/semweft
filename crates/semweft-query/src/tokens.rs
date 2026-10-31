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
