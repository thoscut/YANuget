//! How a run ends, as an exit code a pipeline can branch on.
//!
//! | Code | Meaning |
//! | --- | --- |
//! | 0 | Done: every package was internalized or needed nothing. |
//! | 1 | Failed: the run, or at least one package, failed. |
//! | 2 | Usage: the command line was wrong (clap's own code). |
//! | 3 | Needs attention: nothing failed, but a package needs an override. |

use std::process::ExitCode;

use yanuget_pkg::report::Status;

/// The run's result, ordered from best to worst.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Outcome {
    Done,
    NeedsAttention,
    Failed,
}

impl Outcome {
    pub const fn code(self) -> u8 {
        match self {
            Outcome::Done => 0,
            Outcome::Failed => 1,
            Outcome::NeedsAttention => 3,
        }
    }

    /// The outcome as the JSON report names it.
    pub const fn as_str(self) -> &'static str {
        match self {
            Outcome::Done => "done",
            Outcome::NeedsAttention => "needs-attention",
            Outcome::Failed => "failed",
        }
    }

    /// What one package's status means for the run. A status this version
    /// does not know is a failure: nothing it cannot read is called done.
    pub fn of(status: Status) -> Self {
        match status {
            Status::Internalized | Status::Unchanged => Outcome::Done,
            Status::NeedsOverride => Outcome::NeedsAttention,
            Status::Failed | Status::Unknown => Outcome::Failed,
        }
    }

    /// The run's outcome: its worst package's. No packages is done.
    pub fn of_all(statuses: impl IntoIterator<Item = Status>) -> Self {
        statuses
            .into_iter()
            .map(Outcome::of)
            .max()
            .unwrap_or(Outcome::Done)
    }
}

impl From<Outcome> for ExitCode {
    fn from(o: Outcome) -> Self {
        ExitCode::from(o.code())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_worst_package_decides() {
        use Status::*;
        assert_eq!(Outcome::of_all([]), Outcome::Done);
        assert_eq!(Outcome::of_all([Internalized, Unchanged]), Outcome::Done);
        assert_eq!(
            Outcome::of_all([Internalized, NeedsOverride, Unchanged]),
            Outcome::NeedsAttention
        );
        assert_eq!(
            Outcome::of_all([NeedsOverride, Failed, Internalized]),
            Outcome::Failed
        );
        assert_eq!(Outcome::of_all([Unknown]), Outcome::Failed);
    }

    #[test]
    fn codes_are_distinct_and_leave_2_to_clap() {
        let codes = [Outcome::Done, Outcome::NeedsAttention, Outcome::Failed].map(Outcome::code);
        assert_eq!(codes, [0, 3, 1]);
    }
}
