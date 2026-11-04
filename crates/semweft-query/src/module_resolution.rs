use crate::{engine::StoredView, query};
use anyhow::{Context, Result, bail};
use semweft_core::SourceFile;
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    time::Instant,
};

pub(crate) fn json_config(text: &str) -> Result<Value> {
    let mut bytes = text.as_bytes().to_vec();
    let mut offset = 0;
    let mut quoted = false;
    let mut escaped = false;
    while offset < bytes.len() {
        let byte = bytes[offset];
        if quoted {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                quoted = false;
            }
        } else if byte == b'"' {
            quoted = true;
        } else if byte == b'/' && bytes.get(offset + 1) == Some(&b'/') {
            while offset < bytes.len() && bytes[offset] != b'\n' {
                bytes[offset] = b' ';
                offset += 1;
            }
            continue;
        } else if byte == b'/' && bytes.get(offset + 1) == Some(&b'*') {
            bytes[offset] = b' ';
            bytes[offset + 1] = b' ';
            offset += 2;
            let mut closed = false;
            while offset + 1 < bytes.len() {
                if bytes[offset] == b'*' && bytes[offset + 1] == b'/' {
                    bytes[offset] = b' ';
                    bytes[offset + 1] = b' ';
                    offset += 2;
                    closed = true;
                    break;
                }
                if bytes[offset] != b'\n' {
                    bytes[offset] = b' ';
                }
                offset += 1;
            }
            if !closed {
                bail!("invalid_argument: unterminated JSONC comment");
            }
            continue;
        }
        offset += 1;
    }
    quoted = false;
    escaped = false;
    for offset in 0..bytes.len() {
        let byte = bytes[offset];
        if quoted {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                quoted = false;
            }
        } else if byte == b'"' {
            quoted = true;
        } else if byte == b','
            && bytes[offset + 1..]
                .iter()
                .find(|byte| !byte.is_ascii_whitespace())
                .is_some_and(|byte| matches!(byte, b'}' | b']'))
        {
            bytes[offset] = b' ';
        }
    }
    serde_json::from_slice(&bytes).context("invalid_argument: invalid JSONC configuration")
}

fn joined(directory: &str, path: &str) -> Option<String> {
    if path.starts_with('/') || path.contains('\\') || path.contains(':') {
        return None;
    }
    let mut parts = directory
        .split('/')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            _ => parts.push(part),
        }
    }
    Some(parts.join("/"))
}

#[derive(Clone, Default)]
struct Config {
    base_url: Option<String>,
    paths: BTreeMap<String, (Vec<String>, String)>,
}

fn config(
    files: &BTreeMap<String, SourceFile>,
    path: &str,
    seen: &mut BTreeSet<String>,
    deadline: Instant,
) -> Result<Config> {
    query::check_deadline(deadline)?;
    if !seen.insert(path.into()) || seen.len() > 64 {
        bail!("invalid_argument: tsconfig extends cycle or depth limit");
    }
    let file = files.get(path).with_context(|| {
        format!("invalid_argument: tsconfig extends target not captured: {path}")
    })?;
    let value = json_config(&file.content)?;
    let directory = path
        .rsplit_once('/')
        .map(|(directory, _)| directory)
        .unwrap_or("");
    let mut result = Config::default();
    if let Some(parent) = value["extends"].as_str() {
        if !parent.starts_with('.') {
            bail!("capability_unsupported: package tsconfig extends needs the semantic indexer");
        }
        let mut path = joined(directory, parent)
            .context("invalid_argument: tsconfig parent escapes workspace")?;
        if !path.ends_with(".json") {
            path.push_str(".json");
        }
        result = config(files, &path, seen, deadline)?;
    } else if value.get("extends").is_some() {
        bail!("capability_unsupported: tsconfig multiple inheritance requires semantic resolution");
    }
    if let Some(base) = value["compilerOptions"]["baseUrl"].as_str() {
        result.base_url = Some(
            joined(directory, base)
                .context("invalid_argument: baseUrl escapes captured workspace")?,
        );
    }
    if let Some(paths) = value["compilerOptions"]["paths"].as_object() {
        result.paths.clear();
        for (pattern, targets) in paths {
            query::check_deadline(deadline)?;
            if pattern.matches('*').count() > 1 {
                bail!("invalid_argument: paths patterns support at most one wildcard");
            }
            let targets = targets
                .as_array()
                .context("invalid_argument: paths entries must be string arrays")?
                .iter()
                .map(|target| {
                    target
                        .as_str()
                        .map(str::to_owned)
                        .context("invalid_argument: paths target must be a string")
                })
                .collect::<Result<Vec<_>>>()?;
            result.paths.insert(
                pattern.clone(),
                (
                    targets,
                    result.base_url.as_deref().unwrap_or(directory).into(),
                ),
            );
        }
    }
    seen.remove(path);
    Ok(result)
}

fn captured_file(files: &BTreeMap<String, SourceFile>, base: &str) -> Option<String> {
    for (suffix, replacements) in [
        (".js", &["ts", "tsx", "d.ts", "js"][..]),
        (".jsx", &["tsx", "d.ts", "jsx"][..]),
        (".mjs", &["mts", "d.mts", "mjs"][..]),
        (".cjs", &["cts", "d.cts", "cjs"][..]),
    ] {
        if let Some(stem) = base.strip_suffix(suffix) {
            return replacements
                .iter()
                .map(|extension| format!("{stem}.{extension}"))
                .find(|path| files.contains_key(path));
        }
    }
    if files.contains_key(base) && crate::web::is_source(base) {
        return Some(base.into());
    }
    let stem = base;
    for extension in ["ts", "tsx", "d.ts", "mts", "cts", "js", "jsx", "mjs", "cjs"] {
        for path in [
            format!("{stem}.{extension}"),
            format!("{base}/index.{extension}"),
        ] {
            if files.contains_key(&path) {
                return Some(path);
            }
        }
    }
    None
}

pub(crate) fn resolve(
    stored: &StoredView,
    source: &str,
    import: &str,
    deadline: Instant,
) -> Result<Option<String>> {
    let files = &stored.view.snapshot.manifest.files;
    let directory = source
        .rsplit_once('/')
        .map(|(directory, _)| directory)
        .unwrap_or("");
    if import.starts_with('.') {
        return Ok(joined(directory, import).and_then(|path| captured_file(files, &path)));
    }
    let mut scope = Some(directory);
    while let Some(directory) = scope {
        query::check_deadline(deadline)?;
        let path = ["tsconfig.json", "jsconfig.json"]
            .iter()
            .map(|name| {
                if directory.is_empty() {
                    name.to_string()
                } else {
                    format!("{directory}/{name}")
                }
            })
            .find(|path| files.contains_key(path));
        if let Some(path) = path {
            let config = config(files, &path, &mut BTreeSet::new(), deadline)?;
            let mut matching = vec![];
            for (pattern, (targets, base)) in &config.paths {
                let wildcard = if let Some((prefix, suffix)) = pattern.split_once('*') {
                    import
                        .strip_prefix(prefix)
                        .and_then(|rest| rest.strip_suffix(suffix))
                } else if pattern == import {
                    Some("")
                } else {
                    None
                };
                if let Some(wildcard) = wildcard {
                    let prefix_len = pattern.split('*').next().unwrap_or("").len();
                    matching.push((
                        !pattern.contains('*'),
                        prefix_len,
                        pattern.len(),
                        wildcard,
                        targets,
                        base,
                    ));
                }
            }
            matching.sort_by(|left, right| {
                right
                    .0
                    .cmp(&left.0)
                    .then(right.1.cmp(&left.1))
                    .then(right.2.cmp(&left.2))
            });
            if let Some((_, _, _, wildcard, targets, base)) = matching.first() {
                for target in *targets {
                    query::check_deadline(deadline)?;
                    if let Some(path) = joined(base, &target.replace('*', wildcard))
                        .and_then(|path| captured_file(files, &path))
                    {
                        return Ok(Some(path));
                    }
                }
            }
            if let Some(base) = config.base_url
                && let Some(path) =
                    joined(&base, import).and_then(|path| captured_file(files, &path))
            {
                return Ok(Some(path));
            }
            break;
        }
        scope = directory
            .rsplit_once('/')
            .map(|(parent, _)| parent)
            .or(if directory.is_empty() { None } else { Some("") });
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn jsonc_preserves_strings_and_rejects_unterminated_comments() {
        let value = json_config(
            "{\"url\":\"https://example.test/*x*/\",/*comment*/\"items\":[1,2,],// hi\n}",
        )
        .unwrap();
        assert_eq!(value["url"], "https://example.test/*x*/");
        assert_eq!(value["items"].as_array().unwrap().len(), 2);
        assert!(json_config("{ /* open").is_err());
        assert_eq!(joined("src", "../shared/api"), Some("shared/api".into()));
        assert!(joined("", "../../outside").is_none());
    }
}
