//! Enumerate testharness tests and reftests from WPT's `MANIFEST.json`.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::Value;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TestType {
    Testharness,
    Reftest,
}

impl TestType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Testharness => "testharness",
            Self::Reftest => "reftest",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
pub struct Fuzzy {
    pub max_difference: [u64; 2],
    pub different_pixels: [u64; 2],
}

#[derive(Clone, Debug)]
pub struct Reference {
    pub url: String,
    pub relation: String,
    pub fuzzy: Fuzzy,
}

#[derive(Clone, Debug)]
pub struct TestCase {
    pub path: String,
    pub url: String,
    pub test_type: TestType,
    pub references: Vec<Reference>,
    pub long_timeout: bool,
    pub unsupported_reason: Option<String>,
}

impl TestCase {
    pub fn explicit(url: &str) -> Self {
        Self {
            path: url.to_string(),
            url: url.to_string(),
            test_type: TestType::Testharness,
            references: Vec::new(),
            long_timeout: false,
            unsupported_reason: None,
        }
    }
}

pub struct UrlBuilder {
    pub host: String,
    pub http_port: u16,
    pub https_port: u16,
    pub h2_port: u16,
}

impl UrlBuilder {
    fn build(&self, url_path: &str, file_path: &str) -> String {
        if url_path.contains("://")
            || url_path.starts_with("about:")
            || url_path.starts_with("data:")
        {
            return url_path.to_string();
        }
        let path = if url_path.starts_with('/') {
            url_path.to_string()
        } else {
            format!("/{url_path}")
        };
        if file_path.contains(".h2.") {
            format!("https://{}:{}{}", self.host, self.h2_port, path)
        } else if file_path.contains(".https.")
            || file_path.contains(".serviceworker.")
            || file_path.contains(".h3.")
        {
            format!("https://{}:{}{}", self.host, self.https_port, path)
        } else {
            format!("http://{}:{}{}", self.host, self.http_port, path)
        }
    }
}

pub fn load_tests(
    manifest_path: &Path,
    base: &UrlBuilder,
    filter: Option<&str>,
    include_reftests: bool,
) -> Result<Vec<TestCase>> {
    let data = fs::read_to_string(manifest_path)
        .with_context(|| format!("read {}", manifest_path.display()))?;
    let value: Value = serde_json::from_str(&data).context("parse MANIFEST.json")?;
    Ok(load_value(&value, base, filter, include_reftests))
}

fn load_value(
    value: &Value,
    base: &UrlBuilder,
    filter: Option<&str>,
    include_reftests: bool,
) -> Vec<TestCase> {
    let mut out = Vec::new();
    if let Some(tree) = value.pointer("/items/testharness") {
        walk(
            tree,
            &mut String::new(),
            TestType::Testharness,
            base,
            &mut out,
        );
    }
    if include_reftests {
        if let Some(tree) = value.pointer("/items/reftest") {
            walk(
                tree,
                &mut String::new(),
                TestType::Reftest,
                base,
                &mut out,
            );
        }
    }
    if let Some(filter) = filter {
        out.retain(|test| test.path.contains(filter) || test.url.contains(filter));
    }
    out.sort_by(|a, b| a.path.cmp(&b.path).then_with(|| a.url.cmp(&b.url)));
    out.dedup_by(|a, b| a.test_type == b.test_type && a.url == b.url);
    out
}

fn walk(
    node: &Value,
    prefix: &mut String,
    test_type: TestType,
    base: &UrlBuilder,
    out: &mut Vec<TestCase>,
) {
    match node {
        Value::Object(map) => {
            for (key, child) in map {
                let saved = prefix.len();
                if !prefix.is_empty() {
                    prefix.push('/');
                }
                prefix.push_str(key);
                walk(child, prefix, test_type, base, out);
                prefix.truncate(saved);
            }
        }
        Value::Array(variants) => {
            let file_path = prefix.clone();
            for variant in variants.iter().skip(1).filter_map(Value::as_array) {
                let url_path = variant
                    .first()
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("/{file_path}"));
                let metadata_index = if test_type == TestType::Reftest { 2 } else { 1 };
                let metadata = variant.get(metadata_index).and_then(Value::as_object);
                let mut unsupported_reason = unsupported_reason(&file_path, metadata);
                let references = if test_type == TestType::Reftest {
                    let refs = variant
                        .get(1)
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_array)
                        .filter_map(|reference| {
                            let raw_url = reference.first()?.as_str()?;
                            let relation = reference.get(1)?.as_str()?;
                            Some(Reference {
                                url: base.build(raw_url, &file_path),
                                relation: relation.to_string(),
                                fuzzy: fuzzy_for(metadata, raw_url, relation),
                            })
                        })
                        .collect::<Vec<_>>();
                    if refs.is_empty() {
                        unsupported_reason
                            .get_or_insert_with(|| "reftest has no references".into());
                    }
                    refs
                } else {
                    Vec::new()
                };
                out.push(TestCase {
                    path: file_path.clone(),
                    url: base.build(&url_path, &file_path),
                    test_type,
                    references,
                    long_timeout: metadata
                        .and_then(|m| m.get("timeout"))
                        .and_then(Value::as_str)
                        == Some("long"),
                    unsupported_reason,
                });
            }
        }
        _ => {}
    }
}

fn unsupported_reason(
    path: &str,
    metadata: Option<&serde_json::Map<String, Value>>,
) -> Option<String> {
    if path.contains(".h3.") {
        return Some("HTTP/3 WPT serving is not configured".into());
    }
    let metadata = metadata?;
    if metadata.get("testdriver").and_then(Value::as_bool) == Some(true)
        || metadata
            .get("testdriver_features")
            .and_then(Value::as_array)
            .is_some()
    {
        return Some("testdriver automation is not implemented".into());
    }
    if metadata.get("pac").is_some() {
        return Some("PAC automation is not implemented".into());
    }
    None
}

fn fuzzy_for(
    metadata: Option<&serde_json::Map<String, Value>>,
    reference: &str,
    relation: &str,
) -> Fuzzy {
    let mut fallback = None;
    let Some(entries) = metadata
        .and_then(|m| m.get("fuzzy"))
        .and_then(Value::as_array)
    else {
        return Fuzzy::default();
    };
    for entry in entries.iter().filter_map(Value::as_array) {
        let Some(bounds) = entry.get(1).and_then(parse_fuzzy_bounds) else {
            continue;
        };
        match entry.first() {
            Some(Value::Null) => fallback = Some(bounds),
            Some(Value::Array(key))
                if key.get(1).and_then(Value::as_str) == Some(reference)
                    && key.get(2).and_then(Value::as_str) == Some(relation) =>
            {
                return bounds;
            }
            _ => {}
        }
    }
    fallback.unwrap_or_default()
}

fn parse_fuzzy_bounds(value: &Value) -> Option<Fuzzy> {
    let ranges = value.as_array()?;
    Some(Fuzzy {
        max_difference: parse_range(ranges.first()?)?,
        different_pixels: parse_range(ranges.get(1)?)?,
    })
}

fn parse_range(value: &Value) -> Option<[u64; 2]> {
    let range = value.as_array()?;
    Some([range.first()?.as_u64()?, range.get(1)?.as_u64()?])
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn base() -> UrlBuilder {
        UrlBuilder {
            host: "web-platform.test".into(),
            http_port: 8000,
            https_port: 8443,
            h2_port: 9000,
        }
    }

    #[test]
    fn loads_harness_variants_protocols_and_metadata() {
        let manifest = json!({"items":{"testharness":{"a":{
            "basic.html":["hash",[null,{}]],
            "secure.h2.html":["hash",[null,{"timeout":"long","testdriver":true}]]
        }}}});
        let tests = load_value(&manifest, &base(), None, false);
        assert_eq!(tests.len(), 2);
        assert_eq!(tests[0].url, "http://web-platform.test:8000/a/basic.html");
        assert_eq!(
            tests[1].url,
            "https://web-platform.test:9000/a/secure.h2.html"
        );
        assert!(tests[1].long_timeout);
        assert!(tests[1]
            .unsupported_reason
            .as_deref()
            .unwrap()
            .contains("testdriver"));
        assert_eq!(base().build("about:blank", "a/basic.html"), "about:blank");
    }

    #[test]
    fn loads_references_and_fuzzy_metadata() {
        let manifest = json!({"items":{"reftest":{"css":{
            "paint.html":["hash",[null,[["/css/ref.html","=="]],
                {"fuzzy":[[null,[[0,2],[0,10]]]]}]],
            "chain.html":["hash",[null,[["/css/paint.html","=="]],{}]]
        }}}});
        let tests = load_value(&manifest, &base(), None, true);
        let paint = tests
            .iter()
            .find(|test| test.path.ends_with("paint.html"))
            .unwrap();
        assert_eq!(paint.references[0].fuzzy.max_difference, [0, 2]);
        assert_eq!(paint.references[0].fuzzy.different_pixels, [0, 10]);
        assert_eq!(tests.len(), 2);
    }
}
