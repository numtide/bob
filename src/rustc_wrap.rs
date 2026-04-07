//! In-process rustc wrapper for rmeta pipelining.
//!
//! Replaces the per-crate bash wrapper: arg classification, rmeta→rlib
//! polling/swap, JSON-stderr stream parsing, and the cdylib second pass all
//! happen here without forking jq/cp/mv per line. The bash version cost
//! several hundred ms/crate (jq subprocess per diagnostic + while-read loop);
//! this is ~single-digit ms.
//!
//! Invoked via a one-line shim at `tmp/<key>/rustc-wrap/rustc`:
//!     exec /path/to/nix-inc __rustc-wrap "$@"
//! Config arrives via env vars set in builder.sh after `source $stdenv/setup`
//! (so PATH already has the real rustc). The rmeta-ready signal goes out on
//! fd 3 (the worker's saved stdout), so the scheduler's existing
//! `__META_READY__` reader picks it up in-process without a poller thread.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const POLL: Duration = Duration::from_millis(5);
const POLL_CEILING: Duration = Duration::from_secs(60);

struct Cfg {
    real_rustc: String,
    rmeta_dir: PathBuf,
    expected_rmeta: String,
    skip_link_pass: bool,
    /// `$completeDeps $completeBuildDeps` — already rewritten to tmp/<k>/lib
    /// for in-flight crates, artifacts/<k>/lib for cached.
    complete_deps: Vec<String>,
}

impl Cfg {
    fn from_env() -> Self {
        let env = |k: &str| std::env::var(k).unwrap_or_default();
        let deps = format!("{} {}", env("completeDeps"), env("completeBuildDeps"));
        Self {
            real_rustc: env("NIXINC_REAL_RUSTC"),
            rmeta_dir: PathBuf::from(env("NIXINC_RMETA_DIR")),
            expected_rmeta: env("NIXINC_EXPECTED_RMETA"),
            skip_link_pass: !env("NIXINC_SKIP_LINK_PASS").is_empty(),
            complete_deps: deps.split_whitespace().map(String::from).collect(),
        }
    }
}

/// Write `__META_READY__ <dir>` to fd 3 (the worker's saved stdout). The
/// scheduler's `execute_with_signal` reader dispatches it in-process.
fn signal_meta_ready(rmeta_dir: &Path) {
    use std::os::unix::io::FromRawFd;
    // SAFETY: fd 3 is inherited from the worker parent. We must not close it
    // (other signals may follow), so forget the File after writing.
    let mut f = unsafe { std::fs::File::from_raw_fd(3) };
    let _ = writeln!(f, "__META_READY__ {}", rmeta_dir.display());
    std::mem::forget(f);
}

/// Argv classification: crate-type args are split out so the lib half and
/// the link half (cdylib/staticlib/proc-macro/bin) can be driven separately.
#[derive(Default)]
struct Classified {
    args: Vec<String>,
    lib_types: Vec<String>,
    link_types: Vec<String>,
    out_is_target_lib: bool,
    /// Values of `-L dependency=<dir>`. configurePhase populates these dirs
    /// (`target/deps` for the lib, `target/buildDeps` for build.rs) before any
    /// in-flight transitive's rlib exists; `wait_closure_done_and_relink`
    /// fills them in once committed.
    dep_dirs: Vec<PathBuf>,
}

fn classify(argv: &[String]) -> Classified {
    let mut c = Classified::default();
    let mut it = argv.iter().peekable();
    while let Some(a) = it.next() {
        match a.as_str() {
            // --color conflicts with --json; drop both forms.
            "--color" => {
                it.next();
            }
            s if s.starts_with("--color=") => {}
            "-L" => {
                c.args.push(a.clone());
                if let Some(v) = it.next() {
                    if let Some(d) = v.strip_prefix("dependency=") {
                        c.dep_dirs.push(PathBuf::from(d));
                    }
                    c.args.push(v.clone());
                }
            }
            s if s.starts_with("-Ldependency=") => {
                c.dep_dirs.push(PathBuf::from(&s["-Ldependency=".len()..]));
                c.args.push(a.clone());
            }
            "--out-dir" => {
                c.args.push(a.clone());
                if let Some(v) = it.next() {
                    if v == "target/lib" {
                        c.out_is_target_lib = true;
                    }
                    c.args.push(v.clone());
                }
            }
            "--crate-type" => {
                if let Some(v) = it.next() {
                    let dest = if matches!(v.as_str(), "lib" | "rlib") {
                        &mut c.lib_types
                    } else {
                        &mut c.link_types
                    };
                    dest.push("--crate-type".into());
                    dest.push(v.clone());
                }
            }
            s if s.starts_with("--crate-type=") => {
                let v = &s["--crate-type=".len()..];
                let dest = if matches!(v, "lib" | "rlib") {
                    &mut c.lib_types
                } else {
                    &mut c.link_types
                };
                dest.push(a.clone());
            }
            _ => c.args.push(a.clone()),
        }
    }
    c
}

fn poll_until<F: Fn() -> bool>(f: F) {
    let t0 = Instant::now();
    while !f() && t0.elapsed() < POLL_CEILING {
        std::thread::sleep(POLL);
    }
}

/// `--extern foo=tmp/<k>/rmeta/libfoo.rmeta` → tmp/<k>.
fn rmeta_arg_tmp(arg: &str) -> Option<&str> {
    let p = arg.split_once('=')?.1;
    if !p.ends_with(".rmeta") {
        return None;
    }
    let i = p.find("/nix-inc/tmp/")?;
    let after = &p[i + "/nix-inc/tmp/".len()..];
    let key_end = after.find('/')?;
    if &after[key_end..key_end + "/rmeta/".len()] != "/rmeta/" {
        return None;
    }
    Some(&p[..i + "/nix-inc/tmp/".len() + key_end])
}

/// Swap each in-flight `--extern …rmeta` arg to its rlib once the dep commits.
fn swap_rmeta_args_to_rlib(args: &mut [String]) {
    for a in args.iter_mut() {
        let Some(dep_tmp) = rmeta_arg_tmp(a) else {
            continue;
        };
        let done = format!("{dep_tmp}/done");
        poll_until(|| Path::new(&done).exists());
        // tmp/<k>/rmeta/<f>.rmeta → tmp/<k>/lib/lib/<f>.rlib
        let (name, p) = a.split_once('=').unwrap();
        let rlib = p
            .replacen("/rmeta/", "/lib/lib/", 1)
            .replacen(".rmeta", ".rlib", 1);
        *a = format!("{name}={rlib}");
    }
}

/// configurePhase ran before in-flight deps' rlibs existed; once the closure is
/// committed, re-symlink so `-L dependency=<dir>` resolves transitives. The
/// target dir comes from this rustc invocation's actual `-L dependency=` arg
/// (`target/deps` for the lib, `target/buildDeps` for build.rs) — hardcoding
/// `target/deps` left the build-script link without transitive rlibs.
fn wait_closure_done_and_relink(cfg: &Cfg, dep_dirs: &[PathBuf]) {
    for i in &cfg.complete_deps {
        // In-flight paths are `…/nix-inc/tmp/<k>/lib`.
        if let Some(prefix) = i
            .strip_suffix("/lib")
            .filter(|p| p.contains("/nix-inc/tmp/"))
        {
            let done = format!("{prefix}/done");
            poll_until(|| Path::new(&done).exists());
        }
        // Re-symlink rlibs/sos for both in-flight and cached deps into every
        // `-L dependency=` dir this invocation searches. Symlinking the lib's
        // closure into target/buildDeps (and vice versa) is harmless — rustc
        // ignores entries it has no `--extern` for.
        if let Ok(rd) = std::fs::read_dir(format!("{i}/lib")) {
            for e in rd.flatten() {
                let name = e.file_name();
                let n = name.to_string_lossy();
                if n.ends_with(".rlib") || n.ends_with(".so") {
                    for d in dep_dirs {
                        let _ = std::fs::remove_file(d.join(&name));
                        let _ = std::os::unix::fs::symlink(e.path(), d.join(&name));
                    }
                }
            }
        }
    }
}

/// Lib path: a rewritten dep that's actually a proc-macro at build time
/// (read-crate-info detection) won't have an rmeta — only its .so under
/// tmp/<k>/lib/lib/. Fall back to .so/.rlib once committed.
fn resolve_missing_rmeta_args(args: &mut [String]) {
    for a in args.iter_mut() {
        let Some(dep_tmp) = rmeta_arg_tmp(a) else {
            continue;
        };
        let (name, p) = a.split_once('=').unwrap();
        if Path::new(p).exists() {
            continue;
        }
        let done = format!("{dep_tmp}/done");
        poll_until(|| Path::new(&done).exists());
        let base = p
            .replacen("/rmeta/", "/lib/lib/", 1)
            .trim_end_matches(".rmeta")
            .to_string();
        if Path::new(&format!("{base}.so")).exists() {
            *a = format!("{name}={base}.so");
        } else if Path::new(&format!("{base}.rlib")).exists() {
            *a = format!("{name}={base}.rlib");
        }
    }
}

/// Stream rustc's stderr, publish the rmeta, arm the marker, render diagnostics.
fn run_lib_with_stream(cfg: &Cfg, lib_types: &[String], args: &[String]) -> i32 {
    let _ = std::fs::create_dir_all(&cfg.rmeta_dir);
    let mut cmd = Command::new(&cfg.real_rustc);
    cmd.args(lib_types)
        .args(args)
        .arg("--emit=dep-info,metadata,link")
        .arg("--error-format=json")
        .arg("--json=diagnostic-rendered-ansi,artifacts")
        .stdout(Stdio::inherit())
        .stderr(Stdio::piped());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("nix-inc: spawning rustc: {e}");
            return 127;
        }
    };

    let stderr = child.stderr.take().unwrap();
    let reader = BufReader::new(stderr);
    let mut err = std::io::stderr().lock();
    for line in reader.split(b'\n').flatten() {
        // Cheap substring dispatch — avoid full JSON parse for the hot paths.
        // Artifact lines are short and well-formed; diagnostic lines can be
        // megabytes (rendered ANSI), so only the part we need is sliced out.
        if memchr_contains(&line, br#""emit":"metadata""#) {
            if let Some(artifact) = json_str_field(&line, "artifact") {
                use std::os::unix::ffi::OsStrExt;
                let artifact = Path::new(std::ffi::OsStr::from_bytes(&artifact));
                let bn = artifact
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default();
                // Atomic publish to a path installPhase never touches, so a
                // downstream rustc that mmap()s it can't SIGBUS on truncate.
                let tmp = cfg.rmeta_dir.join(".tmp");
                if std::fs::copy(artifact, &tmp).is_ok()
                    && std::fs::rename(&tmp, cfg.rmeta_dir.join(&bn)).is_ok()
                    && bn == cfg.expected_rmeta
                {
                    signal_meta_ready(&cfg.rmeta_dir);
                }
            }
        } else if memchr_contains(&line, br#""$message_type":"artifact""#) {
            // link/dep-info — drop.
        } else if memchr_contains(&line, br#""rendered":""#) {
            if let Some(rendered) = json_str_field(&line, "rendered") {
                let _ = err.write_all(&rendered);
            }
        } else {
            // Non-JSON noise (incremental-cache notices etc.) — pass through.
            let _ = err.write_all(&line);
            let _ = err.write_all(b"\n");
        }
    }

    child.wait().map(|s| s.code().unwrap_or(1)).unwrap_or(1)
}

pub fn main(argv: &[String]) -> ! {
    let cfg = Cfg::from_env();
    let mut c = classify(argv);

    // Anything that LINKS, the build.rs compile, or a build-script probe:
    // needs rlibs IN. The scheduler optimistically rewrote in-flight deps to
    // tmp/<k>/rmeta/*.rmeta; swap back after each dep commits.
    if c.lib_types.is_empty() || !c.out_is_target_lib {
        swap_rmeta_args_to_rlib(&mut c.args);
        wait_closure_done_and_relink(&cfg, &c.dep_dirs);
        let err = Command::new(&cfg.real_rustc)
            .args(&c.lib_types)
            .args(&c.link_types)
            .args(&c.args)
            .exec();
        eprintln!("nix-inc: exec rustc: {err}");
        std::process::exit(127);
    }

    // Lib half: rmetas suffice IN; emit fat rmeta OUT.
    resolve_missing_rmeta_args(&mut c.args);
    let rc = run_lib_with_stream(&cfg, &c.lib_types, &c.args);

    // `lib cdylib`/`lib staticlib`: lib half just ran on rmeta deps (signal
    // already fired). Now wait for upstream rlibs and link the dylib half.
    // rustc rejects rmeta deps for any linking crate-type, so this can't be
    // one pass. Skipped for non-root crates — downstream Rust only reads the
    // rlib, so the `.so` is dead weight unless this crate is the build target.
    let rc2 = if rc == 0 && !c.link_types.is_empty() && !cfg.skip_link_pass {
        swap_rmeta_args_to_rlib(&mut c.args);
        wait_closure_done_and_relink(&cfg, &c.dep_dirs);
        Command::new(&cfg.real_rustc)
            .args(&c.link_types)
            .args(&c.args)
            .status()
            .map(|s| s.code().unwrap_or(1))
            .unwrap_or(1)
    } else {
        0
    };

    std::process::exit(if rc != 0 { rc } else { rc2 });
}

/// Naive substring check on bytes (no allocation, no UTF-8 validation).
fn memchr_contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// Extract a JSON string field's decoded value without a full parse. rustc's
/// output is one object per line; we find `"<field>":"` and decode until the
/// closing unescaped quote. Handles `\n \t \" \\ \uXXXX` (the escapes rustc
/// actually emits in `rendered`). Returns raw bytes so multi-byte UTF-8 in
/// `rendered` (user source quoted in errors) round-trips to stderr unchanged;
/// the artifact-path caller does the from_utf8 itself.
fn json_str_field(line: &[u8], field: &str) -> Option<Vec<u8>> {
    let key = format!("\"{field}\":\"");
    let start = line.windows(key.len()).position(|w| w == key.as_bytes())? + key.len();
    let mut out: Vec<u8> = Vec::with_capacity(256);
    let mut i = start;
    while i < line.len() {
        match line[i] {
            b'"' => return Some(out),
            b'\\' => {
                i += 1;
                match line.get(i)? {
                    b'n' => out.push(b'\n'),
                    b't' => out.push(b'\t'),
                    b'r' => out.push(b'\r'),
                    b'"' => out.push(b'"'),
                    b'\\' => out.push(b'\\'),
                    b'/' => out.push(b'/'),
                    b'u' => {
                        let hex = std::str::from_utf8(line.get(i + 1..i + 5)?).ok()?;
                        let cp = u32::from_str_radix(hex, 16).ok()?;
                        let mut buf = [0u8; 4];
                        let s = char::from_u32(cp)
                            .unwrap_or('\u{FFFD}')
                            .encode_utf8(&mut buf);
                        out.extend_from_slice(s.as_bytes());
                        i += 4;
                    }
                    c => out.push(*c),
                }
            }
            c => out.push(c),
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_field_basic() {
        let line = br#"{"$message_type":"artifact","artifact":"target/lib/libfoo-abc.rmeta","emit":"metadata"}"#;
        assert_eq!(
            json_str_field(line, "artifact").as_deref(),
            Some(&b"target/lib/libfoo-abc.rmeta"[..])
        );
        assert_eq!(
            json_str_field(line, "emit").as_deref(),
            Some(&b"metadata"[..])
        );
    }

    #[test]
    fn json_field_escapes() {
        let line = br#"{"rendered":"error: foo\n  --> src/lib.rs:1:1\n  \"quoted\"\n"}"#;
        assert_eq!(
            json_str_field(line, "rendered").as_deref(),
            Some(&b"error: foo\n  --> src/lib.rs:1:1\n  \"quoted\"\n"[..])
        );
    }

    #[test]
    fn json_field_utf8_roundtrip() {
        // rustc emits raw UTF-8 in `rendered`; decoder must not Latin-1 mangle it.
        let line = "{\"rendered\":\"café → │\\n\"}".as_bytes();
        assert_eq!(
            json_str_field(line, "rendered").as_deref(),
            Some("café → │\n".as_bytes())
        );
    }

    #[test]
    fn rmeta_tmp_extraction() {
        let a = "syn=/root/.cache/nix-inc/tmp/abc123/rmeta/libsyn-x.rmeta";
        assert_eq!(rmeta_arg_tmp(a), Some("/root/.cache/nix-inc/tmp/abc123"));
        assert_eq!(rmeta_arg_tmp("syn=/artifacts/abc/lib/libsyn.rlib"), None);
    }

    #[test]
    fn classify_lib_cdylib() {
        let args: Vec<String> = [
            "src/lib.rs",
            "--out-dir",
            "target/lib",
            "--crate-type",
            "lib",
            "--crate-type",
            "cdylib",
            "--color",
            "always",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let c = classify(&args);
        assert!(c.out_is_target_lib);
        assert_eq!(c.lib_types, vec!["--crate-type", "lib"]);
        assert_eq!(c.link_types, vec!["--crate-type", "cdylib"]);
        assert!(!c.args.iter().any(|a| a.contains("color")));
    }
}
