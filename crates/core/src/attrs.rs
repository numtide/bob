//! `__structuredAttrs` JSON ↔ bash translation.
//!
//! Mirrors what Nix's builder writes alongside `.attrs.json`: a `.attrs.sh`
//! that declares each top-level attr with the right bash type so
//! `$stdenv/setup` can iterate `outputs`/`env`/`*Inputs` as arrays. Also
//! provides the store-path rewriting and `outputs`/`src` remapping that bob
//! applies before emitting both files.

use std::collections::BTreeMap;
use std::path::Path;

use crate::rewrite::PathRewriter;

/// Rewrite output paths and dependency paths in the `__structuredAttrs` JSON
/// so the builder sees our cache paths instead of `/nix/store`. Also remaps
/// `outputs` from `["out",…]` to `{out: <tmp/out>, …}` (matching what Nix
/// writes to `.attrs.json`) and optionally overrides `src`.
pub fn rewrite_structured_attrs_json(
    json_str: &str,
    outputs: &BTreeMap<String, String>,
    rewriter: &PathRewriter,
    src_override: Option<&Path>,
) -> String {
    let mut val: serde_json::Value = match serde_json::from_str(json_str) {
        Ok(v) => v,
        Err(_) => return json_str.to_string(),
    };

    if let serde_json::Value::Object(ref mut map) = val {
        let outputs_map: serde_json::Map<_, _> = outputs
            .iter()
            .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
            .collect();
        map.insert("outputs".into(), serde_json::Value::Object(outputs_map));

        if let Some(src) = src_override {
            map.insert(
                "src".into(),
                serde_json::Value::String(src.to_string_lossy().into_owned()),
            );
        }

        rewrite_json_values(map, rewriter);
    }

    serde_json::to_string(&val).unwrap_or_else(|_| json_str.to_string())
}

fn rewrite_json_values(
    map: &mut serde_json::Map<String, serde_json::Value>,
    rewriter: &PathRewriter,
) {
    for (_key, val) in map.iter_mut() {
        match val {
            serde_json::Value::String(s) => {
                let rewritten = rewriter.rewrite(s);
                if rewritten != *s {
                    *s = rewritten;
                }
            }
            serde_json::Value::Array(arr) => {
                for item in arr.iter_mut() {
                    if let serde_json::Value::String(s) = item {
                        let rewritten = rewriter.rewrite(s);
                        if rewritten != *s {
                            *s = rewritten;
                        }
                    } else if let serde_json::Value::Object(ref mut inner) = item {
                        rewrite_json_values(inner, rewriter);
                    }
                }
            }
            serde_json::Value::Object(ref mut inner) => {
                rewrite_json_values(inner, rewriter);
            }
            _ => {}
        }
    }
}

/// Render a structured-attrs JSON object as bash declarations, byte-for-byte
/// matching Nix's `StructuredAttrs::writeShell` (libstore/parsed-derivations.cc):
///   - string/int/bool/null → `declare k=<simple>`
///   - array of simple      → `declare -a k=(<v> <v> …)`
///   - object of simple     → `declare -A k=(['<ik>']=<v> …)`
///   - anything else (nested objects, mixed arrays, floats) → skipped
///
/// stdenv/setup's structuredAttrs path iterates `outputs`/`env` as associative
/// arrays and `*Inputs` as indexed arrays; sourcing this before setup gives it
/// the shapes it expects. Notably the `env` map (from `mkDerivation { env = … }`)
/// often carries Nix integers/bools, so the object case must accept non-string
/// simples or stdenv's `for envVar in "${!env[@]}"` export loop never runs.
pub fn json_to_attrs_sh(val: &serde_json::Value) -> String {
    use serde_json::Value;
    let mut out = String::new();
    let Value::Object(map) = val else {
        return out;
    };
    for (k, v) in map {
        if !is_valid_bash_ident(k) {
            continue;
        }
        if let Some(s) = simple_to_sh(v) {
            out.push_str(&format!("declare {k}={s}\n"));
        } else if let Value::Array(a) = v {
            if let Some(items) = a.iter().map(simple_to_sh).collect::<Option<Vec<_>>>() {
                out.push_str(&format!("declare -a {k}=({} )\n", items.join(" ")));
            }
        } else if let Value::Object(o) = v {
            if let Some(items) = o
                .iter()
                .map(|(ik, iv)| simple_to_sh(iv).map(|s| format!("[{}]={s}", sh_escape(ik))))
                .collect::<Option<Vec<_>>>()
            {
                out.push_str(&format!("declare -A {k}=({} )\n", items.join(" ")));
            }
        }
    }
    out
}

/// Nix's `handleSimpleType`: string → shell-escaped, integer → decimal,
/// bool → `1`/``, null → `''`. Floats and compound values → None.
fn simple_to_sh(v: &serde_json::Value) -> Option<String> {
    use serde_json::Value;
    match v {
        Value::String(s) => Some(sh_escape(s)),
        Value::Number(n) if n.is_i64() => Some(n.as_i64().unwrap().to_string()),
        Value::Number(n) if n.is_u64() => Some(n.as_u64().unwrap().to_string()),
        Value::Bool(true) => Some("1".into()),
        Value::Bool(false) => Some(String::new()),
        Value::Null => Some("''".into()),
        _ => None,
    }
}

/// POSIX-sh single-quote escaping: wrap in '…', replace embedded ' with '\''.
pub fn sh_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

/// Bash identifier: `[A-Za-z_][A-Za-z0-9_]*`. Keys like `__json` pass; keys
/// like `foo-bar` or `0abc` are skipped (Nix's .attrs.sh generator does the
/// same).
pub fn is_valid_bash_ident(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Escape a string for bash `$'...'` quoting.
/// Only `\`, `'`, newline, tab, carriage return need escaping.
pub fn escape_for_dollar_single_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + s.len() / 8);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attrs_sh_shapes() {
        // Key names are arbitrary; we're testing the bash-declaration shapes
        // (scalar / -a / -A) and the simple-type coercion, not any builder's
        // schema. `outputs` and `env` use the real names because stdenv/setup
        // iterates exactly those.
        let json = serde_json::json!({
            "name": "foo",
            "release": true,
            "jobs": 16,
            "nativeBuildInputs": ["/nix/store/a", "/nix/store/b"],
            "outputs": {"out": "/tmp/out", "lib": "/tmp/lib"},
            "env": {"SOME_INT": 1, "NIX_MAIN_PROGRAM": "foo"},
            "nested": [{"x": 1}],          // nested → skipped
            "bad-key": "nope",             // invalid ident → skipped
            "quoted": "it's fine",
            "nullish": null,
        });
        let sh = json_to_attrs_sh(&json);
        assert!(sh.contains("declare name='foo'\n"));
        assert!(sh.contains("declare release=1\n"));
        assert!(sh.contains("declare jobs=16\n"));
        assert!(sh.contains("declare -a nativeBuildInputs=('/nix/store/a' '/nix/store/b' )\n"));
        assert!(sh.contains("declare -A outputs=("));
        assert!(sh.contains("['out']='/tmp/out'"));
        assert!(sh.contains("['lib']='/tmp/lib'"));
        // env with mixed int/string values must NOT be skipped
        assert!(sh.contains("declare -A env=("));
        assert!(sh.contains("['SOME_INT']=1"));
        assert!(sh.contains("['NIX_MAIN_PROGRAM']='foo'"));
        assert!(!sh.contains("nested"));
        assert!(!sh.contains("bad-key"));
        assert!(sh.contains("declare quoted='it'\\''s fine'\n"));
        assert!(sh.contains("declare nullish=''\n"));
    }

    #[test]
    fn bash_ident_validation() {
        assert!(is_valid_bash_ident("foo"));
        assert!(is_valid_bash_ident("_foo123"));
        assert!(is_valid_bash_ident("__json"));
        assert!(!is_valid_bash_ident("0foo"));
        assert!(!is_valid_bash_ident("foo-bar"));
        assert!(!is_valid_bash_ident(""));
    }
}
