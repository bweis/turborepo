//! The boundaries baseline: a machine-owned file recording known violations.
//!
//! Large repositories can rarely fix every boundaries violation before
//! enabling `turbo boundaries` in CI. A baseline records the violations that
//! exist today so that they are suppressed, while any *new* violation still
//! fails the check. The baseline is a ratchet:
//!
//! - every entry stores a `count`, so one more identical violation in the same
//!   file is reported as new;
//! - once a recorded violation is fixed, the entry becomes *stale* and is
//!   reported as an error until the baseline is regenerated with `turbo
//!   boundaries --update-baseline`. This keeps the baseline from silently
//!   permitting violations to be reintroduced.
//!
//! Entries are identified without line numbers or spans so that unrelated
//! edits to a file do not invalidate the baseline. An entry is keyed by the
//! package that owns the violation, the file (relative to the repository root,
//! with unix separators), the rule id (see
//! [`BoundariesDiagnostic::rule_id`]) and the rule's subject: the import
//! specifier for import rules, the related package (and tag) for tag rules,
//! and the cycle path for circular dependencies.
//!
//! The file is deterministic, pretty-printed JSON grouped by package:
//!
//! ```json
//! {
//!   "version": 1,
//!   "violations": {
//!     "//": [
//!       { "rule": "circular-dependency", "cycle": "a -> b -> a", "count": 1 }
//!     ],
//!     "web": [
//!       {
//!         "file": "apps/web/index.ts",
//!         "rule": "package-not-found",
//!         "import": "lodash",
//!         "count": 2
//!       },
//!       { "rule": "denied-tag", "package": "internal-ui", "tag": "internal", "count": 1 }
//!     ]
//!   }
//! }
//! ```
//!
//! Circular dependencies are repository-wide and are recorded under the root
//! package key (`//`). Diagnostics that describe broken configuration or
//! unreadable files (parse errors, invalid paths, invalid `boundaries`
//! configuration) are never written to the baseline, because suppressing them
//! would hide the fact that some code is not being checked at all.

use std::{
    collections::{BTreeMap, HashSet},
    fmt,
};

use serde::{Deserialize, Serialize};
use turbopath::{AbsoluteSystemPath, AbsoluteSystemPathBuf, RelativeUnixPathBuf};
use turborepo_repository::package_graph::ROOT_PKG_NAME;

use crate::{
    BoundariesConfig, BoundariesContext, BoundariesDiagnostic, BoundariesResult, Error,
    PackageGraphProvider, TurboJsonProvider,
};

/// Location of the baseline, relative to the repository root, when
/// `boundaries.baseline` is not set in the root `turbo.json`.
pub const DEFAULT_BASELINE_PATH: &str = "boundaries-baseline.json";

/// Version of the baseline file format written by this version of turbo.
pub const BASELINE_VERSION: u32 = 1;

/// Identity of a baselined violation within a package.
///
/// Field order defines the sort order of entries in the baseline file, which
/// groups a package's entries by file.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ViolationKey {
    /// File containing the violation, relative to the repository root with
    /// unix separators. `None` for violations that are not tied to a source
    /// file (tag rules and circular dependencies).
    pub file: Option<String>,
    /// Stable rule id, see [`BoundariesDiagnostic::rule_id`].
    pub rule: String,
    /// Import specifier, for import rules.
    pub import: Option<String>,
    /// The related package, for tag rules.
    pub package: Option<String>,
    /// The offending tag, for `denied-tag`.
    pub tag: Option<String>,
    /// The cycle path, for `circular-dependency`.
    pub cycle: Option<String>,
}

impl ViolationKey {
    fn new(rule: &str) -> Self {
        Self {
            file: None,
            rule: rule.to_string(),
            import: None,
            package: None,
            tag: None,
            cycle: None,
        }
    }

    /// Returns the owning package and key for a diagnostic, or `None` if the
    /// diagnostic can never be baselined.
    pub fn from_diagnostic(
        repo_root: &AbsoluteSystemPath,
        diagnostic: &BoundariesDiagnostic,
    ) -> Option<(String, Self)> {
        let rule = diagnostic.rule_id();
        let file_key = |path: &AbsoluteSystemPath, import: &str| Self {
            file: Some(relative_file(repo_root, path)),
            import: Some(import.to_string()),
            ..Self::new(rule)
        };
        match diagnostic {
            BoundariesDiagnostic::ImportLeavesPackage {
                path,
                import,
                package_name,
                ..
            }
            | BoundariesDiagnostic::NotTypeOnlyImport {
                path,
                import,
                package_name,
                ..
            }
            | BoundariesDiagnostic::PackageNotFound {
                path,
                name: import,
                package_name,
                ..
            } => Some((package_name.to_string(), file_key(path, import))),
            BoundariesDiagnostic::NoTagInAllowlist {
                source_package_name,
                package_name,
                ..
            } => Some((
                source_package_name.to_string(),
                Self {
                    package: Some(package_name.to_string()),
                    ..Self::new(rule)
                },
            )),
            BoundariesDiagnostic::DeniedTag {
                source_package_name,
                package_name,
                tag,
                ..
            } => Some((
                source_package_name.to_string(),
                Self {
                    package: Some(package_name.to_string()),
                    tag: Some(tag.clone()),
                    ..Self::new(rule)
                },
            )),
            BoundariesDiagnostic::DeniedPackage {
                source_package_name,
                declared_by,
                dependency,
                ..
            } => Some((
                source_package_name.to_string(),
                Self {
                    package: Some(declared_by.to_string()),
                    import: Some(dependency.clone()),
                    ..Self::new(rule)
                },
            )),
            BoundariesDiagnostic::CircularDependency { cycle_path } => Some((
                ROOT_PKG_NAME.to_string(),
                Self {
                    cycle: Some(cycle_path.clone()),
                    ..Self::new(rule)
                },
            )),
            // Configuration and I/O problems mean code isn't being checked;
            // they must be fixed rather than baselined.
            BoundariesDiagnostic::PackageBoundariesHasTags { .. }
            | BoundariesDiagnostic::TagSharesPackageName { .. }
            | BoundariesDiagnostic::InvalidPath { .. }
            | BoundariesDiagnostic::ParseError(..)
            | BoundariesDiagnostic::StaleBaselineEntry { .. }
            | BoundariesDiagnostic::PackageBoundariesHasImportChecks { .. }
            | BoundariesDiagnostic::PackageBoundariesHasPackageTags { .. }
            | BoundariesDiagnostic::InvalidPackageTagsGlob { .. }
            | BoundariesDiagnostic::InvalidDenyPackagesPattern { .. }
            | BoundariesDiagnostic::DenyPackagesInDependents { .. } => None,
        }
    }
}

impl fmt::Display for ViolationKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "`{}`", self.rule)?;
        if let Some(file) = &self.file {
            write!(f, " in `{file}`")?;
        }
        if let Some(import) = &self.import {
            write!(f, " for import `{import}`")?;
        }
        if let Some(package) = &self.package {
            write!(f, " for package `{package}`")?;
        }
        if let Some(tag) = &self.tag {
            write!(f, " with tag `{tag}`")?;
        }
        if let Some(cycle) = &self.cycle {
            write!(f, " for cycle `{cycle}`")?;
        }
        Ok(())
    }
}

fn relative_file(repo_root: &AbsoluteSystemPath, path: &AbsoluteSystemPath) -> String {
    match repo_root.anchor(path) {
        Ok(anchored) => anchored.to_unix().to_string(),
        // Files outside of the repository root can't be checked, but fall back
        // to a stable representation rather than dropping the violation.
        Err(_) => path.to_string().replace('\\', "/"),
    }
}

/// The set of packages whose baseline entries are evaluated by a check.
///
/// When `turbo boundaries` runs with `--filter`, packages outside of the
/// filter are not checked, so their entries can neither be matched nor be
/// stale, and `--update-baseline` must leave them untouched.
#[derive(Debug, Clone, Default)]
pub struct BaselineScope {
    /// Packages that were checked. `None` means every package.
    checked: Option<HashSet<String>>,
    /// Every package that exists in the repository.
    known: HashSet<String>,
}

impl BaselineScope {
    /// A scope covering every package.
    pub fn all() -> Self {
        Self::default()
    }

    /// A scope covering `checked` packages. Entries for packages that are not
    /// in `known` (e.g. deleted or renamed packages) are always in scope so
    /// that they are reported as stale and removed by `--update-baseline`.
    pub fn new(
        checked: impl IntoIterator<Item = String>,
        known: impl IntoIterator<Item = String>,
    ) -> Self {
        Self {
            checked: Some(checked.into_iter().collect()),
            known: known.into_iter().collect(),
        }
    }

    /// The scope of a boundaries check run with `ctx`.
    pub fn from_context<G, T>(ctx: &BoundariesContext<'_, G, T>) -> Self
    where
        G: PackageGraphProvider,
        T: TurboJsonProvider,
    {
        Self::new(
            ctx.filtered_pkgs.iter().map(|name| name.to_string()),
            ctx.pkg_dep_graph
                .package_scopes()
                .map(|scope| scope.name.to_string()),
        )
    }

    /// Whether entries for `package` are evaluated.
    pub fn contains(&self, package: &str) -> bool {
        // Circular dependencies are checked across the whole graph regardless
        // of filters.
        package == ROOT_PKG_NAME
            || self
                .checked
                .as_ref()
                .is_none_or(|checked| checked.contains(package))
            || !self.known.contains(package)
    }
}

/// A set of known violations, with an occurrence count for each.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Baseline {
    violations: BTreeMap<String, BTreeMap<ViolationKey, usize>>,
}

/// On-disk representation. Kept separate from [`Baseline`] so the in-memory
/// form can be keyed for lookups while the file stays readable.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BaselineFile {
    version: u32,
    violations: BTreeMap<String, Vec<BaselineFileEntry>>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BaselineFileEntry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    file: Option<String>,
    rule: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    import: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    package: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tag: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cycle: Option<String>,
    count: usize,
}

impl Baseline {
    /// Resolves the baseline location from the root `boundaries` config.
    ///
    /// Returns the absolute path and the path as written relative to the
    /// repository root, for use in messages.
    pub fn path(
        repo_root: &AbsoluteSystemPath,
        root_config: Option<&BoundariesConfig>,
    ) -> Result<(AbsoluteSystemPathBuf, String), Error> {
        let configured = root_config
            .and_then(|config| config.baseline.as_ref())
            .map(|baseline| baseline.as_inner().as_str())
            .unwrap_or(DEFAULT_BASELINE_PATH);
        let relative =
            RelativeUnixPathBuf::new(configured).map_err(|_| Error::InvalidBaselinePath {
                path: configured.to_string(),
            })?;
        Ok((repo_root.join_unix_path(&relative), configured.to_string()))
    }

    /// Reads the baseline at `path`, returning `None` if it does not exist.
    pub fn load(path: &AbsoluteSystemPath) -> Result<Option<Self>, Error> {
        let Some(contents) = path
            .read_existing_to_string()
            .map_err(|_| Error::FileNotFound(path.to_owned()))?
        else {
            return Ok(None);
        };
        Self::from_json(&contents)
            .map(Some)
            .map_err(|reason| Error::InvalidBaseline {
                path: path.to_string(),
                reason,
            })
    }

    /// Parses a baseline. Returns a human readable reason on failure.
    pub fn from_json(contents: &str) -> Result<Self, String> {
        let file: BaselineFile = serde_json::from_str(contents).map_err(|err| err.to_string())?;
        if file.version != BASELINE_VERSION {
            return Err(format!(
                "unsupported version {}, this version of turbo supports version {}",
                file.version, BASELINE_VERSION
            ));
        }

        let mut baseline = Self::default();
        for (package, entries) in file.violations {
            for entry in entries {
                if entry.count == 0 {
                    return Err(format!(
                        "entry for `{}` in package `{package}` has a count of 0",
                        entry.rule
                    ));
                }
                let key = ViolationKey {
                    file: entry.file,
                    rule: entry.rule,
                    import: entry.import,
                    package: entry.package,
                    tag: entry.tag,
                    cycle: entry.cycle,
                };
                baseline.add(package.clone(), key, entry.count);
            }
        }
        Ok(baseline)
    }

    /// Serializes the baseline as deterministic, pretty-printed JSON with a
    /// trailing newline.
    pub fn to_json(&self) -> Result<String, Error> {
        let file = BaselineFile {
            version: BASELINE_VERSION,
            violations: self
                .violations
                .iter()
                .map(|(package, entries)| {
                    let entries = entries
                        .iter()
                        .map(|(key, count)| BaselineFileEntry {
                            file: key.file.clone(),
                            rule: key.rule.clone(),
                            import: key.import.clone(),
                            package: key.package.clone(),
                            tag: key.tag.clone(),
                            cycle: key.cycle.clone(),
                            count: *count,
                        })
                        .collect();
                    (package.clone(), entries)
                })
                .collect(),
        };
        let mut json = serde_json::to_string_pretty(&file).map_err(Error::SerializeBaseline)?;
        json.push('\n');
        Ok(json)
    }

    /// Writes the baseline to `path`, creating parent directories as needed.
    pub fn write(&self, path: &AbsoluteSystemPath) -> Result<(), Error> {
        if let Some(parent) = path.parent() {
            parent
                .create_dir_all()
                .map_err(|_| Error::FileWrite(path.to_owned()))?;
        }
        path.create_with_contents(self.to_json()?)
            .map_err(|_| Error::FileWrite(path.to_owned()))
    }

    /// Builds a baseline from every baselinable diagnostic.
    pub fn from_diagnostics<'a>(
        repo_root: &AbsoluteSystemPath,
        diagnostics: impl IntoIterator<Item = &'a BoundariesDiagnostic>,
    ) -> Self {
        let mut baseline = Self::default();
        for diagnostic in diagnostics {
            if let Some((package, key)) = ViolationKey::from_diagnostic(repo_root, diagnostic) {
                baseline.add(package, key, 1);
            }
        }
        baseline
    }

    fn add(&mut self, package: String, key: ViolationKey, count: usize) {
        *self
            .violations
            .entry(package)
            .or_default()
            .entry(key)
            .or_default() += count;
    }

    /// Total number of violations recorded.
    pub fn len(&self) -> usize {
        self.violations.values().flat_map(|e| e.values()).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.violations.is_empty()
    }

    /// Returns the baseline that `--update-baseline` should write: entries for
    /// packages outside of `scope` are preserved, entries for packages in
    /// scope are replaced with `current`.
    pub fn update(&self, current: Baseline, scope: &BaselineScope) -> Baseline {
        let mut violations: BTreeMap<_, _> = self
            .violations
            .iter()
            .filter(|(package, _)| !scope.contains(package))
            .map(|(package, entries)| (package.clone(), entries.clone()))
            .collect();
        for (package, entries) in current.violations {
            let existing = violations.entry(package).or_default();
            for (key, count) in entries {
                *existing.entry(key).or_default() += count;
            }
        }
        Baseline { violations }
    }

    /// Suppresses violations in `result` that are covered by the baseline and
    /// appends a [`BoundariesDiagnostic::StaleBaselineEntry`] for every
    /// in-scope entry that occurs fewer times than recorded.
    ///
    /// If a violation occurs more times than recorded, the excess occurrences
    /// are kept as diagnostics.
    pub fn apply(
        &self,
        repo_root: &AbsoluteSystemPath,
        baseline_path: &str,
        scope: &BaselineScope,
        result: &mut BoundariesResult,
    ) {
        let mut remaining = self.violations.clone();
        let mut found = Baseline::default();
        let mut suppressed = 0;

        result.diagnostics.retain(|diagnostic| {
            let Some((package, key)) = ViolationKey::from_diagnostic(repo_root, diagnostic) else {
                return true;
            };
            let budget = remaining
                .get_mut(&package)
                .and_then(|entries| entries.get_mut(&key));
            let keep = match budget {
                Some(count) if *count > 0 => {
                    *count -= 1;
                    suppressed += 1;
                    false
                }
                _ => true,
            };
            found.add(package, key, 1);
            keep
        });

        for (package, entries) in &self.violations {
            if !scope.contains(package) {
                continue;
            }
            for (key, &baselined_count) in entries {
                let found_count = found
                    .violations
                    .get(package)
                    .and_then(|entries| entries.get(key))
                    .copied()
                    .unwrap_or(0);
                if found_count < baselined_count {
                    result
                        .diagnostics
                        .push(BoundariesDiagnostic::StaleBaselineEntry {
                            baseline_path: baseline_path.to_string(),
                            package: package.clone(),
                            entry: key.to_string(),
                            baselined_count,
                            found_count,
                        });
                }
            }
        }

        result.suppressed_by_baseline += suppressed;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use miette::{NamedSource, SourceSpan};
    use turborepo_repository::package_graph::PackageName;

    use super::*;

    fn repo_root() -> AbsoluteSystemPathBuf {
        if cfg!(windows) {
            AbsoluteSystemPathBuf::new("C:\\repo").unwrap()
        } else {
            AbsoluteSystemPathBuf::new("/repo").unwrap()
        }
    }

    fn file(repo_root: &AbsoluteSystemPath, path: &str) -> AbsoluteSystemPathBuf {
        repo_root.join_unix_path(RelativeUnixPathBuf::new(path).unwrap())
    }

    fn text() -> NamedSource<Arc<str>> {
        NamedSource::new("file", Arc::from(""))
    }

    fn package_not_found(
        repo_root: &AbsoluteSystemPath,
        package: &str,
        path: &str,
        name: &str,
        offset: usize,
    ) -> BoundariesDiagnostic {
        BoundariesDiagnostic::PackageNotFound {
            path: file(repo_root, path),
            package_name: PackageName::from(package),
            name: name.to_string(),
            help: None,
            span: SourceSpan::from((offset, 1)),
            text: text(),
        }
    }

    fn import_leaves(
        repo_root: &AbsoluteSystemPath,
        package: &str,
        path: &str,
        import: &str,
    ) -> BoundariesDiagnostic {
        BoundariesDiagnostic::ImportLeavesPackage {
            path: file(repo_root, path),
            import: import.to_string(),
            resolved_import_path: "../elsewhere".to_string(),
            package_name: PackageName::from(package),
            span: SourceSpan::from((0, 1)),
            text: text(),
        }
    }

    fn denied_tag(source: &str, package: &str, tag: &str) -> BoundariesDiagnostic {
        BoundariesDiagnostic::DeniedTag {
            source_package_name: PackageName::from(source),
            package_name: PackageName::from(package),
            tag: tag.to_string(),
            span: None,
            text: text(),
            secondary: [crate::SecondaryDiagnostic::Denylist {
                span: None,
                text: text(),
            }],
        }
    }

    fn cycle(path: &str) -> BoundariesDiagnostic {
        BoundariesDiagnostic::CircularDependency {
            cycle_path: path.to_string(),
        }
    }

    fn result_with(diagnostics: Vec<BoundariesDiagnostic>) -> BoundariesResult {
        BoundariesResult {
            diagnostics,
            ..Default::default()
        }
    }

    fn stale(result: &BoundariesResult) -> Vec<(&str, usize, usize)> {
        result
            .diagnostics
            .iter()
            .filter_map(|d| match d {
                BoundariesDiagnostic::StaleBaselineEntry {
                    package,
                    baselined_count,
                    found_count,
                    ..
                } => Some((package.as_str(), *baselined_count, *found_count)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn rule_ids_are_stable() {
        let root = repo_root();
        assert_eq!(
            package_not_found(&root, "web", "apps/web/a.ts", "x", 0).rule_id(),
            "package-not-found"
        );
        assert_eq!(
            import_leaves(&root, "web", "apps/web/a.ts", "../x").rule_id(),
            "import-leaves-package"
        );
        assert_eq!(denied_tag("web", "ui", "internal").rule_id(), "denied-tag");
        assert_eq!(cycle("a -> b -> a").rule_id(), "circular-dependency");
    }

    #[test]
    fn keys_do_not_depend_on_spans() {
        let root = repo_root();
        let a = package_not_found(&root, "web", "apps/web/a.ts", "lodash", 10);
        let b = package_not_found(&root, "web", "apps/web/a.ts", "lodash", 500);
        assert_eq!(
            ViolationKey::from_diagnostic(&root, &a),
            ViolationKey::from_diagnostic(&root, &b)
        );
        let (package, key) = ViolationKey::from_diagnostic(&root, &a).unwrap();
        assert_eq!(package, "web");
        assert_eq!(key.file.as_deref(), Some("apps/web/a.ts"));
        assert_eq!(key.import.as_deref(), Some("lodash"));
    }

    #[test]
    fn config_and_io_diagnostics_are_not_baselinable() {
        let root = repo_root();
        let diagnostics = [
            BoundariesDiagnostic::ParseError(file(&root, "apps/web/a.ts"), "oops".into()),
            BoundariesDiagnostic::InvalidPath { path: "bad".into() },
            BoundariesDiagnostic::PackageBoundariesHasTags {
                span: None,
                text: text(),
            },
        ];
        assert!(Baseline::from_diagnostics(&root, &diagnostics).is_empty());

        let baseline = Baseline::from_diagnostics(&root, &[cycle("a -> b -> a")]);
        let mut result = result_with(diagnostics.to_vec());
        baseline.apply(
            &root,
            DEFAULT_BASELINE_PATH,
            &BaselineScope::all(),
            &mut result,
        );
        // All three remain, plus the stale cycle entry.
        assert_eq!(result.diagnostics.len(), 4);
    }

    #[test]
    fn suppresses_covered_violations() {
        let root = repo_root();
        let diagnostics = vec![
            package_not_found(&root, "web", "apps/web/a.ts", "lodash", 0),
            import_leaves(&root, "web", "apps/web/b.ts", "../ui/index.ts"),
            denied_tag("web", "ui", "internal"),
            cycle("a -> b -> a"),
        ];
        let baseline = Baseline::from_diagnostics(&root, &diagnostics);
        assert_eq!(baseline.len(), 4);

        let mut result = result_with(diagnostics);
        baseline.apply(
            &root,
            DEFAULT_BASELINE_PATH,
            &BaselineScope::all(),
            &mut result,
        );
        assert!(result.is_ok(), "{:?}", result.diagnostics);
        assert_eq!(result.suppressed_by_baseline, 4);
    }

    #[test]
    fn counts_ratchet() {
        let root = repo_root();
        let one = vec![package_not_found(
            &root,
            "web",
            "apps/web/a.ts",
            "lodash",
            0,
        )];
        let baseline = Baseline::from_diagnostics(&root, &one);

        // One more identical violation in the same file is reported.
        let mut result = result_with(vec![
            package_not_found(&root, "web", "apps/web/a.ts", "lodash", 0),
            package_not_found(&root, "web", "apps/web/a.ts", "lodash", 40),
        ]);
        baseline.apply(
            &root,
            DEFAULT_BASELINE_PATH,
            &BaselineScope::all(),
            &mut result,
        );
        assert_eq!(result.suppressed_by_baseline, 1);
        assert_eq!(result.diagnostics.len(), 1);
        assert_eq!(result.diagnostics[0].rule_id(), "package-not-found");

        // The same violation in a different file is reported.
        let mut result = result_with(vec![package_not_found(
            &root,
            "web",
            "apps/web/other.ts",
            "lodash",
            0,
        )]);
        baseline.apply(
            &root,
            DEFAULT_BASELINE_PATH,
            &BaselineScope::all(),
            &mut result,
        );
        assert_eq!(result.suppressed_by_baseline, 0);
        assert_eq!(
            result
                .diagnostics
                .iter()
                .map(|d| d.rule_id())
                .collect::<Vec<_>>(),
            ["package-not-found", "stale-baseline-entry"]
        );
    }

    #[test]
    fn reports_partially_and_fully_fixed_entries_as_stale() {
        let root = repo_root();
        let baseline = Baseline::from_diagnostics(
            &root,
            &[
                package_not_found(&root, "web", "apps/web/a.ts", "lodash", 0),
                package_not_found(&root, "web", "apps/web/a.ts", "lodash", 1),
                package_not_found(&root, "web", "apps/web/a.ts", "lodash", 2),
                denied_tag("web", "ui", "internal"),
            ],
        );

        let mut result = result_with(vec![package_not_found(
            &root,
            "web",
            "apps/web/a.ts",
            "lodash",
            0,
        )]);
        baseline.apply(
            &root,
            DEFAULT_BASELINE_PATH,
            &BaselineScope::all(),
            &mut result,
        );
        assert_eq!(result.suppressed_by_baseline, 1);
        let mut stale = stale(&result);
        stale.sort();
        assert_eq!(stale, [("web", 1, 0), ("web", 3, 1)]);
        assert!(!result.is_ok());
        let message = result
            .diagnostics
            .iter()
            .map(|d| d.to_string())
            .collect::<Vec<_>>();
        assert!(
            message.iter().any(|m| m.contains(
                "`package-not-found` in `apps/web/a.ts` for import `lodash` (baselined: 3, found: \
                 1)"
            )),
            "{message:?}"
        );
    }

    #[test]
    fn filter_scopes_stale_detection() {
        let root = repo_root();
        let baseline = Baseline::from_diagnostics(
            &root,
            &[
                package_not_found(&root, "web", "apps/web/a.ts", "lodash", 0),
                package_not_found(&root, "docs", "apps/docs/a.ts", "lodash", 0),
                package_not_found(&root, "deleted", "apps/deleted/a.ts", "lodash", 0),
                cycle("a -> b -> a"),
            ],
        );
        // Only `web` is checked; `docs` exists but was filtered out; `deleted`
        // no longer exists.
        let scope =
            BaselineScope::new(["web".to_string()], ["web".to_string(), "docs".to_string()]);
        assert!(scope.contains("web"));
        assert!(!scope.contains("docs"));
        assert!(scope.contains("deleted"));
        assert!(scope.contains(ROOT_PKG_NAME));

        let mut result = result_with(vec![]);
        baseline.apply(&root, DEFAULT_BASELINE_PATH, &scope, &mut result);
        let mut stale = stale(&result);
        stale.sort();
        assert_eq!(stale, [("//", 1, 0), ("deleted", 1, 0), ("web", 1, 0)]);
    }

    #[test]
    fn update_only_replaces_in_scope_packages() {
        let root = repo_root();
        let existing = Baseline::from_diagnostics(
            &root,
            &[
                package_not_found(&root, "web", "apps/web/a.ts", "lodash", 0),
                package_not_found(&root, "docs", "apps/docs/a.ts", "lodash", 0),
                package_not_found(&root, "deleted", "apps/deleted/a.ts", "lodash", 0),
            ],
        );
        let current = Baseline::from_diagnostics(
            &root,
            &[import_leaves(&root, "web", "apps/web/b.ts", "../ui")],
        );
        let scope =
            BaselineScope::new(["web".to_string()], ["web".to_string(), "docs".to_string()]);

        let updated = existing.update(current, &scope);
        let expected = Baseline::from_diagnostics(
            &root,
            &[
                import_leaves(&root, "web", "apps/web/b.ts", "../ui"),
                package_not_found(&root, "docs", "apps/docs/a.ts", "lodash", 0),
            ],
        );
        assert_eq!(updated, expected);

        // Without a filter, the update is a full replacement.
        let updated = existing.update(Baseline::default(), &BaselineScope::all());
        assert!(updated.is_empty());
    }

    #[test]
    fn serialization_is_deterministic_and_round_trips() {
        let root = repo_root();
        let diagnostics = vec![
            package_not_found(&root, "web", "apps/web/z.ts", "lodash", 0),
            cycle("a -> b -> a"),
            denied_tag("web", "ui", "internal"),
            package_not_found(&root, "web", "apps/web/a.ts", "lodash", 0),
            package_not_found(&root, "web", "apps/web/a.ts", "lodash", 9),
            import_leaves(&root, "docs", "apps/docs/a.ts", "../web/index.ts"),
        ];
        let forward = Baseline::from_diagnostics(&root, &diagnostics);
        let backward = Baseline::from_diagnostics(&root, diagnostics.iter().rev());
        assert_eq!(forward.to_json().unwrap(), backward.to_json().unwrap());

        let json = forward.to_json().unwrap();
        assert_eq!(
            json,
            r#"{
  "version": 1,
  "violations": {
    "//": [
      {
        "rule": "circular-dependency",
        "cycle": "a -> b -> a",
        "count": 1
      }
    ],
    "docs": [
      {
        "file": "apps/docs/a.ts",
        "rule": "import-leaves-package",
        "import": "../web/index.ts",
        "count": 1
      }
    ],
    "web": [
      {
        "rule": "denied-tag",
        "package": "ui",
        "tag": "internal",
        "count": 1
      },
      {
        "file": "apps/web/a.ts",
        "rule": "package-not-found",
        "import": "lodash",
        "count": 2
      },
      {
        "file": "apps/web/z.ts",
        "rule": "package-not-found",
        "import": "lodash",
        "count": 1
      }
    ]
  }
}
"#
        );
        assert_eq!(Baseline::from_json(&json).unwrap(), forward);
    }

    #[test]
    fn rejects_invalid_files() {
        assert!(
            Baseline::from_json(r#"{"version": 2, "violations": {}}"#)
                .unwrap_err()
                .contains("unsupported version 2")
        );
        assert!(
            Baseline::from_json(
                r#"{"version": 1, "violations": {"web": [{"rule": "x", "count": 0}]}}"#
            )
            .unwrap_err()
            .contains("count of 0")
        );
        assert!(
            Baseline::from_json(
                r#"{"version": 1, "violations": {"web": [{"rule": "x", "line": 3, "count": 1}]}}"#
            )
            .is_err()
        );
        assert!(Baseline::from_json("not json").is_err());
    }

    #[test]
    fn duplicate_entries_are_summed() {
        let baseline = Baseline::from_json(
            r#"{"version": 1, "violations": {"web": [
                {"file": "a.ts", "rule": "package-not-found", "import": "x", "count": 1},
                {"file": "a.ts", "rule": "package-not-found", "import": "x", "count": 2}
            ]}}"#,
        )
        .unwrap();
        assert_eq!(baseline.len(), 3);
    }

    #[test]
    fn resolves_path_from_config() {
        let root = repo_root();
        let (path, display) = Baseline::path(&root, None).unwrap();
        assert_eq!(display, DEFAULT_BASELINE_PATH);
        assert_eq!(path, file(&root, DEFAULT_BASELINE_PATH));

        let config = BoundariesConfig {
            baseline: Some(turborepo_errors::Spanned::new(
                "config/boundaries.json".to_string(),
            )),
            ..Default::default()
        };
        let (path, display) = Baseline::path(&root, Some(&config)).unwrap();
        assert_eq!(display, "config/boundaries.json");
        assert_eq!(path, file(&root, "config/boundaries.json"));

        let config = BoundariesConfig {
            baseline: Some(turborepo_errors::Spanned::new("/abs.json".to_string())),
            ..Default::default()
        };
        assert!(matches!(
            Baseline::path(&root, Some(&config)),
            Err(Error::InvalidBaselinePath { .. })
        ));
    }

    #[test]
    fn load_and_write_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let root = AbsoluteSystemPath::new(tmp.path().to_str().unwrap()).unwrap();
        let path = file(root, "nested/dir/baseline.json");
        assert_eq!(Baseline::load(&path).unwrap(), None);

        let baseline = Baseline::from_diagnostics(root, &[cycle("a -> b -> a")]);
        baseline.write(&path).unwrap();
        assert_eq!(Baseline::load(&path).unwrap(), Some(baseline));

        path.create_with_contents("{").unwrap();
        assert!(matches!(
            Baseline::load(&path),
            Err(Error::InvalidBaseline { .. })
        ));
    }
}
