//! Bounded native subprocess runner with stdin streaming, output caps, and hard deadlines.
use context_graph_core::{Result, RuntimeError};
use std::{
    collections::BTreeMap,
    ffi::OsString,
    io::{Read, Write},
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

#[derive(Clone, Debug, Default)]
pub struct ProcessOutput {
    pub code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

fn read_capped<R: Read + Send + 'static>(
    reader: R,
    limit: usize,
) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = reader.take(limit as u64).read_to_end(&mut buf);
        buf
    })
}

pub fn run_with_env(
    program: &OsString,
    args: &[OsString],
    cwd: &Path,
    stdin_bytes: Option<&[u8]>,
    timeout: Duration,
    output_limit: usize,
    env: &BTreeMap<OsString, Option<OsString>>,
) -> Result<ProcessOutput> {
    if timeout.is_zero() {
        return Err(RuntimeError::invalid("Process timeout must be positive"));
    }
    let mut cmd = Command::new(program);
    cmd.args(args)
        .current_dir(cwd)
        .stdin(if stdin_bytes.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in env {
        match v {
            Some(val) => {
                cmd.env(k, val);
            }
            None => {
                cmd.env_remove(k);
            }
        }
    }
    let mut child = cmd.spawn().map_err(|err| {
        RuntimeError::unavailable(format!(
            "Failed to launch {}: {err}",
            program.to_string_lossy()
        ))
    })?;

    if let (Some(bytes), Some(mut child_in)) = (stdin_bytes, child.stdin.take()) {
        let data = bytes.to_vec();
        std::thread::spawn(move || {
            let _ = child_in.write_all(&data);
        });
    }

    let stdout_handle = child.stdout.take().map(|r| read_capped(r, output_limit));
    let stderr_handle = child.stderr.take().map(|r| read_capped(r, output_limit));

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait()? {
            Some(status) => break Some(status),
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            None => std::thread::sleep(Duration::from_millis(10)),
        }
    };

    let stdout = stdout_handle
        .and_then(|h| h.join().ok())
        .unwrap_or_default();
    let stderr = stderr_handle
        .and_then(|h| h.join().ok())
        .unwrap_or_default();

    let Some(status) = status else {
        let mut err = RuntimeError::unavailable("Subprocess exceeded deadline");
        err.code = Some("native-timeout".into());
        return Err(err);
    };

    Ok(ProcessOutput {
        code: status.code().unwrap_or(1),
        stdout,
        stderr,
    })
}
