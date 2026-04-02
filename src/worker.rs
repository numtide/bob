//! Persistent bash worker with stdenv pre-sourced.
//!
//! Spawns a bash process that sources stdenv/setup once, then accepts
//! build commands: for each, it forks a subshell that inherits the
//! fully-initialized stdenv (PATH, genericBuild, hooks, arrays).
//! Per-build stdout/stderr are redirected to temp files to avoid interleaving.
//!
//! This saves ~40ms per crate (stdenv sourcing cost).

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

/// A persistent bash process with stdenv sourced.
pub struct Worker {
    child: Child,
    reader: BufReader<std::process::ChildStdout>,
}

/// Result of a single build executed by a worker.
pub struct WorkerBuildResult {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl Worker {
    /// Spawn a worker that sources stdenv/setup, then waits for build scripts.
    pub fn spawn(bash: &str, stdenv_path: &str) -> Result<Self, String> {
        let setup = format!("{stdenv_path}/setup");

        // The parent bash:
        // 1. Sets minimal env for stdenv sourcing
        // 2. Sources stdenv/setup (populates PATH, defines genericBuild, hooks, etc.)
        // 3. Reads script paths from stdin, one per line
        // 4. For each: forks a subshell with output redirected to temp files
        // 5. Prints __DONE__ <exit_code> on its stdout
        let init = format!(
            r#"
export out=/dev/null
export lib=/dev/null
export outputs="out lib"
export NIX_ENFORCE_PURITY=0
export NIX_STORE=/nix/store
export NIX_BUILD_TOP=/tmp/nib-worker-$$
export TMPDIR=/tmp/nib-worker-$$
export HOME=/homeless-shelter
mkdir -p "$NIX_BUILD_TOP"

source "{setup}"

echo "__READY__"

while IFS= read -r line; do
    # line format: <script_path> <stdout_file> <stderr_file>
    read -r script_path stdout_file stderr_file <<< "$line"
    ( source "$script_path" ) > "$stdout_file" 2> "$stderr_file"
    echo "__DONE__ $?"
done
"#
        );

        let mut child = Command::new(bash)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .arg("-c")
            .arg(&init)
            .spawn()
            .map_err(|e| format!("spawning worker: {e}"))?;

        let stdout = child.stdout.take().unwrap();
        let mut reader = BufReader::new(stdout);

        // Wait for READY
        let mut line = String::new();
        reader.read_line(&mut line)
            .map_err(|e| format!("reading worker ready: {e}"))?;
        if !line.contains("__READY__") {
            return Err(format!("worker didn't signal ready, got: {line}"));
        }

        Ok(Self { child, reader })
    }

    /// Execute a build script in a forked subshell.
    /// `script_path` is the path to the crate's builder.sh.
    /// `tmp_dir` is where stdout/stderr temp files go.
    pub fn execute(&mut self, script_path: &Path, tmp_dir: &Path) -> Result<WorkerBuildResult, String> {
        let stdout_file = tmp_dir.join("worker-stdout");
        let stderr_file = tmp_dir.join("worker-stderr");

        // Ensure temp files exist (subshell redirect needs them)
        std::fs::write(&stdout_file, b"").map_err(|e| format!("creating stdout file: {e}"))?;
        std::fs::write(&stderr_file, b"").map_err(|e| format!("creating stderr file: {e}"))?;

        let stdin = self.child.stdin.as_mut()
            .ok_or("worker stdin closed")?;

        // Send: script_path stdout_file stderr_file
        writeln!(stdin, "{} {} {}",
            script_path.display(),
            stdout_file.display(),
            stderr_file.display(),
        ).map_err(|e| format!("writing to worker: {e}"))?;
        stdin.flush().map_err(|e| format!("flushing worker stdin: {e}"))?;

        // Read __DONE__ <exit_code>
        let mut line = String::new();
        self.reader.read_line(&mut line)
            .map_err(|e| format!("reading worker result: {e}"))?;

        let exit_code = if let Some(code_str) = line.strip_prefix("__DONE__ ") {
            code_str.trim().parse::<i32>().unwrap_or(-1)
        } else {
            return Err(format!("unexpected worker output: {line}"));
        };

        let stdout = std::fs::read_to_string(&stdout_file).unwrap_or_default();
        let stderr = std::fs::read_to_string(&stderr_file).unwrap_or_default();

        Ok(WorkerBuildResult { exit_code, stdout, stderr })
    }

    /// Check if the worker process is still alive.
    pub fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        drop(self.child.stdin.take());
        let _ = self.child.wait();
    }
}
