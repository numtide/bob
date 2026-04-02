//! Parser for Nix .drv files (ATerm format).
//!
//! The on-disk format is:
//! ```text
//! Derive(
//!   [("out","/nix/store/...","",""), ...],       // outputs
//!   [("/nix/store/...drv",["out"]), ...],         // input derivations
//!   ["/nix/store/...", ...],                      // input sources
//!   "x86_64-linux",                               // platform
//!   "/nix/store/.../bash",                        // builder
//!   ["-e", "...", ...],                           // args
//!   [("key","value"), ...]                        // env vars
//! )
//! ```
//!
//! String escaping: `\"`, `\\`, `\n`, `\r`, `\t` — nothing else.

use std::collections::BTreeMap;

/// A parsed Nix derivation.
#[derive(Debug, Clone)]
pub struct Derivation {
    pub outputs: BTreeMap<String, Output>,
    /// Map from input drv path → set of output names requested.
    pub input_derivations: BTreeMap<String, Vec<String>>,
    pub input_sources: Vec<String>,
    pub platform: String,
    pub builder: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
}

#[derive(Debug, Clone)]
pub struct Output {
    pub path: String,
    pub hash_algo: String,
    pub hash: String,
}

/// Parser state: a cursor over bytes.
struct Parser<'a> {
    input: &'a [u8],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn new(input: &'a [u8]) -> Self {
        Self { input, pos: 0 }
    }

    fn remaining(&self) -> &'a [u8] {
        &self.input[self.pos..]
    }

    fn peek(&self) -> Option<u8> {
        self.input.get(self.pos).copied()
    }

    fn advance(&mut self, n: usize) {
        self.pos += n;
    }

    fn expect_byte(&mut self, b: u8) -> Result<(), String> {
        match self.peek() {
            Some(c) if c == b => {
                self.advance(1);
                Ok(())
            }
            Some(c) => Err(format!(
                "expected {:?}, got {:?} at pos {}",
                b as char, c as char, self.pos
            )),
            None => Err(format!("expected {:?}, got EOF at pos {}", b as char, self.pos)),
        }
    }

    fn expect_str(&mut self, s: &str) -> Result<(), String> {
        let bytes = s.as_bytes();
        if self.remaining().starts_with(bytes) {
            self.advance(bytes.len());
            Ok(())
        } else {
            let got: String = self.remaining().iter().take(s.len()).map(|&b| b as char).collect();
            Err(format!("expected {s:?}, got {got:?} at pos {}", self.pos))
        }
    }

    /// Parse a quoted string: `"..."` with escape handling.
    fn parse_string(&mut self) -> Result<String, String> {
        self.expect_byte(b'"')?;
        let mut result = String::new();
        loop {
            match self.peek() {
                None => return Err("unterminated string".into()),
                Some(b'"') => {
                    self.advance(1);
                    return Ok(result);
                }
                Some(b'\\') => {
                    self.advance(1);
                    match self.peek() {
                        Some(b'"') => result.push('"'),
                        Some(b'\\') => result.push('\\'),
                        Some(b'n') => result.push('\n'),
                        Some(b'r') => result.push('\r'),
                        Some(b't') => result.push('\t'),
                        Some(c) => return Err(format!("unknown escape \\{} at pos {}", c as char, self.pos)),
                        None => return Err("unterminated escape".into()),
                    }
                    self.advance(1);
                }
                Some(b) => {
                    result.push(b as char);
                    self.advance(1);
                }
            }
        }
    }

    /// Parse `[elem, elem, ...]` where elem is parsed by `f`.
    fn parse_list<T>(&mut self, f: impl Fn(&mut Self) -> Result<T, String>) -> Result<Vec<T>, String> {
        self.expect_byte(b'[')?;
        let mut items = Vec::new();
        if self.peek() == Some(b']') {
            self.advance(1);
            return Ok(items);
        }
        loop {
            items.push(f(self)?);
            match self.peek() {
                Some(b',') => self.advance(1),
                Some(b']') => {
                    self.advance(1);
                    return Ok(items);
                }
                Some(c) => return Err(format!("expected ',' or ']', got {:?} at pos {}", c as char, self.pos)),
                None => return Err("unterminated list".into()),
            }
        }
    }
}

impl Derivation {
    pub fn parse(input: &[u8]) -> Result<Self, String> {
        let mut p = Parser::new(input);
        p.expect_str("Derive(")?;

        // 1. Outputs: [("name","path","hashAlgo","hash"), ...]
        let output_tuples = p.parse_list(|p| {
            p.expect_byte(b'(')?;
            let name = p.parse_string()?;
            p.expect_byte(b',')?;
            let path = p.parse_string()?;
            p.expect_byte(b',')?;
            let hash_algo = p.parse_string()?;
            p.expect_byte(b',')?;
            let hash = p.parse_string()?;
            p.expect_byte(b')')?;
            Ok((name, Output { path, hash_algo, hash }))
        })?;
        let outputs: BTreeMap<_, _> = output_tuples.into_iter().collect();

        p.expect_byte(b',')?;

        // 2. Input derivations: [("/nix/store/...drv",["out"]), ...]
        let input_drv_tuples = p.parse_list(|p| {
            p.expect_byte(b'(')?;
            let drv_path = p.parse_string()?;
            p.expect_byte(b',')?;
            let outputs = p.parse_list(|p| p.parse_string())?;
            p.expect_byte(b')')?;
            Ok((drv_path, outputs))
        })?;
        let input_derivations: BTreeMap<_, _> = input_drv_tuples.into_iter().collect();

        p.expect_byte(b',')?;

        // 3. Input sources: ["/nix/store/...", ...]
        let input_sources = p.parse_list(|p| p.parse_string())?;

        p.expect_byte(b',')?;

        // 4. Platform
        let platform = p.parse_string()?;

        p.expect_byte(b',')?;

        // 5. Builder
        let builder = p.parse_string()?;

        p.expect_byte(b',')?;

        // 6. Args: ["-e", "...", ...]
        let args = p.parse_list(|p| p.parse_string())?;

        p.expect_byte(b',')?;

        // 7. Env vars: [("key","value"), ...]
        let env_tuples = p.parse_list(|p| {
            p.expect_byte(b'(')?;
            let key = p.parse_string()?;
            p.expect_byte(b',')?;
            let value = p.parse_string()?;
            p.expect_byte(b')')?;
            Ok((key, value))
        })?;
        let env: BTreeMap<_, _> = env_tuples.into_iter().collect();

        p.expect_byte(b')')?;

        Ok(Derivation {
            outputs,
            input_derivations,
            input_sources,
            platform,
            builder,
            args,
            env,
        })
    }

    /// Returns the store paths of all outputs.
    pub fn output_paths(&self) -> Vec<&str> {
        self.outputs.values().map(|o| o.path.as_str()).collect()
    }

    /// Returns the value of an env var, if present.
    pub fn env_var(&self, key: &str) -> Option<&str> {
        self.env.get(key).map(|s| s.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_minimal_drv() {
        let input = br#"Derive([("out","/nix/store/aaaa-foo","","")],[],[],"x86_64-linux","/nix/store/bbbb-bash",["-e","build.sh"],[("name","foo")])"#;
        let drv = Derivation::parse(input).unwrap();
        assert_eq!(drv.outputs.len(), 1);
        assert_eq!(drv.outputs["out"].path, "/nix/store/aaaa-foo");
        assert_eq!(drv.platform, "x86_64-linux");
        assert_eq!(drv.builder, "/nix/store/bbbb-bash");
    }

    #[test]
    fn parse_real_drv_from_store() {
        // Try parsing the actual drv file we instantiated earlier
        let path = "/nix/store/8lqbka6bfivhj53a2m65vnm6z03rn56v-rust_hello-0.1.0.drv";
        if !std::path::Path::new(path).exists() {
            eprintln!("skipping: {path} not found");
            return;
        }
        let contents = std::fs::read(path).unwrap();
        let drv = Derivation::parse(&contents).unwrap();

        assert_eq!(drv.platform, "x86_64-linux");
        assert!(drv.builder.contains("bash"));
        assert!(drv.outputs.contains_key("out"));
        assert!(drv.outputs.contains_key("lib"));
        assert!(drv.env.contains_key("buildPhase"));
        assert!(drv.env.contains_key("configurePhase"));
        assert!(drv.env.contains_key("installPhase"));
        assert_eq!(drv.env["crateName"], "hello");
    }

    #[test]
    fn parse_drv_with_deps() {
        let path = "/nix/store/ps4wmxcnwk3sx6177pn0rwbr2ix7sps4-rust_hello-0.1.0.drv";
        if !std::path::Path::new(path).exists() {
            eprintln!("skipping: {path} not found");
            return;
        }
        let contents = std::fs::read(path).unwrap();
        let drv = Derivation::parse(&contents).unwrap();

        // Should have serde as a dependency
        assert!(
            drv.env.get("completeDeps").is_some_and(|v| v.contains("serde")),
            "expected completeDeps to reference serde"
        );
        // buildPhase should have --extern serde=...
        assert!(
            drv.env["buildPhase"].contains("--extern serde="),
            "expected buildPhase to have --extern serde"
        );
    }

    #[test]
    fn parse_string_escapes() {
        let input = br#"Derive([("out","/nix/store/x-test","","")],[],["/nix/store/src"],"x86_64-linux","/bin/bash",[],[("script","echo \"hello\nworld\ttab\\done\"")])"#;
        let drv = Derivation::parse(input).unwrap();
        assert_eq!(drv.env["script"], "echo \"hello\nworld\ttab\\done\"");
    }

    #[test]
    fn parse_minimal_correct_format() {
        // Correct ATerm format with all 7 fields
        let input = br#"Derive([("out","/nix/store/x-foo","","")],[],["/nix/store/src"],"x86_64-linux","/nix/store/bash/bin/bash",["-e","script.sh"],[("name","foo"),("system","x86_64-linux")])"#;
        let drv = Derivation::parse(input).unwrap();
        assert_eq!(drv.outputs.len(), 1);
        assert_eq!(drv.outputs["out"].path, "/nix/store/x-foo");
        assert_eq!(drv.input_sources, vec!["/nix/store/src"]);
        assert_eq!(drv.platform, "x86_64-linux");
        assert_eq!(drv.builder, "/nix/store/bash/bin/bash");
        assert_eq!(drv.args, vec!["-e", "script.sh"]);
        assert_eq!(drv.env["name"], "foo");
        assert_eq!(drv.env["system"], "x86_64-linux");
    }
}
