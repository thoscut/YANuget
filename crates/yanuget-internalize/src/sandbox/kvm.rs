//! The KVM backend: a Windows Server Core guest under QEMU, restored from the
//! capture image's saved state onto a throwaway overlay, with no network
//! device. The runner itself is `INT-15` in ROADMAP.md; until it lands this
//! backend checks its host and refuses every capture.

use std::path::{Path, PathBuf};

use async_trait::async_trait;

use super::{CaptureJob, RawCapture, Sandbox, SandboxError};
use crate::config::KvmConfig;

/// The device KVM is reached through.
pub const KVM_DEVICE: &str = "/dev/kvm";

/// The first four bytes of every qcow2 image.
pub const QCOW2_MAGIC: [u8; 4] = *b"QFI\xfb";

#[derive(Debug)]
pub struct KvmSandbox {
    #[allow(dead_code)] // read by the runner (INT-15)
    config: KvmConfig,
}

impl KvmSandbox {
    /// Refuses to start without `/dev/kvm`: software emulation is too slow to
    /// be useful.
    pub fn new(config: KvmConfig) -> Result<Self, SandboxError> {
        if !cfg!(target_os = "linux") {
            return Err(SandboxError::Unavailable(
                "the KVM backend needs a Linux host".into(),
            ));
        }
        if !Path::new(KVM_DEVICE).exists() {
            return Err(SandboxError::Unavailable(format!(
                "{KVM_DEVICE} does not exist; the KVM backend needs hardware virtualization"
            )));
        }
        Ok(KvmSandbox { config })
    }
}

#[async_trait]
impl Sandbox for KvmSandbox {
    fn name(&self) -> &'static str {
        "kvm"
    }

    async fn capture(&self, _job: &CaptureJob) -> Result<RawCapture, SandboxError> {
        Err(SandboxError::Unavailable(
            "the KVM runner is not built yet".into(),
        ))
    }
}

/// Whether the file at `path` starts like a qcow2 image.
pub fn is_qcow2(path: &Path) -> std::io::Result<bool> {
    use std::io::Read;
    let mut magic = [0u8; 4];
    let mut file = std::fs::File::open(path)?;
    match file.read_exact(&mut magic) {
        Ok(()) => Ok(magic == QCOW2_MAGIC),
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Ok(false),
        Err(e) => Err(e),
    }
}

/// `program` as a path: itself when it names one, else the first match in
/// `PATH`.
pub fn find_program(program: &Path) -> Option<PathBuf> {
    if program.components().count() > 1 {
        return program.is_file().then(|| program.to_path_buf());
    }
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(program))
        .find(|p| p.is_file())
}
