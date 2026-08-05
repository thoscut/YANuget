//! Offline package policy evaluation.
//!
//! The only policy implemented here is **license** allow/deny, evaluated from a
//! package's SPDX `licenseExpression` (or the legacy `licenseUrl`). It needs no
//! network access — everything is decided from the manifest and the feed's
//! configured allow/deny lists. The result is a [`PolicyOutcome`]: a package may
//! be *allowed* (optionally with a recorded violation, when the feed only warns)
//! or *rejected* (when the feed blocks).

use crate::config::{LicensePolicyConfig, PolicyAction};
use crate::models::Package;

/// The result of evaluating a feed's policy against a package.
#[derive(Debug, Clone)]
pub struct PolicyOutcome {
    /// Whether the push/mirror may proceed. `false` only under a blocking action.
    pub allowed: bool,
    /// A human-readable violation reason, when the policy was not satisfied.
    /// Present for both "warn" (allowed, flagged) and "block" (rejected).
    pub violation: Option<String>,
}

impl PolicyOutcome {
    fn ok() -> Self {
        Self {
            allowed: true,
            violation: None,
        }
    }
}

/// Evaluate a feed's [`LicensePolicyConfig`] against a package.
pub fn evaluate_license(policy: &LicensePolicyConfig, package: &Package) -> PolicyOutcome {
    if !policy.enabled {
        return PolicyOutcome::ok();
    }

    let license = package
        .license_expression
        .as_deref()
        .or(package.license_url.as_deref())
        .map(str::trim)
        .filter(|s| !s.is_empty());

    let Some(license) = license else {
        return if policy.allow_unlicensed {
            PolicyOutcome::ok()
        } else {
            violation(policy, "package declares no license".to_string())
        };
    };

    if policy.blocked.iter().any(|b| matches_license(b, license)) {
        return violation(policy, format!("license {license:?} is blocked"));
    }
    if !policy.allowed.is_empty() && !expression_is_allowed(&policy.allowed, license) {
        return violation(policy, format!("license {license:?} is not allowed"));
    }
    PolicyOutcome::ok()
}

/// Whether an SPDX expression is satisfied by the allow-list.
///
/// The two SPDX operators mean opposite things for an allow-list, and treating
/// them alike is what makes a naive "does any component match?" check wrong:
///
/// * `A OR B` lets the consumer pick either, so it is allowed if **either** side
///   is allowed.
/// * `A AND B` obliges the consumer to satisfy **both**, so `MIT AND Proprietary`
///   must *not* pass a policy that only permits `MIT` — the package still
///   carries the proprietary terms.
///
/// Precedence with parentheses is not modelled. Rather than guess (and guess
/// permissively), a parenthesised expression is allowed only on an exact match
/// against a configured rule.
fn expression_is_allowed(allowed: &[String], license: &str) -> bool {
    if allowed
        .iter()
        .any(|a| a.trim().eq_ignore_ascii_case(license.trim()))
    {
        return true;
    }
    if license.contains('(') || license.contains(')') {
        return false;
    }
    // OR binds looser than AND: the expression is a disjunction of conjunctions.
    split_operator(license, "OR")
        .into_iter()
        .any(|alternative| {
            let conjuncts = split_operator(&alternative, "AND");
            !conjuncts.is_empty()
                && conjuncts
                    .iter()
                    .all(|term| allowed.iter().any(|a| matches_identifier(a, term)))
        })
}

/// Split an expression on a top-level SPDX operator, case-insensitively.
fn split_operator(expr: &str, op: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current: Vec<&str> = Vec::new();
    for token in expr.split_whitespace() {
        if token.eq_ignore_ascii_case(op) {
            parts.push(current.join(" "));
            current.clear();
        } else {
            current.push(token);
        }
    }
    parts.push(current.join(" "));
    parts
        .into_iter()
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .collect()
}

/// Whether `rule` matches a single SPDX identifier, which may carry a `WITH`
/// exception. An exception only ever *relaxes* the base licence, so a rule
/// naming the base identifier matches the `WITH` form too.
fn matches_identifier(rule: &str, identifier: &str) -> bool {
    let rule = rule.trim();
    let identifier = identifier.trim();
    if rule.eq_ignore_ascii_case(identifier) {
        return true;
    }
    let base = split_operator(identifier, "WITH")
        .into_iter()
        .next()
        .unwrap_or_default();
    !base.is_empty() && rule.eq_ignore_ascii_case(&base)
}

fn violation(policy: &LicensePolicyConfig, reason: String) -> PolicyOutcome {
    PolicyOutcome {
        allowed: policy.action != PolicyAction::Block,
        violation: Some(reason),
    }
}

/// Whether `rule` matches `license`, case-insensitively. Beyond an exact match,
/// a single-identifier rule (e.g. `MIT`) matches any SPDX component of a
/// compound expression (e.g. `MIT OR Apache-2.0`), so simple allow/deny lists
/// behave intuitively.
fn matches_license(rule: &str, license: &str) -> bool {
    let rule = rule.trim();
    if rule.is_empty() {
        return false;
    }
    if rule.eq_ignore_ascii_case(license) {
        return true;
    }
    // Don't expand compound rules; only expand the license into its components.
    if rule.contains(char::is_whitespace) {
        return false;
    }
    license
        .split(|c: char| c.is_whitespace() || matches!(c, '(' | ')'))
        .map(str::trim)
        .filter(|t| {
            !t.is_empty()
                && !t.eq_ignore_ascii_case("OR")
                && !t.eq_ignore_ascii_case("AND")
                && !t.eq_ignore_ascii_case("WITH")
        })
        .any(|tok| tok.eq_ignore_ascii_case(rule))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::version::NuGetVersion;

    fn pkg_with_license(expr: Option<&str>, url: Option<&str>) -> Package {
        Package {
            id: "Test.Pkg".into(),
            version: NuGetVersion::parse("1.0.0").unwrap(),
            listed: true,
            enabled: true,
            authors: vec![],
            description: String::new(),
            icon_url: None,
            license_url: url.map(str::to_string),
            license_expression: expr.map(str::to_string),
            project_url: None,
            repository_url: None,
            repository_type: None,
            min_client_version: None,
            release_notes: None,
            language: None,
            title: None,
            summary: None,
            tags: vec![],
            has_readme: false,
            has_embedded_icon: false,
            is_development_dependency: false,
            require_license_acceptance: false,
            is_semver2: false,
            package_size: 1,
            package_hash: "h".into(),
            package_hash_algorithm: "SHA512".into(),
            published: chrono::Utc::now(),
            downloads: 0,
            package_types: vec![],
            dependencies: vec![],
        }
    }

    #[test]
    fn disabled_policy_allows_everything() {
        let policy = LicensePolicyConfig::default(); // enabled = false
        let out = evaluate_license(&policy, &pkg_with_license(None, None));
        assert!(out.allowed);
        assert!(out.violation.is_none());
    }

    #[test]
    fn allowlist_passes_listed_license_and_flags_others() {
        let policy = LicensePolicyConfig {
            enabled: true,
            allowed: vec!["MIT".into(), "Apache-2.0".into()],
            action: PolicyAction::Warn,
            ..Default::default()
        };
        assert!(
            evaluate_license(&policy, &pkg_with_license(Some("MIT"), None))
                .violation
                .is_none()
        );
        // Compound expression matches by component.
        assert!(
            evaluate_license(&policy, &pkg_with_license(Some("MIT OR GPL-3.0"), None))
                .violation
                .is_none()
        );
        // Not on the list -> flagged but still allowed under "warn".
        let out = evaluate_license(&policy, &pkg_with_license(Some("GPL-3.0-only"), None));
        assert!(out.allowed);
        assert!(out.violation.is_some());
    }

    #[test]
    fn and_requires_every_component_to_be_allowed() {
        let policy = LicensePolicyConfig {
            enabled: true,
            allowed: vec!["MIT".into(), "Apache-2.0".into()],
            action: PolicyAction::Block,
            ..Default::default()
        };

        // `A OR B` lets the consumer pick, so one allowed side is enough.
        for ok in ["MIT", "MIT OR GPL-3.0-only", "GPL-3.0-only OR Apache-2.0"] {
            assert!(
                evaluate_license(&policy, &pkg_with_license(Some(ok), None)).allowed,
                "{ok:?} should be allowed"
            );
        }

        // `A AND B` obliges the consumer to satisfy both. A package licensed
        // "MIT AND Proprietary" still carries the proprietary terms, so a
        // policy permitting only MIT must not let it through.
        for bad in [
            "MIT AND Proprietary",
            "Proprietary AND MIT",
            "GPL-3.0-only AND Apache-2.0",
        ] {
            assert!(
                !evaluate_license(&policy, &pkg_with_license(Some(bad), None)).allowed,
                "{bad:?} should be rejected"
            );
        }

        // Both sides allowed is fine.
        assert!(
            evaluate_license(&policy, &pkg_with_license(Some("MIT AND Apache-2.0"), None)).allowed
        );

        // Parenthesised expressions are not second-guessed: precedence is not
        // modelled, so only an exact rule match allows them.
        assert!(
            !evaluate_license(&policy, &pkg_with_license(Some("(MIT OR X) AND Y"), None)).allowed
        );
        let exact = LicensePolicyConfig {
            allowed: vec!["(MIT OR X) AND Y".into()],
            ..policy.clone()
        };
        assert!(
            evaluate_license(&exact, &pkg_with_license(Some("(MIT OR X) AND Y"), None)).allowed
        );
    }

    #[test]
    fn a_with_exception_matches_its_base_identifier() {
        let policy = LicensePolicyConfig {
            enabled: true,
            allowed: vec!["GPL-2.0-only".into()],
            action: PolicyAction::Block,
            ..Default::default()
        };
        // An exception only relaxes the base licence, so allowing the base
        // allows the `WITH` form.
        assert!(
            evaluate_license(
                &policy,
                &pkg_with_license(Some("GPL-2.0-only WITH Classpath-exception-2.0"), None)
            )
            .allowed
        );
        assert!(!evaluate_license(&policy, &pkg_with_license(Some("GPL-3.0-only"), None)).allowed);
    }

    #[test]
    fn blocklist_rejects_under_block_action() {
        let policy = LicensePolicyConfig {
            enabled: true,
            blocked: vec!["GPL-3.0-only".into()],
            action: PolicyAction::Block,
            ..Default::default()
        };
        let out = evaluate_license(&policy, &pkg_with_license(Some("GPL-3.0-only"), None));
        assert!(!out.allowed);
        assert!(out.violation.is_some());
        // A blocked component inside a compound expression is caught too.
        let out2 = evaluate_license(
            &policy,
            &pkg_with_license(Some("MIT OR GPL-3.0-only"), None),
        );
        assert!(!out2.allowed);
    }

    #[test]
    fn unlicensed_packages_respect_allow_unlicensed() {
        let strict = LicensePolicyConfig {
            enabled: true,
            allow_unlicensed: false,
            action: PolicyAction::Block,
            ..Default::default()
        };
        assert!(!evaluate_license(&strict, &pkg_with_license(None, None)).allowed);

        let lenient = LicensePolicyConfig {
            enabled: true,
            allow_unlicensed: true,
            ..Default::default()
        };
        assert!(evaluate_license(&lenient, &pkg_with_license(None, None))
            .violation
            .is_none());
    }

    #[test]
    fn falls_back_to_license_url() {
        let policy = LicensePolicyConfig {
            enabled: true,
            allowed: vec!["https://example.com/mit".into()],
            ..Default::default()
        };
        let out = evaluate_license(
            &policy,
            &pkg_with_license(None, Some("https://example.com/mit")),
        );
        assert!(out.violation.is_none());
    }
}
