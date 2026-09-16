use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::manifest::RateLimit;
use crate::{Error, Result};

const GLAB_TIMEOUT: Duration = Duration::from_secs(30);

fn is_not_found(diagnostic: &str) -> bool {
    diagnostic.contains("HTTP 404")
}

pub trait GitlabApi {
    fn get(&mut self, endpoint: &str) -> Result<Value>;
    fn requests(&self) -> u64;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GlabCommand {
    pub executable: &'static str,
    pub args: Vec<String>,
}

pub fn command_spec(host: &str, endpoint: &str) -> Result<GlabCommand> {
    if !(endpoint.starts_with("projects/") || endpoint.starts_with("todos?"))
        || endpoint.contains("://")
        || endpoint.split('/').any(|part| part == "..")
    {
        return Err(Error::Remote(
            "refused invalid internally generated GitLab endpoint".to_owned(),
        ));
    }
    Ok(GlabCommand {
        executable: option_env!("FORGE_SYNC_GLAB_PATH").unwrap_or("glab"),
        args: vec![
            "api".to_owned(),
            "--hostname".to_owned(),
            host.to_owned(),
            endpoint.to_owned(),
        ],
    })
}

pub struct GlabClient {
    host: String,
    requests: u64,
}

impl GlabClient {
    pub fn new(host: String) -> Self {
        Self { host, requests: 0 }
    }
}

impl GitlabApi for GlabClient {
    fn get(&mut self, endpoint: &str) -> Result<Value> {
        let spec = command_spec(&self.host, endpoint)?;
        let mut child = Command::new(spec.executable)
            .args(&spec.args)
            .current_dir("/")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|_| Error::Remote("could not start Nix-provided glab".to_owned()))?;
        self.requests += 1;
        let mut stdout = child.stdout.take().expect("piped stdout");
        let mut stderr = child.stderr.take().expect("piped stderr");
        let out_thread = thread::spawn(move || {
            let mut data = Vec::new();
            stdout.read_to_end(&mut data).map(|_| data)
        });
        let err_thread = thread::spawn(move || {
            let mut bounded = Vec::new();
            let mut buffer = [0_u8; 4096];
            loop {
                let count = stderr.read(&mut buffer)?;
                if count == 0 {
                    break;
                }
                let available = 8192_usize.saturating_sub(bounded.len());
                bounded.write_all(&buffer[..count.min(available)])?;
            }
            Ok::<_, std::io::Error>(bounded)
        });
        let deadline = Instant::now() + GLAB_TIMEOUT;
        let (status, timed_out) = loop {
            if let Some(status) = child
                .try_wait()
                .map_err(|_| Error::Remote("waiting for glab failed".to_owned()))?
            {
                break (status, false);
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let status = child
                    .wait()
                    .map_err(|_| Error::Remote("terminating timed-out glab failed".to_owned()))?;
                break (status, true);
            }
            thread::sleep(Duration::from_millis(50));
        };
        let stdout = out_thread
            .join()
            .map_err(|_| Error::Remote("reading glab output failed".to_owned()))?
            .map_err(|_| Error::Remote("reading glab output failed".to_owned()))?;
        let stderr = err_thread
            .join()
            .map_err(|_| Error::Remote("reading glab diagnostic failed".to_owned()))?
            .map_err(|_| Error::Remote("reading glab diagnostic failed".to_owned()))?;
        if timed_out {
            return Err(Error::Remote(
                "glab request timed out after 30 seconds".to_owned(),
            ));
        }
        if !status.success() {
            let diagnostic = String::from_utf8_lossy(&stderr);
            if is_not_found(&diagnostic) {
                return Err(Error::RemoteNotFound);
            }
            let first_line: String = diagnostic
                .lines()
                .next()
                .unwrap_or("no diagnostic")
                .chars()
                .take(300)
                .collect();
            return Err(Error::Remote(format!(
                "glab exited with {status}: {first_line}"
            )));
        }
        serde_json::from_slice(&stdout).map_err(|_| Error::Json {
            context: "GitLab response".to_owned(),
        })
    }

    fn requests(&self) -> u64 {
        self.requests
    }
}

pub fn encode_project(project: &str) -> String {
    project
        .bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.') {
                (byte as char).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect()
}

#[allow(dead_code)]
fn _rate_limit_type_is_shared(_: RateLimit) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glab_command_has_exact_safe_shape() {
        let command = command_spec("gl.example.com", "projects/x/issues?page=1").unwrap();
        assert_eq!(
            command.args,
            [
                "api",
                "--hostname",
                "gl.example.com",
                "projects/x/issues?page=1"
            ]
        );
        for forbidden in ["--method", "-X", "--field", "--raw-field"] {
            assert!(!command.args.iter().any(|arg| arg == forbidden));
        }
    }

    #[test]
    fn recognizes_only_http_404_as_not_found() {
        assert!(is_not_found("glab: 404 Not found (HTTP 404)"));
        assert!(!is_not_found("glab: 403 Forbidden (HTTP 403)"));
        assert_eq!(GLAB_TIMEOUT, Duration::from_secs(30));
    }
}
