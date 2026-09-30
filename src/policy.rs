//! Offline package policy evaluation.
//!
//! The only policy implemented here is **license** allow/deny, evaluated from a
//! package's SPDX `licenseExpression` (or the legacy `licenseUrl`). It needs no
//! network access — everything is decided from the manifest and the feed's
//! configured allow/deny lists. The result is a [`PolicyOutcome`]: a package may
//! be *allowed* (optionally with a recorded violation, when the feed only warns)
//! or *rejected* (when the feed blocks).
//!
//! A deny list on its own is advisory. It can only catch what a package
//! declares in a form it recognises: a package whose license is a file, or a
//! `licenseUrl` the list does not name, or an id nobody thought to deny,
//! passes it. Only an allow list says what *may* come in.

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
    let rules: Vec<Term> = allowed.iter().map(|a| Term::parse(a)).collect();
    // OR binds looser than AND: the expression is a disjunction of conjunctions.
    split_operator(license, "OR")
        .into_iter()
        .any(|alternative| {
            let conjuncts = split_operator(&alternative, "AND");
            !conjuncts.is_empty()
                && conjuncts.iter().all(|term| {
                    let term = Term::parse(term);
                    rules.iter().any(|rule| rule.allows(&term))
                })
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

/// One SPDX license term, `id` or `id WITH exception`, normalised so that the
/// spellings SPDX treats as the same licence compare equal: case, the `+`
/// suffix (`GPL-2.0+` is `GPL-2.0-or-later`), and the deprecated ids
/// (`GPL-2.0` is `GPL-2.0-only`, `GPL-2.0-with-classpath-exception` is
/// `GPL-2.0-only WITH Classpath-exception-2.0`). Without this a rule for
/// `GPL-2.0` did not match a package declaring `GPL-2.0+`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Term {
    /// The licence id, lower-cased.
    id: String,
    /// The exception after `WITH`, lower-cased.
    exception: Option<String>,
    /// More than one `WITH`, or `WITH` with nothing after it.
    malformed: bool,
}

impl Term {
    fn parse(raw: &str) -> Self {
        let parts = split_operator(raw, "WITH");
        let malformed = parts.len() > 2
            || parts.is_empty()
            || (parts.len() < 2
                && raw
                    .split_whitespace()
                    .any(|t| t.eq_ignore_ascii_case("WITH")));
        let (id, implied) = normalize_id(parts.first().map(String::as_str).unwrap_or(""));
        let exception = parts
            .get(1)
            .map(|e| e.to_ascii_lowercase())
            .or(implied.map(str::to_ascii_lowercase));
        Self {
            id,
            exception,
            malformed,
        }
    }

    /// The licence without its version qualifier: `gpl-2.0` for both
    /// `GPL-2.0-only` and `GPL-2.0-or-later`.
    fn family(&self) -> &str {
        family(&self.id)
    }

    /// Whether an exception, if any, is one SPDX defines. An allow-list names
    /// licences, and an exception only relaxes one — but only a real
    /// exception does. `MIT WITH <anything>` used to pass a list allowing
    /// `MIT`, whatever the text after `WITH` granted or took away.
    fn exception_is_known(&self) -> bool {
        !self.malformed
            && self
                .exception
                .as_deref()
                .is_none_or(|e| KNOWN_EXCEPTIONS.iter().any(|k| k.eq_ignore_ascii_case(e)))
    }

    /// Whether this allow rule admits `term`.
    ///
    /// A rule without an exception admits its licence with any known exception
    /// (an exception only relaxes the base licence); a rule with one admits
    /// exactly that pair. An `-or-later` licence satisfies a rule for the
    /// `-only` form of the same version, since the consumer may pick that
    /// version; the reverse does not hold.
    fn allows(&self, term: &Term) -> bool {
        if self.id.is_empty() || !term.exception_is_known() {
            return false;
        }
        let id_ok = self.id == term.id
            || (self.id.ends_with("-only")
                && term.id.ends_with("-or-later")
                && self.family() == term.family());
        id_ok
            && match &self.exception {
                None => true,
                Some(e) => term.exception.as_deref() == Some(e.as_str()),
            }
    }
}

/// Normalise one licence id: lower-case, `+` as `-or-later`, deprecated ids
/// as their replacements. A deprecated id that folded an exception into its
/// name returns that exception too.
fn normalize_id(raw: &str) -> (String, Option<&'static str>) {
    let raw = raw.trim();
    let (base, or_later) = match raw.strip_suffix('+') {
        Some(base) => (base.trim_end(), true),
        None => (raw, false),
    };
    let (mut id, exception) = match DEPRECATED
        .iter()
        .find(|(old, _, _)| old.eq_ignore_ascii_case(base))
    {
        Some((_, new, exception)) => (new.to_ascii_lowercase(), *exception),
        None => (base.to_ascii_lowercase(), None),
    };
    if or_later {
        id = format!("{}-or-later", family(&id));
    }
    (id, exception)
}

fn family(id: &str) -> &str {
    id.strip_suffix("-only")
        .or_else(|| id.strip_suffix("-or-later"))
        .unwrap_or(id)
}

/// Deprecated SPDX licence ids: `(deprecated, replacement, implied exception)`.
const DEPRECATED: &[(&str, &str, Option<&str>)] = &[
    ("AGPL-1.0", "AGPL-1.0-only", None),
    ("AGPL-3.0", "AGPL-3.0-only", None),
    ("GFDL-1.1", "GFDL-1.1-only", None),
    ("GFDL-1.2", "GFDL-1.2-only", None),
    ("GFDL-1.3", "GFDL-1.3-only", None),
    ("GPL-1.0", "GPL-1.0-only", None),
    ("GPL-2.0", "GPL-2.0-only", None),
    ("GPL-3.0", "GPL-3.0-only", None),
    ("LGPL-2.0", "LGPL-2.0-only", None),
    ("LGPL-2.1", "LGPL-2.1-only", None),
    ("LGPL-3.0", "LGPL-3.0-only", None),
    (
        "GPL-2.0-with-autoconf-exception",
        "GPL-2.0-only",
        Some("Autoconf-exception-2.0"),
    ),
    (
        "GPL-2.0-with-bison-exception",
        "GPL-2.0-only",
        Some("Bison-exception-2.2"),
    ),
    (
        "GPL-2.0-with-classpath-exception",
        "GPL-2.0-only",
        Some("Classpath-exception-2.0"),
    ),
    (
        "GPL-2.0-with-font-exception",
        "GPL-2.0-only",
        Some("Font-exception-2.0"),
    ),
    (
        "GPL-2.0-with-GCC-exception",
        "GPL-2.0-only",
        Some("GCC-exception-2.0"),
    ),
    (
        "GPL-3.0-with-autoconf-exception",
        "GPL-3.0-only",
        Some("Autoconf-exception-3.0"),
    ),
    (
        "GPL-3.0-with-GCC-exception",
        "GPL-3.0-only",
        Some("GCC-exception-3.1"),
    ),
    ("eCos-2.0", "GPL-2.0-or-later", Some("eCos-exception-2.0")),
    (
        "wxWindows",
        "LGPL-2.0-or-later",
        Some("WxWindows-exception-3.1"),
    ),
    ("BSD-2-Clause-FreeBSD", "BSD-2-Clause", None),
    ("BSD-2-Clause-NetBSD", "BSD-2-Clause", None),
    ("bzip2-1.0.5", "bzip2-1.0.6", None),
    ("Nunit", "zlib-acknowledgement", None),
    ("StandardML-NJ", "SMLNJ", None),
];

/// The exception ids of the SPDX License List. Only these are accepted after
/// `WITH` by an allow-list; an expression can name the pair exactly in a rule
/// to accept anything else.
const KNOWN_EXCEPTIONS: &[&str] = &[
    "389-exception",
    "Asterisk-exception",
    "Asterisk-linking-protocols-exception",
    "Autoconf-exception-2.0",
    "Autoconf-exception-3.0",
    "Autoconf-exception-generic",
    "Autoconf-exception-generic-3.0",
    "Autoconf-exception-macro",
    "Bison-exception-1.24",
    "Bison-exception-2.2",
    "Bootloader-exception",
    "CGAL-linking-exception",
    "Classpath-exception-2.0",
    "Classpath-exception-2.0-short",
    "CLISP-exception-2.0",
    "cryptsetup-OpenSSL-exception",
    "Digia-Qt-LGPL-exception-1.1",
    "DigiRule-FOSS-exception",
    "eCos-exception-2.0",
    "erlang-otp-linking-exception",
    "Fawkes-Runtime-exception",
    "FLTK-exception",
    "fmt-exception",
    "Font-exception-2.0",
    "freertos-exception-2.0",
    "GCC-exception-2.0",
    "GCC-exception-2.0-note",
    "GCC-exception-3.1",
    "Gmsh-exception",
    "GNAT-exception",
    "GNOME-examples-exception",
    "GNU-compiler-exception",
    "gnu-javamail-exception",
    "GPL-3.0-389-ds-base-exception",
    "GPL-3.0-interface-exception",
    "GPL-3.0-linking-exception",
    "GPL-3.0-linking-source-exception",
    "GPL-CC-1.0",
    "GStreamer-exception-2005",
    "GStreamer-exception-2008",
    "harbour-exception",
    "i2p-gpl-java-exception",
    "Independent-modules-exception",
    "KiCad-libraries-exception",
    "LGPL-3.0-linking-exception",
    "libpri-OpenH323-exception",
    "Libtool-exception",
    "Linux-syscall-note",
    "LLGPL",
    "LLVM-exception",
    "LZMA-exception",
    "mif-exception",
    "mxml-exception",
    "Nokia-Qt-exception-1.1",
    "OCaml-LGPL-linking-exception",
    "OCCT-exception-1.0",
    "OpenJDK-assembly-exception-1.0",
    "openvpn-openssl-exception",
    "PCRE2-exception",
    "polyparse-exception",
    "PS-or-PDF-font-exception-20170817",
    "QPL-1.0-INRIA-2004-exception",
    "Qt-GPL-exception-1.0",
    "Qt-LGPL-exception-1.1",
    "Qwt-exception-1.0",
    "romic-exception",
    "RRDtool-FLOSS-exception-2.0",
    "SANE-exception",
    "SHL-2.0",
    "SHL-2.1",
    "stunnel-exception",
    "SWI-exception",
    "Swift-exception",
    "Texinfo-exception",
    "u-boot-exception-2.0",
    "UBDL-exception",
    "Universal-FOSS-exception-1.0",
    "vsftpd-openssl-exception",
    "WxWindows-exception-3.1",
    "x11vnc-openssl-exception",
];

fn violation(policy: &LicensePolicyConfig, reason: String) -> PolicyOutcome {
    PolicyOutcome {
        allowed: policy.action != PolicyAction::Block,
        violation: Some(reason),
    }
}

/// Whether the deny rule `rule` matches `license`, case-insensitively.
///
/// Beyond an exact match, a single-identifier rule (e.g. `GPL-2.0`) matches
/// any licence of the same family anywhere in a compound expression: `-only`,
/// `-or-later`, `+` and the deprecated spelling are one licence as far as a
/// deny list is concerned, since denying `GPL-2.0` and letting `GPL-2.0+`
/// through defeats the rule. Exceptions after `WITH` do not shield the
/// licence they modify.
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
    let (rule_id, _) = normalize_id(rule);
    let rule_family = family(&rule_id);
    let mut after_with = false;
    for token in license
        .split(|c: char| c.is_whitespace() || matches!(c, '(' | ')'))
        .map(str::trim)
        .filter(|t| !t.is_empty())
    {
        if token.eq_ignore_ascii_case("WITH") {
            after_with = true;
            continue;
        }
        if token.eq_ignore_ascii_case("OR") || token.eq_ignore_ascii_case("AND") {
            continue;
        }
        if std::mem::take(&mut after_with) {
            // An exception id: only an exact rule names it.
            if token.eq_ignore_ascii_case(rule) {
                return true;
            }
            continue;
        }
        let (id, _) = normalize_id(token);
        if family(&id) == rule_family {
            return true;
        }
    }
    false
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

    fn blocking(allowed: &[&str], blocked: &[&str]) -> LicensePolicyConfig {
        LicensePolicyConfig {
            enabled: true,
            allowed: allowed.iter().map(|s| s.to_string()).collect(),
            blocked: blocked.iter().map(|s| s.to_string()).collect(),
            action: PolicyAction::Block,
            ..Default::default()
        }
    }

    fn allows(policy: &LicensePolicyConfig, expression: &str) -> bool {
        evaluate_license(policy, &pkg_with_license(Some(expression), None)).allowed
    }

    #[test]
    fn a_deny_rule_matches_every_spelling_of_its_licence() {
        let deny = blocking(&[], &["GPL-2.0"]);
        for expression in [
            "GPL-2.0",
            "GPL-2.0+",
            "gpl-2.0-only",
            "GPL-2.0-or-later",
            "MIT OR GPL-2.0+",
            "(MIT AND GPL-2.0-only)",
            "GPL-2.0-only WITH Classpath-exception-2.0",
            "GPL-2.0-with-classpath-exception",
        ] {
            assert!(
                !allows(&deny, expression),
                "{expression:?} passed the deny list"
            );
        }
        for expression in ["MIT", "GPL-3.0-only", "LGPL-2.0-only", "GPL-2.0-X"] {
            assert!(allows(&deny, expression), "{expression:?} was denied");
        }
        // Any spelling of the rule works the same way.
        assert!(!allows(&blocking(&[], &["GPL-3.0-or-later"]), "GPL-3.0"));
    }

    #[test]
    fn an_allow_rule_accepts_equivalent_spellings_only() {
        let allow = blocking(&["GPL-2.0-only", "MIT"], &[]);
        for ok in ["GPL-2.0", "GPL-2.0-only", "GPL-2.0+", "GPL-2.0-or-later"] {
            assert!(allows(&allow, ok), "{ok:?} should be allowed");
        }
        // `-only` does not satisfy a rule that allows the later versions'
        // terms, and a different version is a different licence.
        let later = blocking(&["GPL-2.0-or-later"], &[]);
        assert!(!allows(&later, "GPL-2.0-only"));
        assert!(!allows(&allow, "GPL-3.0-only"));
    }

    #[test]
    fn only_known_exceptions_pass_an_allow_list() {
        let allow = blocking(&["MIT", "GPL-2.0-only"], &[]);
        assert!(allows(&allow, "GPL-2.0-only WITH Classpath-exception-2.0"));
        assert!(allows(&allow, "GPL-2.0-with-classpath-exception"));
        assert!(allows(&allow, "MIT WITH llvm-exception"));
        for bad in [
            "MIT WITH Anything-I-Like",
            "MIT WITH",
            "MIT WITH LLVM-exception WITH Classpath-exception-2.0",
        ] {
            assert!(!allows(&allow, bad), "{bad:?} should be rejected");
        }
        // A rule naming the pair exactly still admits it.
        let exact = blocking(&["MIT WITH Custom-exception"], &[]);
        assert!(allows(&exact, "MIT WITH Custom-exception"));
        assert!(!allows(&exact, "MIT"));
    }
}
