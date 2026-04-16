//! Persistent bash worker with stdenv pre-sourced.
//!
//! Spawns a bash process that sources stdenv/setup once, then accepts
//! build commands: for each, it forks a subshell that inherits the
//! fully-initialized stdenv (PATH, genericBuild, hooks, arrays).
//! Per-build stdout/stderr are redirected to temp files to avoid interleaving.
//!
//! This saves ~40ms per crate (stdenv sourcing cost).
//!
//! ## Pipelining protocol
//!
//! The worker saves its stdout as fd 3 (`exec 3>&1`). Subshells inherit
//! this fd, so build tools can write signals mid-build:
//!
//!   `__META_READY__ <rmeta_dir>\n`  — .rmeta files are available
//!
//! The Rust side reads lines from worker stdout, dispatching intermediate
//! signals before the final `__DONE__ <exit_code>`.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

/// Result of a single build executed by a worker.
pub struct WorkerBuildResult {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// A persistent bash process with stdenv sourced.
pub struct Worker {
    child: Child,
    reader: BufReader<std::process::ChildStdout>,
}

impl Worker {
    /// Spawn a worker that sources stdenv/setup, then waits for build scripts.
    pub fn spawn(bash: &str, stdenv_path: &str) -> Result<Self, String> {
        let setup = format!("{stdenv_path}/setup");

        // The parent bash reads script paths from stdin, forks a subshell per
        // script with stdout/stderr to temp files (fd 3 passed through for
        // mid-build __META_READY__ signaling), and reports `__DONE__ <rc>`.
        // stdenv is sourced inside each build script (after env.sh), not here
        // — input processing must see the crate's real *Inputs. We pre-source
        // it once anyway to fail fast if the toolchain is broken.
        //
        // Locale is forced to C: the Nix sandbox has no LC_*/LANG, and host
        // locale leaking into the replay changes sort order, regex character
        // classes, decimal separators (EPOCHREALTIME, printf %f), etc.
        let init = format!(
            r#"
export LC_ALL=C
unset LANG LANGUAGE
export out=/dev/null
export lib=/dev/null
export outputs="out lib"
export NIX_ENFORCE_PURITY=0
export NIX_STORE=/nix/store
export NIX_BUILD_TOP=/tmp/nib-worker-$$
export TMPDIR=/tmp/nib-worker-$$
export HOME=/homeless-shelter
mkdir -p "$NIX_BUILD_TOP"

source "{setup}" 2>/dev/null || true

# Save stdout as fd 3 for mid-build signaling.
# Subshells inherit fd 3 so build tools can write
# __META_READY__ messages directly to the Rust reader.
exec 3>&1

echo "__READY__"

# One path per line so spaces in $XDG_CACHE_HOME don't break field splitting.
while IFS= read -r script_path; do
    IFS= read -r stdout_file
    IFS= read -r stderr_file
    ( source "$script_path" ) > "$stdout_file" 2> "$stderr_file" 3>&3
    echo "__DONE__ $?"
done
"#
        );

        // Put the worker in its own process group so Drop can kill the whole
        // tree. Without this, an aborted run leaves orphaned subshells (e.g.
        // aws-lc-sys mid-cmake) writing into tmp/<key>/ that the next run's
        // prepare_tmp() removes from under them.
        // NIX_BUILD_CORES is part of the standard nix builder environment
        // (libstore sets it from `--cores`, default = nproc). Replays must
        // export it: build scripts size `make -j` from it, and cnp's Rust
        // builder sizes the rustc/build-script jobserver from it (defaulting
        // to 1 when unset → serial LLVM codegen). Honour an inherited value
        // so callers can throttle.
        let cores = std::env::var("NIX_BUILD_CORES").unwrap_or_else(|_| {
            std::thread::available_parallelism()
                .map_or(1, |n| n.get())
                .to_string()
        });

        let mut child = unsafe {
            Command::new(bash)
                .env("NIX_BUILD_CORES", &cores)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .arg("-c")
                .arg(&init)
                .pre_exec(|| {
                    // setsid(): new session + process group, pgid = pid
                    if libc::setsid() == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                })
                .spawn()
                .map_err(|e| format!("spawning worker: {e}"))?
        };

        let stdout = child.stdout.take().unwrap();
        let mut reader = BufReader::new(stdout);

        // Wait for READY
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .map_err(|e| format!("reading worker ready: {e}"))?;
        if !line.contains("__READY__") {
            return Err(format!("worker didn't signal ready, got: {line}"));
        }

        Ok(Self { child, reader })
    }

    /// Execute a build script in a forked subshell.
    /// `script_path` is the path to the crate's builder.sh.
    /// `tmp_dir` is where stdout/stderr temp files go.
    ///
    /// Returns after `__DONE__`, but may receive `__META_READY__` mid-build.
    /// The caller can provide an `on_meta_ready` callback that fires when
    /// metadata becomes available (before the full build finishes).
    pub fn execute_with_signal(
        &mut self,
        script_path: &Path,
        tmp_dir: &Path,
        on_meta_ready: impl FnOnce(PathBuf),
    ) -> Result<WorkerBuildResult, String> {
        let stdout_file = tmp_dir.join("worker-stdout");
        let stderr_file = tmp_dir.join("worker-stderr");

        std::fs::write(&stdout_file, b"").map_err(|e| format!("creating stdout file: {e}"))?;
        std::fs::write(&stderr_file, b"").map_err(|e| format!("creating stderr file: {e}"))?;

        let stdin = self.child.stdin.as_mut().ok_or("worker stdin closed")?;

        writeln!(
            stdin,
            "{}\n{}\n{}",
            script_path.display(),
            stdout_file.display(),
            stderr_file.display(),
        )
        .map_err(|e| format!("writing to worker: {e}"))?;
        stdin
            .flush()
            .map_err(|e| format!("flushing worker stdin: {e}"))?;

        let mut callback = Some(on_meta_ready);

        // Read lines until __DONE__, handling intermediate signals
        loop {
            let mut line = String::new();
            self.reader
                .read_line(&mut line)
                .map_err(|e| format!("reading worker result: {e}"))?;

            if let Some(dir_str) = line.strip_prefix("__META_READY__ ") {
                if let Some(cb) = callback.take() {
                    cb(PathBuf::from(dir_str.trim()));
                }
                continue;
            }

            if let Some(code_str) = line.strip_prefix("__DONE__ ") {
                let exit_code = code_str.trim().parse::<i32>().unwrap_or(-1);
                let stdout = std::fs::read_to_string(&stdout_file).unwrap_or_default();
                let stderr = std::fs::read_to_string(&stderr_file).unwrap_or_default();
                return Ok(WorkerBuildResult {
                    exit_code,
                    stdout,
                    stderr,
                });
            }

            if line.is_empty() {
                return Err("worker closed stdout unexpectedly".into());
            }

            // Unknown line — ignore (could be stray output)
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        // Closing stdin makes the read loop exit, but any in-flight subshell
        // (forked before EOF) keeps running. Kill the whole process group so
        // orphaned builds don't collide with a subsequent run's tmp dirs.
        let pid = self.child.id() as libc::pid_t;
        unsafe {
            libc::kill(-pid, libc::SIGTERM);
        }
        drop(self.child.stdin.take());
        let _ = self.child.wait();
    }
}
