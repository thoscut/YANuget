//! The Windows container backend: a Server Core container under Hyper-V
//! isolation with no network, for operators who have a Windows host instead
//! of KVM. The runner itself is `INT-29` in ROADMAP.md; until it lands this
//! backend checks its host and refuses every capture.

use async_trait::async_trait;

use super::{CaptureJob, RawCapture, Sandbox, SandboxError};
use crate::config::WindowsContainerConfig;

#[derive(Debug)]
pub struct WindowsContainerSandbox {
    #[allow(dead_code)] // read by the runner (INT-29)
    config: WindowsContainerConfig,
}

impl WindowsContainerSandbox {
    pub fn new(config: WindowsContainerConfig) -> Result<Self, SandboxError> {
        if !cfg!(windows) {
            return Err(SandboxError::Unavailable(
                "the windows-container backend needs a Windows host".into(),
            ));
        }
        if config.image.is_none() {
            return Err(SandboxError::Unavailable(
                "sandbox.windows_container.image is not set".into(),
            ));
        }
        Ok(WindowsContainerSandbox { config })
    }
}

#[async_trait]
impl Sandbox for WindowsContainerSandbox {
    fn name(&self) -> &'static str {
        "windows-container"
    }

    async fn capture(&self, _job: &CaptureJob) -> Result<RawCapture, SandboxError> {
        Err(SandboxError::Unavailable(
            "the windows-container runner is not built yet".into(),
        ))
    }
}

/// Whether `image` is pinned by digest: `name@sha256:` and 64 hex digits.
pub fn is_pinned(image: &str) -> bool {
    image.rsplit_once("@sha256:").is_some_and(|(name, hex)| {
        !name.is_empty() && hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit())
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn only_a_digest_pins_an_image() {
        let digest = "a".repeat(64);
        assert!(super::is_pinned(&format!(
            "example.com/internalize@sha256:{digest}"
        )));
        assert!(super::is_pinned(&format!(
            "internalize:1.0@sha256:{digest}"
        )));
        assert!(!super::is_pinned("example.com/internalize:latest"));
        assert!(!super::is_pinned(&format!("@sha256:{digest}")));
        assert!(!super::is_pinned("internalize@sha256:abc"));
    }
}
