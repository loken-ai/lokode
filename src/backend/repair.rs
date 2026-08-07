//! Three-layer auto-repair for tool-call JSON.
//!
//!  Layer 1 — Grammar (server-enforced via response_format=json_schema).
//!            Lives in `backend::client`; this module is the
//!            client-side safety net.
//!  Layer 2 — Tolerant parse: serde_json, then json5, plus a small
//!            recovery pass (strip code fences, normalise Python bools).
//!  Layer 3 — Repair round-trip: ask the model to fix its own output,
//!            capped at 2 attempts per turn.

use anyhow::{bail, Result};
use serde_json::Value;

pub const MAX_REPAIR_ATTEMPTS: usize = 2;

/// Strip a single ```json …``` / ``` …``` fenced block if present, return inner.
pub fn strip_code_fence(s: &str) -> &str {
    let t = s.trim();
    if let Some(rest) = t.strip_prefix("```json") {
        return rest.trim_start().trim_end_matches("```").trim();
    }
    if let Some(rest) = t.strip_prefix("```") {
        return rest.trim_start().trim_end_matches("```").trim();
    }
    t
}

/// Find the substring between the first '{' and the matching final '}'
/// (balanced, ignoring braces inside strings). Returns None if no balanced
/// pair found — caller falls back to raw input.
fn extract_balanced_object(s: &str) -> Option<&str> {
    let bytes = s.as_bytes();
    let start = s.find('{')?;
    let mut depth = 0i32;
    let mut in_str = false;
    let mut esc = false;
    for i in start..bytes.len() {
        let c = bytes[i];
        if in_str {
            if esc { esc = false; }
            else if c == b'\\' { esc = true; }
            else if c == b'"' { in_str = false; }
            continue;
        }
        match c {
            b'"' => in_str = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&s[start..=i]);
                }
            }
            _ => {}
        }
    }
    None
}

/// Replace bare Python literals with JSON equivalents *outside* string bodies.
fn pythonise(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    let mut in_str = false;
    let mut esc = false;
    while i < bytes.len() {
        let c = bytes[i];
        if in_str {
            out.push(c as char);
            if esc { esc = false; }
            else if c == b'\\' { esc = true; }
            else if c == b'"' { in_str = false; }
            i += 1;
            continue;
        }
        // Word-aligned replacement
        if c.is_ascii_alphabetic() {
            let mut j = i;
            while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
                j += 1;
            }
            let word = &s[i..j];
            let prev_alnum = !out.is_empty() && out.as_bytes().last().map(|b| b.is_ascii_alphanumeric() || *b == b'_').unwrap_or(false);
            let mapped = if prev_alnum {
                None
            } else {
                match word {
                    "True" => Some("true"),
                    "False" => Some("false"),
                    "None" => Some("null"),
                    _ => None,
                }
            };
            out.push_str(mapped.unwrap_or(word));
            i = j;
            continue;
        }
        if c == b'"' { in_str = true; }
        out.push(c as char);
        i += 1;
    }
    out
}

/// Parse the model's raw text into a JSON value via the layer-2 pipeline.
pub fn tolerant_parse(raw: &str) -> Result<Value> {
    // Fast path
    if let Ok(v) = serde_json::from_str::<Value>(raw) {
        return Ok(v);
    }
    let unfenced = strip_code_fence(raw);
    if let Ok(v) = serde_json::from_str::<Value>(unfenced) {
        return Ok(v);
    }
    if let Some(obj) = extract_balanced_object(unfenced) {
        if let Ok(v) = serde_json::from_str::<Value>(obj) {
            return Ok(v);
        }
        let py = pythonise(obj);
        if let Ok(v) = serde_json::from_str::<Value>(&py) {
            return Ok(v);
        }
        if let Ok(v) = json5::from_str::<Value>(&py) {
            return Ok(v);
        }
    }
    // Last-ditch: json5 over the unfenced body
    if let Ok(v) = json5::from_str::<Value>(unfenced) {
        return Ok(v);
    }
    bail!("could not parse model output as JSON")
}

/// Compose the repair prompt to send back to the model.
pub fn repair_prompt(error: &str, raw_output: &str) -> String {
    format!(
        "Your previous response was not valid for the required JSON schema.\n\
         Parser error: {error}\n\n\
         Re-emit ONLY a single JSON object that conforms exactly to the schema. \
         Do not include prose, code fences, or commentary outside the JSON.\n\n\
         Previous (invalid) response was:\n{raw_output}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn plain_json_passes() {
        let v = tolerant_parse(r#"{"a": 1}"#).unwrap();
        assert_eq!(v, json!({"a": 1}));
    }

    #[test]
    fn fenced_json_passes() {
        let v = tolerant_parse("```json\n{\"a\": 1}\n```").unwrap();
        assert_eq!(v, json!({"a": 1}));
    }

    #[test]
    fn prose_before_object_passes() {
        let v = tolerant_parse("Here you go:\n{\"a\": 1}\nAll done.").unwrap();
        assert_eq!(v, json!({"a": 1}));
    }

    #[test]
    fn trailing_comma_recovered_by_json5() {
        let v = tolerant_parse(r#"{"a": 1,}"#).unwrap();
        assert_eq!(v["a"], 1);
    }

    #[test]
    fn python_bools_recovered() {
        let v = tolerant_parse(r#"{"ok": True, "bad": False, "n": None}"#).unwrap();
        assert_eq!(v, json!({"ok": true, "bad": false, "n": null}));
    }

    #[test]
    fn single_quotes_recovered() {
        let v = tolerant_parse(r#"{'a': 'b'}"#).unwrap();
        assert_eq!(v, json!({"a": "b"}));
    }

    #[test]
    fn unparseable_fails() {
        assert!(tolerant_parse("this is not json at all !!!").is_err());
    }

    #[test]
    fn string_with_braces_doesnt_confuse_extractor() {
        let v = tolerant_parse(r#"{"s": "a {fake} brace"}"#).unwrap();
        assert_eq!(v["s"], "a {fake} brace");
    }
}
