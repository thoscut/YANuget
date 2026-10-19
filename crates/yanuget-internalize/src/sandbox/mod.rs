//! Where package scripts run: a disposable, offline Windows environment with
//! Chocolatey's download and install helpers replaced by recorders.
//!
//! Everything else (planning, parsing the capture, downloading, rewriting,
//! publishing) only sees the [`Sandbox`] trait, so it builds and is tested on
//! Linux against [`fake::FakeSandbox`]. The backends are the KVM runner
//! ([`kvm`]) and Windows containers ([`windows_container`]); both must
//! produce the same capture for the same package.

pub mod fake;
pub mod kvm;
pub mod windows_container;

use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use yanuget_pkg::report::{Environment, PackageRef};

use crate::config::{Backend, SandboxConfig};

/// One capture: install `package` from a local source holding its closure,
/// with every download recorded instead of made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureJob {
    /// The root package, installed as `choco install {id} --version {v}`.
    pub package: PackageRef,
    /// The `.nupkg` files of the closure: the root, its dependencies and the
    /// extensions their scripts use. The only source the install sees.
    pub sources: Vec<PathBuf>,
    /// From restore to power-off.
    pub timeout: Duration,
    /// The most the capture may hold; more is [`SandboxError::CaptureTooLarge`].
    pub max_capture_bytes: u64,
    /// Passed as `--params` to the root's install: from an override
    /// (`INT-25`), one line, never a shell word of its own.
    pub package_parameters: Option<String>,
}

/// What came back from a capture, as the sandbox found it.
///
/// Untrusted: code from the package wrote `calls` and `log`. Nothing here is
/// parsed or repaired by the backend; that is the capture parser's job.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RawCapture {
    /// Chocolatey's exit code, when the install finished in time.
    pub exit_code: Option<i32>,
    /// The wall-clock limit ended the install.
    pub timed_out: bool,
    /// The capture helpers' output: one JSON object per line.
    pub calls: Vec<u8>,
    /// The tail of Chocolatey's output, for the report and for people.
    pub log: String,
    /// What the install ran in: image hash and Chocolatey version.
    pub environment: Environment,
}

/// Why a sandbox gave no capture.
#[derive(Debug, thiserror::Error)]
pub enum SandboxError {
    /// The backend cannot run on this host or with this configuration.
    #[error("sandbox unavailable: {0}")]
    Unavailable(String),
    /// The capture is larger than the job allows.
    #[error("the capture exceeds {limit} bytes")]
    CaptureTooLarge { limit: u64 },
    /// The sandbox itself failed (not the package's scripts, whose failure
    /// is an exit code in [`RawCapture`]).
    #[error("sandbox failed: {0}")]
    Failed(String),
    #[error("sandbox I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// A place to run package scripts.
#[async_trait]
pub trait Sandbox: Send + Sync {
    /// The backend's name, for logs and the report.
    fn name(&self) -> &'static str;

    /// Run one capture. Whatever happens, the environment it ran in is gone
    /// afterwards.
    async fn capture(&self, job: &CaptureJob) -> Result<RawCapture, SandboxError>;
}

/// The backend the configuration names. Refuses one that cannot run on this
/// host, so a run stops before it fetches anything.
pub fn from_config(config: &SandboxConfig) -> Result<Box<dyn Sandbox>, SandboxError> {
    Ok(match config.backend {
        Backend::Kvm => Box::new(kvm::KvmSandbox::new(config.kvm.clone())?),
        Backend::WindowsContainer => Box::new(windows_container::WindowsContainerSandbox::new(
            config.windows_container.clone(),
        )?),
    })
}

/// The job for `package` under `config`'s limits.
pub fn job(config: &SandboxConfig, package: PackageRef, sources: Vec<PathBuf>) -> CaptureJob {
    CaptureJob {
        package,
        sources,
        timeout: Duration::from_secs(config.timeout_secs),
        max_capture_bytes: config.max_capture_bytes,
        package_parameters: None,
    }
}
