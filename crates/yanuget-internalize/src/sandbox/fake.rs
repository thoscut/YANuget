//! A sandbox that runs nothing and answers from a script: for tests of
//! everything around the sandbox, on any host.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;

use super::{CaptureJob, RawCapture, Sandbox, SandboxError};

/// What the fake answers for one package.
#[derive(Debug, Clone)]
enum Answer {
    Capture(RawCapture),
    Fail(String),
}

/// Answers each job from the capture scripted for its package, and keeps
/// every job it was given. A package with nothing scripted is a sandbox
/// failure.
#[derive(Debug, Default)]
pub struct FakeSandbox {
    answers: HashMap<(String, String), Answer>,
    jobs: Mutex<Vec<CaptureJob>>,
}

impl FakeSandbox {
    pub fn new() -> Self {
        Self::default()
    }

    /// Answer `id` `version` with `capture`. The id matches case-insensitively,
    /// as NuGet ids do.
    pub fn with(mut self, id: &str, version: &str, capture: RawCapture) -> Self {
        self.answers
            .insert(key(id, version), Answer::Capture(capture));
        self
    }

    /// Fail the capture of `id` `version` the way a broken sandbox would.
    pub fn failing(mut self, id: &str, version: &str, why: &str) -> Self {
        self.answers
            .insert(key(id, version), Answer::Fail(why.to_string()));
        self
    }

    /// The jobs given so far, in order.
    pub fn jobs(&self) -> Vec<CaptureJob> {
        self.jobs.lock().expect("jobs lock").clone()
    }
}

fn key(id: &str, version: &str) -> (String, String) {
    (id.to_ascii_lowercase(), version.to_string())
}

#[async_trait]
impl Sandbox for FakeSandbox {
    fn name(&self) -> &'static str {
        "fake"
    }

    async fn capture(&self, job: &CaptureJob) -> Result<RawCapture, SandboxError> {
        self.jobs.lock().expect("jobs lock").push(job.clone());
        match self
            .answers
            .get(&key(&job.package.id, &job.package.version))
        {
            // The cap is the backend's to enforce, so the fake enforces it too.
            Some(Answer::Capture(c)) if c.calls.len() as u64 > job.max_capture_bytes => {
                Err(SandboxError::CaptureTooLarge {
                    limit: job.max_capture_bytes,
                })
            }
            Some(Answer::Capture(c)) => Ok(c.clone()),
            Some(Answer::Fail(why)) => Err(SandboxError::Failed(why.clone())),
            None => Err(SandboxError::Failed(format!(
                "nothing scripted for {} {}",
                job.package.id, job.package.version
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use yanuget_pkg::report::PackageRef;

    fn job(id: &str, version: &str, cap: u64) -> CaptureJob {
        CaptureJob {
            package: PackageRef {
                id: id.into(),
                version: version.into(),
            },
            sources: vec![],
            timeout: Duration::from_secs(1),
            max_capture_bytes: cap,
        }
    }

    #[tokio::test]
    async fn answers_from_its_script_and_keeps_the_jobs() {
        let capture = RawCapture {
            exit_code: Some(0),
            calls: b"{\"url\":\"https://example.com/a.msi\"}\n".to_vec(),
            ..RawCapture::default()
        };
        let sandbox = FakeSandbox::new()
            .with("Git.Install", "2.51.0", capture.clone())
            .failing("broken", "1.0.0", "guest agent gone");

        assert_eq!(
            sandbox
                .capture(&job("git.install", "2.51.0", 1024))
                .await
                .unwrap(),
            capture
        );
        assert!(matches!(
            sandbox.capture(&job("git.install", "2.51.0", 8)).await,
            Err(SandboxError::CaptureTooLarge { limit: 8 })
        ));
        assert!(matches!(
            sandbox.capture(&job("broken", "1.0.0", 1024)).await,
            Err(SandboxError::Failed(m)) if m == "guest agent gone"
        ));
        assert!(sandbox.capture(&job("other", "1.0.0", 1024)).await.is_err());
        let ids: Vec<String> = sandbox.jobs().into_iter().map(|j| j.package.id).collect();
        assert_eq!(ids, ["git.install", "git.install", "broken", "other"]);
    }
}
