//! The dependency closure of a package: what `choco install` installs along
//! with it, resolved against the upstream before anything is downloaded.
//!
//! The versions are the ones Chocolatey CLI v2 picks. Its `NugetService` runs
//! NuGet's resolver with `DependencyBehavior.Highest` and the dependencies of
//! `AnyFramework`, so each dependency takes the highest version that every
//! range on it allows, backtracking when two ranges clash. Like there:
//!
//! - pre-release versions are candidates only when the root is one, or when
//!   a range pins exactly that version;
//! - an unlisted version is taken only when no listed one fits;
//! - a dependency on `chocolatey` itself is left to the CLI in the image,
//!   since Chocolatey keeps the installed one.
//!
//! Extension packages (`*.extension`) are ordinary dependencies and are part
//! of the closure: scripts call their helpers.

use std::collections::HashMap;

use async_trait::async_trait;
use yanuget_pkg::{NuGetVersion, VersionRange};

use crate::upstream::{DependencyRef, PackageInfo, Upstream, UpstreamError};

/// The most packages one closure may hold.
pub const MAX_PACKAGES: usize = 500;

/// The most choices one resolution may try before it gives up.
const MAX_STEPS: usize = 100_000;

/// The most version lists and manifests one resolution may ask for.
const MAX_LOOKUPS: usize = 2_000;

/// The package Chocolatey CLI itself ships as.
const CLI_PACKAGE: &str = "chocolatey";

#[derive(Debug, thiserror::Error)]
pub enum ClosureError {
    #[error(transparent)]
    Upstream(#[from] UpstreamError),
    /// No set of versions fits, or the metadata cannot be used.
    #[error("{0}")]
    Unresolvable(String),
}

/// Where the closure's metadata comes from: the upstream, or a fixture in
/// tests.
#[async_trait]
pub trait Source: Sync {
    /// Every version of `id`, oldest first; none for an unknown id.
    async fn versions(&self, id: &str) -> Result<Vec<PackageInfo>, UpstreamError>;
    /// What `info` depends on.
    async fn dependencies(&self, info: &PackageInfo) -> Result<Vec<DependencyRef>, UpstreamError>;
}

#[async_trait]
impl Source for Upstream {
    async fn versions(&self, id: &str) -> Result<Vec<PackageInfo>, UpstreamError> {
        Upstream::versions(self, id).await
    }

    async fn dependencies(&self, info: &PackageInfo) -> Result<Vec<DependencyRef>, UpstreamError> {
        Upstream::dependencies(self, info).await
    }
}

/// A root package and everything it needs.
#[derive(Debug, Clone)]
pub struct Closure {
    /// The root first, then its dependencies in the order they were resolved.
    pub packages: Vec<Resolved>,
    /// Dependencies on Chocolatey CLI, which the image provides, as
    /// `(dependent, range)`.
    pub provided: Vec<(String, String)>,
}

/// One package of a closure.
#[derive(Debug, Clone)]
pub struct Resolved {
    pub info: PackageInfo,
    /// The packages that depend on it, as `id version`; empty for the root.
    pub required_by: Vec<String>,
}

/// Resolve the closure of `root`, a version the upstream lists.
pub async fn resolve(source: &dyn Source, root: &PackageInfo) -> Result<Closure, ClosureError> {
    let mut known = Known::default();
    for _ in 0..MAX_LOOKUPS {
        let mut solver = Solver {
            known: &known,
            prerelease: root.version.is_prerelease(),
            steps: 0,
            conflict: None,
        };
        let need = match solver.solve(root) {
            Ok(closure) => return Ok(closure),
            Err(Stop::Fail(why)) => return Err(ClosureError::Unresolvable(why)),
            Err(Stop::Need(need)) => need,
        };
        match need {
            Need::Versions(id) => {
                let versions = source.versions(&id).await?;
                known.versions.insert(id.to_ascii_lowercase(), versions);
            }
            Need::Dependencies(info) => {
                let deps = source.dependencies(&info).await?;
                let edges = deps
                    .into_iter()
                    .map(|d| Edge::new(&info, d))
                    .collect::<Result<Vec<_>, _>>()?;
                known.dependencies.insert(key(&info), edges);
            }
        }
    }
    Err(ClosureError::Unresolvable(format!(
        "the closure of {} {} needs more than {MAX_LOOKUPS} lookups",
        root.id,
        root.version.normalized()
    )))
}

/// What the resolution has learned from the source so far.
#[derive(Default)]
struct Known {
    /// Versions by lower-case id, oldest first.
    versions: HashMap<String, Vec<PackageInfo>>,
    /// Dependencies by lower-case id and version.
    dependencies: HashMap<(String, NuGetVersion), Vec<Edge>>,
}

fn key(info: &PackageInfo) -> (String, NuGetVersion) {
    (info.id.to_ascii_lowercase(), info.version.clone())
}

/// A dependency, parsed.
struct Edge {
    /// The id as the dependent spells it.
    id: String,
    /// Lower case, to compare.
    key: String,
    range: VersionRange,
}

impl Edge {
    fn new(from: &PackageInfo, dep: DependencyRef) -> Result<Edge, ClosureError> {
        let bad = |why: String| {
            ClosureError::Unresolvable(format!(
                "{} {} lists a dependency {why}",
                from.id,
                from.version.normalized()
            ))
        };
        // The id goes into upstream URLs: hold it to NuGet's rule.
        if !crate::cli::is_valid_id(&dep.id) {
            return Err(bad(format!("{:?} that is not a package id", dep.id)));
        }
        let range = VersionRange::parse(dep.range.as_deref().unwrap_or("")).map_err(|e| {
            bad(format!(
                "on {} with a range that does not parse: {e}",
                dep.id
            ))
        })?;
        Ok(Edge {
            key: dep.id.to_ascii_lowercase(),
            id: dep.id,
            range,
        })
    }

    fn describe(&self) -> String {
        match self.range.normalized().as_str() {
            "" => "any version".to_string(),
            r => r.to_string(),
        }
    }
}

fn pins(range: &VersionRange, version: &NuGetVersion) -> bool {
    matches!((&range.min, &range.max), (Some(lo), Some(hi))
        if lo.inclusive && hi.inclusive && &lo.version == version && &hi.version == version)
}

enum Need {
    Versions(String),
    Dependencies(Box<PackageInfo>),
}

enum Stop {
    /// The source has to be asked first; the solver then runs again.
    Need(Need),
    Fail(String),
}

struct Solver<'a> {
    known: &'a Known,
    prerelease: bool,
    steps: usize,
    /// The first clash met, to explain a failure.
    conflict: Option<String>,
}

impl<'a> Solver<'a> {
    fn solve(&mut self, root: &'a PackageInfo) -> Result<Closure, Stop> {
        let mut chosen = vec![root];
        if !self.search(&mut chosen)? {
            return Err(Stop::Fail(format!(
                "no set of versions fits the dependencies of {} {}: {}",
                root.id,
                root.version.normalized(),
                self.conflict.take().unwrap_or_default()
            )));
        }
        let mut packages: Vec<Resolved> = chosen
            .iter()
            .map(|info| Resolved {
                info: (*info).clone(),
                required_by: Vec::new(),
            })
            .collect();
        let mut provided = Vec::new();
        for parent in &chosen {
            let name = format!("{} {}", parent.id, parent.version.normalized());
            for edge in &self.known.dependencies[&key(parent)] {
                if edge.key == CLI_PACKAGE {
                    provided.push((name.clone(), edge.describe()));
                } else if let Some(p) = packages
                    .iter_mut()
                    .find(|p| p.info.id.eq_ignore_ascii_case(&edge.id))
                {
                    p.required_by.push(name.clone());
                }
            }
        }
        Ok(Closure { packages, provided })
    }

    /// Extend `chosen` until every dependency of every package in it is
    /// chosen too, or report that it cannot be.
    fn search(&mut self, chosen: &mut Vec<&'a PackageInfo>) -> Result<bool, Stop> {
        self.steps += 1;
        if self.steps > MAX_STEPS {
            return Err(Stop::Fail(format!(
                "the dependencies of {} {} need more than {MAX_STEPS} tries to resolve",
                chosen[0].id,
                chosen[0].version.normalized()
            )));
        }
        let mut edges: Vec<(&PackageInfo, &Edge)> = Vec::new();
        for parent in chosen.iter() {
            let Some(deps) = self.known.dependencies.get(&key(parent)) else {
                return Err(Stop::Need(Need::Dependencies(Box::new((*parent).clone()))));
            };
            edges.extend(
                deps.iter()
                    .filter(|e| e.key != CLI_PACKAGE)
                    .map(|e| (*parent, e)),
            );
        }
        let chosen_as = |key: &str| chosen.iter().find(|c| c.id.eq_ignore_ascii_case(key));

        // Whatever is chosen already has to fit every range on it.
        for (parent, edge) in &edges {
            if let Some(c) = chosen_as(&edge.key) {
                if !edge.range.contains(&c.version) {
                    self.clash(format!(
                        "{} {} needs {} {}, not {}",
                        parent.id,
                        parent.version.normalized(),
                        edge.id,
                        edge.describe(),
                        c.version.normalized()
                    ));
                    return Ok(false);
                }
            }
        }

        let Some((_, next)) = edges.iter().find(|(_, e)| chosen_as(&e.key).is_none()) else {
            return Ok(true);
        };
        if chosen.len() >= MAX_PACKAGES {
            return Err(Stop::Fail(format!(
                "the closure of {} {} holds more than {MAX_PACKAGES} packages",
                chosen[0].id,
                chosen[0].version.normalized()
            )));
        }
        let Some(versions) = self.known.versions.get(&next.key) else {
            return Err(Stop::Need(Need::Versions(next.id.clone())));
        };
        let on_it: Vec<&(&PackageInfo, &Edge)> =
            edges.iter().filter(|(_, e)| e.key == next.key).collect();
        let mut candidates: Vec<&'a PackageInfo> = versions
            .iter()
            .rev()
            .filter(|v| {
                let pinned = on_it.iter().any(|(_, e)| pins(&e.range, &v.version));
                on_it.iter().all(|(_, e)| e.range.contains(&v.version))
                    && (pinned || self.prerelease || !v.version.is_prerelease())
            })
            .collect();
        // NuGet's `ResolverComparer`: listed before unlisted, then highest
        // first. The sort is stable, so the order within each stays.
        candidates.sort_by_key(|v| !v.listed);
        if candidates.is_empty() {
            let wanted = on_it
                .iter()
                .map(|(p, e)| format!("{} {} needs {}", p.id, p.version.normalized(), e.describe()))
                .collect::<Vec<_>>()
                .join(", ");
            self.clash(if versions.is_empty() {
                format!("{} is not on the upstream ({wanted})", next.id)
            } else {
                format!("no version of {} fits ({wanted})", next.id)
            });
            return Ok(false);
        }
        for candidate in candidates {
            chosen.push(candidate);
            if self.search(chosen)? {
                return Ok(true);
            }
            chosen.pop();
        }
        Ok(false)
    }

    fn clash(&mut self, why: String) {
        self.conflict.get_or_insert(why);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::Url;
    use std::sync::Mutex;

    /// An upstream in memory: `(id, version, listed, dependencies)`.
    struct Fixture {
        packages: Vec<PackageInfo>,
        asked: Mutex<Vec<String>>,
    }

    fn info(id: &str, version: &str, listed: bool, deps: &[(&str, &str)]) -> PackageInfo {
        PackageInfo {
            id: id.to_string(),
            version: NuGetVersion::parse(version).unwrap(),
            listed,
            hash: None,
            size: None,
            dependencies: deps
                .iter()
                .map(|(id, range)| DependencyRef {
                    id: id.to_string(),
                    range: (!range.is_empty()).then(|| range.to_string()),
                })
                .collect(),
            download_url: Url::parse("https://example.com/x.nupkg").unwrap(),
        }
    }

    impl Fixture {
        fn new(packages: Vec<PackageInfo>) -> Self {
            Fixture {
                packages,
                asked: Mutex::new(Vec::new()),
            }
        }

        fn get(&self, id: &str, version: &str) -> PackageInfo {
            let v = NuGetVersion::parse(version).unwrap();
            self.packages
                .iter()
                .find(|p| p.id.eq_ignore_ascii_case(id) && p.version == v)
                .unwrap()
                .clone()
        }
    }

    #[async_trait]
    impl Source for Fixture {
        async fn versions(&self, id: &str) -> Result<Vec<PackageInfo>, UpstreamError> {
            self.asked.lock().unwrap().push(id.to_string());
            let mut v: Vec<_> = self
                .packages
                .iter()
                .filter(|p| p.id.eq_ignore_ascii_case(id))
                .cloned()
                .collect();
            v.sort_by(|a, b| a.version.cmp(&b.version));
            Ok(v)
        }

        async fn dependencies(
            &self,
            info: &PackageInfo,
        ) -> Result<Vec<DependencyRef>, UpstreamError> {
            Ok(info.dependencies.clone())
        }
    }

    fn names(closure: &Closure) -> Vec<String> {
        closure
            .packages
            .iter()
            .map(|p| format!("{} {}", p.info.id, p.info.version.normalized()))
            .collect()
    }

    async fn closure_of(
        fixture: &Fixture,
        id: &str,
        version: &str,
    ) -> Result<Closure, ClosureError> {
        resolve(fixture, &fixture.get(id, version)).await
    }

    #[tokio::test]
    async fn the_highest_fitting_version_of_each_dependency_is_taken() {
        let f = Fixture::new(vec![
            info("git", "2.51.0", true, &[("git.install", "[2.51.0]")]),
            info(
                "git.install",
                "2.50.0",
                true,
                &[("chocolatey-core.extension", "1.3.3")],
            ),
            info(
                "git.install",
                "2.51.0",
                true,
                &[
                    ("chocolatey-core.extension", "1.3.3"),
                    ("chocolatey", "0.10.8"),
                ],
            ),
            info("git.install", "2.52.0", true, &[]),
            info("chocolatey-core.extension", "1.3.3", true, &[]),
            info("chocolatey-core.extension", "1.4.0", true, &[]),
            info("chocolatey-core.extension", "1.5.0-beta", true, &[]),
            info("chocolatey-core.extension", "1.6.0", false, &[]),
        ]);
        let c = closure_of(&f, "git", "2.51.0").await.unwrap();
        // Highest, not NuGet's usual lowest: 1.4.0, not 1.3.3. Neither the
        // pre-release nor the unlisted 1.6.0, while a listed version fits.
        assert_eq!(
            names(&c),
            [
                "git 2.51.0",
                "git.install 2.51.0",
                "chocolatey-core.extension 1.4.0"
            ]
        );
        assert_eq!(c.packages[1].required_by, ["git 2.51.0"]);
        assert_eq!(c.packages[2].required_by, ["git.install 2.51.0"]);
        // The CLI is left to the image, and never looked up.
        assert_eq!(
            c.provided,
            [("git.install 2.51.0".to_string(), "[0.10.8, )".to_string())]
        );
        assert!(!f.asked.lock().unwrap().iter().any(|id| id == "chocolatey"));
    }

    #[tokio::test]
    async fn a_clash_backtracks_to_a_lower_version() {
        // app takes lib's newest (2.0), whose ext range clashes with the
        // root's; lib 1.0 fits both.
        let f = Fixture::new(vec![
            info(
                "app",
                "1.0.0",
                true,
                &[("lib", "1.0"), ("ext.extension", "[1.0,2.0)")],
            ),
            info("lib", "1.0.0", true, &[("ext.extension", "1.0")]),
            info("lib", "2.0.0", true, &[("ext.extension", "[2.0,)")]),
            info("ext.extension", "1.5.0", true, &[]),
            info("ext.extension", "2.0.0", true, &[]),
        ]);
        let c = closure_of(&f, "app", "1.0.0").await.unwrap();
        assert_eq!(names(&c), ["app 1.0.0", "lib 1.0.0", "ext.extension 1.5.0"]);
        assert_eq!(c.packages[2].required_by, ["app 1.0.0", "lib 1.0.0"]);
    }

    #[tokio::test]
    async fn an_unlisted_version_is_taken_when_no_listed_one_fits() {
        let f = Fixture::new(vec![
            info(
                "app",
                "1.0.0",
                true,
                &[("lib", "[1.0.0]"), ("tool", "(,2.0)")],
            ),
            info("lib", "1.0.0", false, &[]),
            info("lib", "2.0.0", true, &[]),
            info("tool", "1.0.0", true, &[]),
            info("tool", "1.5.0", false, &[]),
            info("pre", "1.0.0", true, &[("lib", "[2.0.0-rc]")]),
            info("lib", "2.0.0-rc", true, &[]),
        ]);
        let c = closure_of(&f, "app", "1.0.0").await.unwrap();
        assert_eq!(names(&c), ["app 1.0.0", "lib 1.0.0", "tool 1.0.0"]);
        // An exact pin takes a pre-release under a stable root too.
        let c = closure_of(&f, "pre", "1.0.0").await.unwrap();
        assert_eq!(names(&c), ["pre 1.0.0", "lib 2.0.0-rc"]);
    }

    #[tokio::test]
    async fn a_prerelease_root_lets_prerelease_dependencies_in() {
        let f = Fixture::new(vec![
            info("app", "2.0.0-beta", true, &[("lib", "1.0")]),
            info("app", "1.0.0", true, &[("lib", "1.0")]),
            info("lib", "1.0.0", true, &[]),
            info("lib", "1.1.0-rc", true, &[]),
        ]);
        let c = closure_of(&f, "app", "2.0.0-beta").await.unwrap();
        assert_eq!(names(&c), ["app 2.0.0-beta", "lib 1.1.0-rc"]);
        let c = closure_of(&f, "app", "1.0.0").await.unwrap();
        assert_eq!(names(&c), ["app 1.0.0", "lib 1.0.0"]);
    }

    #[tokio::test]
    async fn cycles_and_shared_dependencies_resolve_once() {
        let f = Fixture::new(vec![
            info("a", "1.0.0", true, &[("b", ""), ("c", "")]),
            info("b", "1.0.0", true, &[("C", "1.0"), ("A", "")]),
            info("c", "1.0.0", true, &[("b", "")]),
        ]);
        let c = closure_of(&f, "a", "1.0.0").await.unwrap();
        assert_eq!(names(&c), ["a 1.0.0", "b 1.0.0", "c 1.0.0"]);
        assert_eq!(c.packages[0].required_by, ["b 1.0.0"]);
        // One version list per id.
        let mut asked = f.asked.lock().unwrap().clone();
        asked.sort();
        assert_eq!(asked, ["b", "c"]);
    }

    #[tokio::test]
    async fn what_cannot_be_resolved_says_why() {
        let f = Fixture::new(vec![
            info("app", "1.0.0", true, &[("missing", "1.0")]),
            info("clash", "1.0.0", true, &[("lib", "[2.0,)"), ("other", "")]),
            info("other", "1.0.0", true, &[("lib", "(,2.0)")]),
            info("lib", "1.0.0", true, &[]),
            info("lib", "2.0.0", true, &[]),
            info("bad", "1.0.0", true, &[("lib", "[2.0,1.0]")]),
            info("evil", "1.0.0", true, &[("../x", "")]),
        ]);
        let err = closure_of(&f, "app", "1.0.0")
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("missing is not on the upstream (app 1.0.0 needs [1.0.0, ))"),
            "{err}"
        );
        let err = closure_of(&f, "clash", "1.0.0")
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("other 1.0.0 needs lib (, 2.0.0), not 2.0.0"),
            "{err}"
        );
        let err = closure_of(&f, "bad", "1.0.0")
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("does not parse"), "{err}");
        let err = closure_of(&f, "evil", "1.0.0")
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("not a package id"), "{err}");
    }
}
