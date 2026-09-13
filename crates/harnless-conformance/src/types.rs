//! The violation record shared by every suite.

/// One contract obligation a provider failed.
///
/// `case` names the suite case (stable, machine-matchable); `detail`
/// explains the observed behavior for a human.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    /// The suite case this violation was found in.
    pub case: String,
    /// What the provider did instead of honoring the contract.
    pub detail: String,
}

impl Violation {
    /// Build a violation for `case` with `detail`.
    pub fn new(case: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            case: case.into(),
            detail: detail.into(),
        }
    }
}

impl std::fmt::Display for Violation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.case, self.detail)
    }
}
