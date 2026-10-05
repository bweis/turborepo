// miette's derive macro causes false positives for these lints
#![allow(unused_assignments)]

mod baseline;
mod config;
mod imports;
mod package_tags;
mod tags;

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs::OpenOptions,
    io::Write,
    sync::Arc,
};

pub use baseline::{
    BASELINE_VERSION, Baseline, BaselineScope, DEFAULT_BASELINE_PATH, ViolationKey,
};
pub use config::{BoundariesConfig, PackageTagsMap, Permissions, Rule, RulesMap};
use globwalk::{Settings, ValidatedGlob};
use indicatif::ProgressBar;
use miette::{Diagnostic, NamedSource, Report, SourceSpan};
use oxc_ast::ast::Comment;
use oxc_span::Span;
use oxc_syntax::module_record::ModuleRecord;
use rayon::prelude::*;
pub use tags::{DeniedPackages, ProcessedPermissions, ProcessedRule, ProcessedRulesMap};
use thiserror::Error;
use tracing::{debug_span, info_span};
use turbo_trace::{ImportResult, ImportTraceType, ImportType, Tracer, find_imports};
use turbopath::{AbsoluteSystemPath, AbsoluteSystemPathBuf};
use turborepo_errors::Spanned;
use turborepo_log::Subsystem;
use turborepo_repository::{
    external_resolution::PackageExternalDeclarations,
    package_graph::{PackageCycle, PackageGraph, PackageGraphNodeKind, PackageName, PackageNode},
};
use turborepo_ui::{BOLD_GREEN, BOLD_RED, ColorConfig, color};
use unrs_resolver::Resolver;

use crate::{
    imports::{DependencyLocations, WorkspacePackageDirectories},
    package_tags::PackageTagIndex,
};

#[derive(Clone)]
pub struct PackageScope<'a> {
    pub name: PackageName,
    pub name_source: Option<&'a Spanned<()>>,
    pub directory: &'a turbopath::AnchoredSystemPath,
    pub definition_path: &'a turbopath::AnchoredSystemPath,
    pub kind: PackageGraphNodeKind,
}

impl PackageScope<'_> {
    fn is_boundary_checkable(&self) -> bool {
        // Boundary analysis consumes package.json import semantics. The
        // authoritative definition identifies that format independently of
        // contributor provenance.
        self.kind == PackageGraphNodeKind::Package
            && self.definition_path.as_path().file_name() == Some("package.json".as_ref())
    }
}

pub trait PackageGraphProvider: Send + Sync {
    /// Authoritative scopes. Implementations must not derive these facts from
    /// compatibility manifests.
    fn package_scopes(&self) -> Box<dyn Iterator<Item = PackageScope<'_>> + '_>;
    fn external_declarations<'a>(
        &'a self,
        name: &'a PackageName,
    ) -> PackageExternalDeclarations<'a> {
        PackageExternalDeclarations::new(&[], name.as_str())
    }
    fn immediate_dependencies(&self, node: &PackageNode) -> Option<HashSet<&PackageNode>>;
    fn dependencies(&self, node: &PackageNode) -> Box<dyn Iterator<Item = &PackageNode> + '_>;
    fn ancestors(&self, node: &PackageNode) -> Box<dyn Iterator<Item = &PackageNode> + '_>;
    /// Returns strongly connected components with more than one member,
    /// representing circular dependency chains in the package graph, each
    /// with a representative cycle path and its full, sorted membership.
    fn find_cycles(&self) -> Vec<PackageCycle>;
}

impl PackageGraphProvider for PackageGraph {
    fn package_scopes(&self) -> Box<dyn Iterator<Item = PackageScope<'_>> + '_> {
        Box::new(self.node_views().filter_map(|(node, view)| match node {
            PackageNode::Workspace(name) => Some(PackageScope {
                name,
                name_source: view.name_source(),
                directory: view.directory()?,
                definition_path: view.definition_path()?,
                kind: view.kind(),
            }),
            PackageNode::Root => None,
        }))
    }

    fn external_declarations<'a>(
        &'a self,
        name: &'a PackageName,
    ) -> PackageExternalDeclarations<'a> {
        PackageGraph::external_declarations(self, name)
    }

    fn immediate_dependencies(&self, node: &PackageNode) -> Option<HashSet<&PackageNode>> {
        self.immediate_dependencies(node)
    }

    fn dependencies(&self, node: &PackageNode) -> Box<dyn Iterator<Item = &PackageNode> + '_> {
        Box::new(self.dependencies(node).into_iter())
    }

    fn ancestors(&self, node: &PackageNode) -> Box<dyn Iterator<Item = &PackageNode> + '_> {
        Box::new(self.ancestors(node).into_iter())
    }

    fn find_cycles(&self) -> Vec<PackageCycle> {
        self.find_cycle_components()
    }
}

pub trait TurboJsonProvider: Send + Sync {
    /// Returns true if turbo.json exists and can be loaded for this package
    fn has_turbo_json(&self, pkg: &PackageName) -> bool;
    fn boundaries_config(&self, pkg: &PackageName) -> Option<&BoundariesConfig>;
    fn package_tags(&self, pkg: &PackageName) -> Option<&Spanned<Vec<Spanned<String>>>>;
    fn implicit_dependencies(&self, pkg: &PackageName) -> HashMap<String, Spanned<()>>;
}

pub struct BoundariesContext<'a, G: PackageGraphProvider, T: TurboJsonProvider> {
    pub repo_root: &'a AbsoluteSystemPath,
    pub pkg_dep_graph: &'a G,
    pub turbo_json_provider: &'a T,
    pub root_boundaries_config: Option<&'a BoundariesConfig>,
    pub filtered_pkgs: &'a HashSet<PackageName>,
}

/// Converts an owned `String`-backed source (produced by
/// `Spanned::span_and_text` for configuration files) into the shared
/// `Arc<str>`-backed representation used by all diagnostics, so retaining N
/// diagnostics never retains N copies of the text.
pub(crate) fn into_shared_source(source: NamedSource<String>) -> NamedSource<Arc<str>> {
    let name = source.name().to_string();
    let text: Arc<str> = source.inner().as_str().into();
    NamedSource::new(name, text)
}

#[derive(Clone, Debug, Error, Diagnostic)]
pub enum SecondaryDiagnostic {
    #[error("package `{package} is defined here")]
    PackageDefinedHere {
        package: String,
        #[label]
        package_span: Option<SourceSpan>,
        #[source_code]
        package_text: NamedSource<Arc<str>>,
    },
    #[error("consider adding one of the following tags listed here")]
    Allowlist {
        #[label]
        span: Option<SourceSpan>,
        #[source_code]
        text: NamedSource<Arc<str>>,
    },
    #[error("denylist defined here")]
    Denylist {
        #[label]
        span: Option<SourceSpan>,
        #[source_code]
        text: NamedSource<Arc<str>>,
    },
    #[error("denied by `{pattern}` in `denyPackages`")]
    DeniedPackagePattern {
        pattern: String,
        #[label]
        span: Option<SourceSpan>,
        #[source_code]
        text: NamedSource<Arc<str>>,
    },
}

#[derive(Clone, Debug, Error, Diagnostic)]
pub enum BoundariesDiagnostic {
    #[error("Package boundaries rules cannot have `tags` key")]
    PackageBoundariesHasTags {
        #[label("tags defined here")]
        span: Option<SourceSpan>,
        #[source_code]
        text: NamedSource<Arc<str>>,
    },
    #[error("Package boundaries rules cannot have `importChecks` key")]
    #[diagnostic(help("`importChecks` can only be set in the root `turbo.json`"))]
    PackageBoundariesHasImportChecks {
        #[label("importChecks defined here")]
        span: Option<SourceSpan>,
        #[source_code]
        text: NamedSource<Arc<str>>,
    },
    #[error("Package boundaries rules cannot have `packageTags` key")]
    #[diagnostic(help("`packageTags` can only be used in the root `turbo.json`"))]
    PackageBoundariesHasPackageTags {
        #[label("packageTags defined here")]
        span: Option<SourceSpan>,
        #[source_code]
        text: NamedSource<Arc<str>>,
    },
    #[error("Invalid glob `{glob}` in `boundaries.packageTags`: {reason}")]
    InvalidPackageTagsGlob {
        glob: String,
        reason: String,
        #[label("tags assigned to this glob")]
        span: Option<SourceSpan>,
        #[source_code]
        text: NamedSource<Arc<str>>,
    },
    #[error("Tag `{tag}` cannot share the same name as package `{package}`")]
    TagSharesPackageName {
        tag: String,
        package: String,
        #[label("tag defined here")]
        tag_span: Option<SourceSpan>,
        #[source_code]
        tag_text: NamedSource<Arc<str>>,
        #[related]
        secondary: [SecondaryDiagnostic; 1],
    },
    #[error("Path `{path}` is not valid UTF-8. Turborepo only supports UTF-8 paths.")]
    InvalidPath {
        path: String,
        /// The file containing the import that resolved to `path`
        file: AbsoluteSystemPathBuf,
    },
    #[error(
        "Package `{package_name}` found without any tag listed in allowlist for \
         `{source_package_name}`"
    )]
    NoTagInAllowlist {
        // The package that is declaring the allowlist
        source_package_name: PackageName,
        // The package that is either a dependency or dependent of the source package
        package_name: PackageName,
        #[label("tag not found here")]
        span: Option<SourceSpan>,
        #[help]
        help: Option<String>,
        #[source_code]
        text: NamedSource<Arc<str>>,
        #[related]
        secondary: [SecondaryDiagnostic; 1],
    },
    #[error(
        "Package `{package_name}` found with tag listed in denylist for `{source_package_name}`: \
         `{tag}`"
    )]
    DeniedTag {
        source_package_name: PackageName,
        package_name: PackageName,
        tag: String,
        #[label("tag found here")]
        span: Option<SourceSpan>,
        #[source_code]
        text: NamedSource<Arc<str>>,
        #[related]
        secondary: [SecondaryDiagnostic; 1],
    },
    #[error(
        "Package `{source_package_name}` depends on denied package `{dependency}` (declared in \
         `{dependency_kind}` of `{declared_by}`)"
    )]
    DeniedPackage {
        // The package whose rules deny the dependency
        source_package_name: PackageName,
        // The workspace package whose package.json declares the dependency. Either
        // the source package itself or one of its transitive workspace dependencies.
        declared_by: PackageName,
        // The dependency as declared in package.json (the alias for npm aliases)
        dependency: String,
        // The package.json field the dependency is declared in
        dependency_kind: &'static str,
        // The `denyPackages` entry that matched
        pattern: String,
        #[label("package defined here")]
        span: Option<SourceSpan>,
        #[source_code]
        text: NamedSource<Arc<str>>,
        #[help]
        help: Option<String>,
        #[related]
        secondary: [SecondaryDiagnostic; 1],
    },
    #[error("Invalid `denyPackages` pattern `{pattern}`: {reason}")]
    InvalidDenyPackagesPattern {
        pattern: String,
        reason: String,
        #[label("pattern defined here")]
        span: Option<SourceSpan>,
        #[source_code]
        text: NamedSource<Arc<str>>,
    },
    #[error("`denyPackages` can only be used in `dependencies` rules")]
    #[diagnostic(help(
        "`denyPackages` restricts the npm packages that a package and its workspace dependencies \
         depend on. To restrict which packages can depend on this one, use `allow` or `deny` with \
         tags or package names."
    ))]
    DenyPackagesInDependents {
        #[label("`denyPackages` defined here")]
        span: Option<SourceSpan>,
        #[source_code]
        text: NamedSource<Arc<str>>,
    },
    #[error(
        "importing from a type declaration package, but import is not declared as a type-only \
         import"
    )]
    #[help("add `type` to the import declaration")]
    NotTypeOnlyImport {
        path: AbsoluteSystemPathBuf,
        // The package containing the importing file
        package_name: PackageName,
        import: String,
        #[label("package imported here")]
        span: SourceSpan,
        #[source_code]
        text: NamedSource<Arc<str>>,
    },
    #[error("cannot import package `{name}` because it is not a dependency")]
    PackageNotFound {
        path: AbsoluteSystemPathBuf,
        // The package containing the importing file
        package_name: PackageName,
        name: String,
        #[help]
        help: Option<String>,
        #[label("package imported here")]
        span: SourceSpan,
        #[source_code]
        text: NamedSource<Arc<str>>,
    },
    #[error("import `{import}` leaves the package")]
    #[diagnostic(help(
        "`{import}` resolves to path `{resolved_import_path}` which is outside of `{package_name}`"
    ))]
    ImportLeavesPackage {
        path: AbsoluteSystemPathBuf,
        import: String,
        resolved_import_path: String,
        package_name: PackageName,
        #[label("file imported here")]
        span: SourceSpan,
        #[source_code]
        text: NamedSource<Arc<str>>,
    },
    #[error("failed to parse file {0}: {1}")]
    ParseError(AbsoluteSystemPathBuf, String),
    #[error("Circular package dependency detected: {cycle_path}")]
    CircularDependency {
        cycle_path: String,
        /// Every package in the cycle's strongly connected component, sorted.
        /// `cycle_path` is a single representative loop and may not visit all
        /// of them.
        members: Vec<String>,
    },
    #[error(
        "Stale entry in boundaries baseline `{baseline_path}` for package `{package}`: {entry} \
         (baselined: {baselined_count}, found: {found_count})"
    )]
    #[diagnostic(help(
        "a baselined violation was fixed. Run `turbo boundaries --update-baseline` to remove it \
         from the baseline so that it cannot be reintroduced"
    ))]
    StaleBaselineEntry {
        baseline_path: String,
        package: String,
        entry: String,
        baselined_count: usize,
        found_count: usize,
    },
}

#[derive(Debug, Error, Diagnostic)]
pub enum Error {
    #[error("file `{0}` does not have a parent directory")]
    NoParentDir(AbsoluteSystemPathBuf),
    #[error(transparent)]
    Path(#[from] turbopath::PathError),
    #[error(transparent)]
    Lockfiles(#[from] turborepo_lockfiles::Error),
    #[error(transparent)]
    Glob(#[from] globwalk::GlobError),
    #[error(transparent)]
    GlobWalk(#[from] globwalk::WalkError),
    #[error("failed to apply gitignore rules: {0}")]
    GitIgnore(#[from] ignore::Error),
    #[error("failed to read file: {0}")]
    FileNotFound(AbsoluteSystemPathBuf),
    #[error("failed to write to file: {0}")]
    FileWrite(AbsoluteSystemPathBuf),
    #[error(transparent)]
    #[diagnostic(transparent)]
    InvalidIgnoreGlob(Box<InvalidIgnoreGlob>),
    #[error("invalid boundaries baseline `{path}`: {reason}")]
    #[diagnostic(help(
        "fix the file, or delete it and regenerate it with `turbo boundaries --update-baseline`"
    ))]
    InvalidBaseline { path: String, reason: String },
    #[error("invalid `boundaries.baseline` path `{path}`: {reason}")]
    #[diagnostic(help(
        "use a path to a file inside the repository, relative to the repository root"
    ))]
    InvalidBaselinePath { path: String, reason: String },
    #[error("failed to serialize boundaries baseline: {0}")]
    SerializeBaseline(#[source] serde_json::Error),
}

#[derive(Debug, Error, Diagnostic)]
#[error("Invalid glob `{glob}` in `boundaries.ignore`: {reason}")]
pub struct InvalidIgnoreGlob {
    glob: String,
    reason: String,
    #[label("glob defined here")]
    span: Option<SourceSpan>,
    #[source_code]
    text: NamedSource<Arc<str>>,
}

impl BoundariesDiagnostic {
    /// A stable, kebab-case identifier for the kind of violation this
    /// diagnostic reports. Unlike the human readable message, rule ids are
    /// part of turbo's public interface (they are written to the boundaries
    /// baseline file) and must not change.
    pub fn rule_id(&self) -> &'static str {
        match self {
            Self::PackageBoundariesHasTags { .. } => "package-boundaries-has-tags",
            Self::TagSharesPackageName { .. } => "tag-shares-package-name",
            Self::InvalidPath { .. } => "invalid-path",
            Self::NoTagInAllowlist { .. } => "tag-not-in-allowlist",
            Self::DeniedTag { .. } => "denied-tag",
            Self::NotTypeOnlyImport { .. } => "not-type-only-import",
            Self::PackageNotFound { .. } => "package-not-found",
            Self::ImportLeavesPackage { .. } => "import-leaves-package",
            Self::ParseError(..) => "parse-error",
            Self::CircularDependency { .. } => "circular-dependency",
            Self::StaleBaselineEntry { .. } => "stale-baseline-entry",
            Self::PackageBoundariesHasImportChecks { .. } => "package-boundaries-has-import-checks",
            Self::PackageBoundariesHasPackageTags { .. } => "package-boundaries-has-package-tags",
            Self::InvalidPackageTagsGlob { .. } => "invalid-package-tags-glob",
            Self::DeniedPackage { .. } => "denied-package",
            Self::InvalidDenyPackagesPattern { .. } => "invalid-deny-packages-pattern",
            Self::DenyPackagesInDependents { .. } => "deny-packages-in-dependents",
        }
    }

    /// Returns the file that this diagnostic prevented from being fully
    /// checked, if any. Violations in such a file may be missing from the
    /// results.
    pub fn unchecked_file(&self) -> Option<&AbsoluteSystemPath> {
        match self {
            Self::ParseError(path, _) => Some(path),
            Self::InvalidPath { file, .. } => Some(file),
            _ => None,
        }
    }

    pub fn path_and_span(&self) -> Option<(&AbsoluteSystemPath, SourceSpan)> {
        match self {
            Self::ImportLeavesPackage { path, span, .. } => Some((path, *span)),
            Self::PackageNotFound { path, span, .. } => Some((path, *span)),
            Self::NotTypeOnlyImport { path, span, .. } => Some((path, *span)),
            Self::CircularDependency { .. } => None,
            _ => None,
        }
    }
}

fn is_valid_package_name_segment(segment: &str) -> bool {
    let Some((first, rest)) = segment.as_bytes().split_first() else {
        return false;
    };

    matches!(*first, b'a'..=b'z' | b'0'..=b'9' | b'-' | b'~')
        && rest
            .iter()
            .all(|byte| matches!(byte, b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~'))
}

fn is_valid_package_name(package_name: &str) -> bool {
    if let Some(scoped_name) = package_name.strip_prefix('@') {
        let Some((scope, name)) = scoped_name.split_once('/') else {
            return false;
        };

        is_valid_package_name_segment(scope) && is_valid_package_name_segment(name)
    } else {
        is_valid_package_name_segment(package_name)
    }
}

/// Maximum number of warnings to show
const MAX_WARNINGS: usize = 16;

// Report completed work in batches so worker threads do not synchronize with
// the progress bar after every package. This retains frequent visible feedback
// while making progress reporting proportional to batches rather than package
// count.
const PROGRESS_UPDATE_BATCH_SIZE: usize = 16;

#[derive(Default)]
pub struct BoundariesResult {
    pub files_checked: usize,
    pub packages_checked: usize,
    /// Set when import checks were disabled with `importChecks: false`, in
    /// which case no files were checked.
    pub import_checks_skipped: bool,
    pub warnings: Vec<String>,
    pub diagnostics: Vec<BoundariesDiagnostic>,
    /// Number of violations that were found but suppressed because they are
    /// recorded in the boundaries baseline.
    pub suppressed_by_baseline: usize,
}

impl BoundariesResult {
    pub fn is_ok(&self) -> bool {
        self.diagnostics.is_empty()
    }

    fn merge(&mut self, other: BoundariesResult) {
        self.files_checked += other.files_checked;
        self.packages_checked += other.packages_checked;
        self.import_checks_skipped |= other.import_checks_skipped;
        self.warnings.extend(other.warnings);
        self.diagnostics.extend(other.diagnostics);
        self.suppressed_by_baseline += other.suppressed_by_baseline;
    }

    pub fn emit(&self, color_config: ColorConfig) {
        for diagnostic in &self.diagnostics {
            eprintln!("{:?}", Report::new(diagnostic.clone()));
        }
        let result_message = match self.diagnostics.len() {
            0 => color!(color_config, BOLD_GREEN, "no issues found"),
            1 => color!(color_config, BOLD_RED, "1 issue found"),
            _ => color!(
                color_config,
                BOLD_RED,
                "{} issues found",
                self.diagnostics.len()
            ),
        };

        for warning in self.warnings.iter().take(MAX_WARNINGS) {
            turborepo_log::warn(
                turborepo_log::Source::turbo(Subsystem::Boundaries),
                warning.to_string(),
            )
            .emit();
        }
        if !self.warnings.is_empty() {
            eprintln!();
        }

        let suppressed_message = match self.suppressed_by_baseline {
            0 => String::new(),
            n => format!(" ({n} suppressed by baseline)"),
        };

        if self.import_checks_skipped {
            println!(
                "Checked {} packages (import checks disabled), {}{}",
                self.packages_checked, result_message, suppressed_message
            );
        } else {
            println!(
                "Checked {} files in {} packages, {}{}",
                self.files_checked, self.packages_checked, result_message, suppressed_message
            );
        }
    }
}

fn find_dynamic_imports(module_record: &ModuleRecord, source: &str) -> Vec<ImportResult> {
    module_record
        .dynamic_imports
        .iter()
        .filter_map(|dynamic_import| {
            let request_span = dynamic_import.module_request;
            let request = source.get(request_span.start as usize..request_span.end as usize)?;
            let allocator = oxc_allocator::Allocator::default();
            let parsed =
                oxc_parser::Parser::new(&allocator, request, oxc_span::SourceType::default())
                    .parse();
            let literal = &parsed.program.directives.first()?.expression;

            Some(ImportResult {
                specifier: literal.value.to_string(),
                span: Span::new(
                    request_span.start + literal.span.start,
                    request_span.start + literal.span.end,
                ),
                statement_span: dynamic_import.span,
                import_type: ImportType::Value,
            })
        })
        .collect()
}

/// Parse a file with oxc, returning both imports and comments.
///
/// We parse directly here (rather than using `turbo_trace::parse_file`) because
/// we need access to the comment list for `@boundaries-ignore` detection.
fn parse_with_comments(
    file_path: &AbsoluteSystemPath,
    source: &str,
) -> Option<(Vec<turbo_trace::ImportResult>, Vec<Comment>)> {
    let _span = debug_span!("parse_file", path = %file_path).entered();
    let allocator = oxc_allocator::Allocator::default();
    let source_type = oxc_span::SourceType::from_path(file_path.as_std_path()).unwrap_or_default();
    let ret = oxc_parser::Parser::new(&allocator, source, source_type).parse();
    if ret.panicked {
        return None;
    }
    let mut imports = find_imports(&ret.module_record, &ret.program.body, ImportTraceType::All);
    imports.extend(find_dynamic_imports(&ret.module_record, source));
    let comments: Vec<Comment> = ret.program.comments.iter().copied().collect();
    Some((imports, comments))
}

pub struct BoundariesChecker;

impl BoundariesChecker {
    /// Returns the underlying reason if an import has been marked as ignored.
    ///
    /// Searches for the nearest comment that ends before the import span and
    /// checks if it contains `@boundaries-ignore`.
    pub(crate) fn get_ignored_comment(
        comments: &[Comment],
        source_text: &str,
        import_span: oxc_span::Span,
    ) -> Option<String> {
        // Walk backwards through comments that end before the import. We check
        // multiple because there may be stacked comments before an import:
        //   // @boundaries-ignore reason
        //   // @ts-ignore
        //   import { foo } from "bar";
        //
        // Comments are collected in source order, so a binary search finds
        // where the comments preceding the import end without evaluating the
        // span predicate for every comment in the file on every import.
        let leading = comments.partition_point(|c| c.span.end <= import_span.start);

        // To detect blank lines we check the gap between each comment and the
        // *next* item in the chain (initially the import, then the previous
        // comment we visited). A blank line means more than one newline in
        // that gap, so counting stops as soon as two are found.
        let mut next_start = import_span.start;

        for comment in comments[..leading].iter().rev() {
            let between = &source_text[comment.span.end as usize..next_start as usize];
            if between
                .char_indices()
                .filter(|&(_, c)| c == '\n')
                .nth(1)
                .is_some()
            {
                break;
            }

            let content_span = comment.content_span();
            let text = &source_text[content_span.start as usize..content_span.end as usize];
            if let Some(reason) = text.trim().strip_prefix("@boundaries-ignore") {
                return Some(reason.to_string());
            }

            next_start = comment.span.start;
        }
        None
    }

    /// Returns `true` if the import specifier looks like it could be an npm
    /// package name (e.g. `react`, `@scope/pkg`, `lodash/fp`).
    ///
    /// Used in [`imports::check_import`] to decide whether a non-relative
    /// import that didn't resolve as a tsconfig alias should be checked
    /// against declared dependencies.
    fn is_potential_package_name(import: &str) -> bool {
        let base = imports::get_package_name(import);
        is_valid_package_name(base)
    }

    /// Patch a file with boundaries-ignore comments
    pub fn patch_file(
        file_path: &AbsoluteSystemPath,
        file_patches: Vec<(SourceSpan, String)>,
    ) -> Result<(), Error> {
        // Deduplicate and sort by offset
        let file_patches = file_patches
            .into_iter()
            .map(|(span, patch)| (span.offset(), patch))
            .collect::<BTreeMap<usize, String>>();

        let contents = file_path
            .read_to_string()
            .map_err(|_| Error::FileNotFound(file_path.to_owned()))?;

        let mut options = OpenOptions::new();
        options.read(true).write(true).truncate(true);
        let mut file = file_path
            .open_with_options(options)
            .map_err(|_| Error::FileNotFound(file_path.to_owned()))?;

        let mut last_idx = 0;
        for (idx, reason) in file_patches {
            let contents_before_span = &contents[last_idx..idx];

            // Find the last newline before the span (note this is the index into the slice,
            // not the full file)
            let newline_idx = contents_before_span.rfind('\n');

            // If newline exists, we write all the contents before newline
            if let Some(newline_idx) = newline_idx {
                file.write_all(&contents.as_bytes()[last_idx..(last_idx + newline_idx)])
                    .map_err(|_| Error::FileWrite(file_path.to_owned()))?;
                file.write_all(b"\n")
                    .map_err(|_| Error::FileWrite(file_path.to_owned()))?;
            }

            file.write_all(b"// @boundaries-ignore ")
                .map_err(|_| Error::FileWrite(file_path.to_owned()))?;
            file.write_all(reason.as_bytes())
                .map_err(|_| Error::FileWrite(file_path.to_owned()))?;
            file.write_all(b"\n")
                .map_err(|_| Error::FileWrite(file_path.to_owned()))?;

            last_idx = idx;
        }

        file.write_all(&contents.as_bytes()[last_idx..])
            .map_err(|_| Error::FileWrite(file_path.to_owned()))?;

        Ok(())
    }

    /// Check boundaries for all filtered package.json scopes.
    pub fn check_boundaries<G, T>(
        ctx: &BoundariesContext<'_, G, T>,
        show_progress: bool,
    ) -> Result<BoundariesResult, Error>
    where
        G: PackageGraphProvider,
        T: TurboJsonProvider,
    {
        let _span = info_span!("check_boundaries").entered();
        let import_checks = ctx
            .root_boundaries_config
            .is_none_or(BoundariesConfig::import_checks_enabled);
        let mut result = BoundariesResult {
            import_checks_skipped: !import_checks,
            ..Default::default()
        };
        let rules_map =
            Self::get_processed_rules_map(ctx.root_boundaries_config, &mut result.diagnostics);
        let packages: Vec<_> = ctx.pkg_dep_graph.package_scopes().collect();

        {
            let _span = info_span!("find_cycles").entered();
            for cycle in ctx.pkg_dep_graph.find_cycles() {
                let Some(first) = cycle.path.first() else {
                    continue;
                };
                let cycle_path = cycle
                    .path
                    .iter()
                    .chain(std::iter::once(first))
                    .map(|name| name.to_string())
                    .collect::<Vec<_>>()
                    .join(" -> ");
                let members = cycle.members.iter().map(|name| name.to_string()).collect();
                result
                    .diagnostics
                    .push(BoundariesDiagnostic::CircularDependency {
                        cycle_path,
                        members,
                    });
            }
        }

        let global_implicit_dependencies = ctx
            .turbo_json_provider
            .implicit_dependencies(&PackageName::Root);
        let global_ignore_globs = Self::ignore_globs(ctx.root_boundaries_config)?;

        // Every checkable package, regardless of the filter, can be the target of
        // an import, so the directory lookup covers the whole workspace.
        let workspace_packages = WorkspacePackageDirectories::new(
            packages
                .iter()
                .filter(|scope| {
                    matches!(scope.name, PackageName::Other(_)) && scope.is_boundary_checkable()
                })
                .map(|scope| (ctx.repo_root.resolve(scope.directory), scope.name.clone())),
        );

        let packages_to_check: Vec<_> = packages
            .iter()
            .filter(|scope| {
                matches!(scope.name, PackageName::Other(_))
                    && ctx.filtered_pkgs.contains(&scope.name)
                    && scope.is_boundary_checkable()
            })
            .map(|scope| (scope.name.clone(), scope.name_source, scope.directory))
            .collect();

        let package_tags_config = ctx
            .root_boundaries_config
            .and_then(|boundaries| boundaries.package_tags.as_ref());
        // Tags are only read by tag rules and package-level rules. Without
        // either (and without `packageTags` to validate), skip loading every
        // package's turbo.json.
        let needs_package_tags = package_tags_config.is_some()
            || rules_map.as_ref().is_some_and(|rules| !rules.is_empty())
            || packages_to_check.iter().any(|(name, ..)| {
                ctx.turbo_json_provider
                    .boundaries_config(name)
                    .is_some_and(|boundaries| {
                        boundaries.dependencies.is_some() || boundaries.dependents.is_some()
                    })
            });
        let package_tags = if needs_package_tags {
            let _span = info_span!("resolve_package_tags").entered();
            let resolved = PackageTagIndex::resolve(
                ctx.turbo_json_provider,
                package_tags_config,
                packages.iter().map(|scope| (&scope.name, scope.directory)),
            );
            result.diagnostics.extend(resolved.diagnostics);
            result.warnings.extend(resolved.warnings);
            resolved.index
        } else {
            PackageTagIndex::default()
        };

        let progress = if show_progress {
            println!("Checking packages...");
            ProgressBar::new(packages_to_check.len() as u64)
        } else {
            ProgressBar::hidden()
        };

        let package_results: Vec<Result<BoundariesResult, Error>> = {
            let _span = info_span!("check_all_packages", count = packages_to_check.len()).entered();
            turborepo_rayon_compat::block_in_place(|| {
                packages_to_check
                    .par_chunks(PROGRESS_UPDATE_BATCH_SIZE)
                    .map(|packages| {
                        let results = packages
                            .iter()
                            .map(|(package_name, package_name_source, package_directory)| {
                                Self::check_package(
                                    ctx,
                                    package_name,
                                    *package_name_source,
                                    package_directory,
                                    &rules_map,
                                    &package_tags,
                                    &global_implicit_dependencies,
                                    &workspace_packages,
                                    import_checks,
                                    &global_ignore_globs,
                                )
                            })
                            .collect::<Vec<_>>();
                        progress.inc(packages.len() as u64);
                        results
                    })
                    .collect::<Vec<_>>()
                    .into_iter()
                    .flatten()
                    .collect()
            })
        };

        for pkg_result in package_results {
            result.merge(pkg_result?);
        }

        Ok(result)
    }

    fn get_processed_rules_map(
        root_boundaries_config: Option<&BoundariesConfig>,
        diagnostics: &mut Vec<BoundariesDiagnostic>,
    ) -> Option<ProcessedRulesMap> {
        root_boundaries_config
            .and_then(|boundaries| boundaries.tags.as_ref())
            .map(|tags| {
                tags.as_inner()
                    .iter()
                    .map(|(k, v)| (k.clone(), ProcessedRule::new(v.clone(), diagnostics)))
                    .collect()
            })
    }

    /// Parses the `boundaries.ignore` globs of a config, pointing at the
    /// offending entry in `turbo.json` if one is invalid.
    fn ignore_globs(config: Option<&BoundariesConfig>) -> Result<Vec<ValidatedGlob>, Error> {
        let Some(ignore) = config.and_then(|config| config.ignore.as_ref()) else {
            return Ok(Vec::new());
        };

        ignore
            .as_inner()
            .iter()
            .map(|glob| {
                let invalid = |reason: String| {
                    let (span, text) = glob.span_and_text("turbo.json");
                    Error::InvalidIgnoreGlob(Box::new(InvalidIgnoreGlob {
                        glob: glob.as_inner().clone(),
                        reason,
                        span,
                        text: into_shared_source(text),
                    }))
                };

                // Exclusions don't support negation, so a leading `!` would
                // silently match nothing.
                if glob.starts_with('!') {
                    return Err(invalid("negated globs are not supported".to_string()));
                }
                let validated: ValidatedGlob = glob
                    .parse()
                    .map_err(|e: globwalk::GlobError| invalid(e.reason().to_string()))?;
                // Globs are relative to the package directory. Once cleaned,
                // anything that resolves to the package root or above it would
                // exclude every file in the package.
                match validated.as_str() {
                    "" | "." => {
                        return Err(invalid(
                            "glob matches the entire package directory".to_string(),
                        ));
                    }
                    path if path == ".." || path.starts_with("../") => {
                        return Err(invalid(
                            "glob must not point outside the package directory".to_string(),
                        ));
                    }
                    _ => {}
                }
                // Compile the glob up front so syntax errors are reported against
                // the config entry instead of surfacing from the file walk.
                wax::Glob::new(&globwalk::fix_glob_pattern(validated.as_str()))
                    .map_err(|e| invalid(e.to_string()))?;
                Ok(validated)
            })
            .collect()
    }

    #[expect(clippy::too_many_arguments)]
    fn check_package<G, T>(
        ctx: &BoundariesContext<'_, G, T>,
        package_name: &PackageName,
        package_name_source: Option<&Spanned<()>>,
        package_directory: &turbopath::AnchoredSystemPath,
        tag_rules: &Option<ProcessedRulesMap>,
        package_tags: &PackageTagIndex,
        global_implicit_dependencies: &HashMap<String, Spanned<()>>,
        workspace_packages: &WorkspacePackageDirectories,
        import_checks: bool,
        global_ignore_globs: &[ValidatedGlob],
    ) -> Result<BoundariesResult, Error>
    where
        G: PackageGraphProvider,
        T: TurboJsonProvider,
    {
        let _span = info_span!("check_package", package = %package_name).entered();
        let mut result = BoundariesResult::default();

        if import_checks {
            let implicit_dependencies = ctx.turbo_json_provider.implicit_dependencies(package_name);
            let mut ignore_globs = global_ignore_globs.to_vec();
            ignore_globs.extend(Self::ignore_globs(
                ctx.turbo_json_provider.boundaries_config(package_name),
            )?);
            let file_result = Self::check_package_files(
                ctx,
                package_name,
                package_directory,
                &implicit_dependencies,
                global_implicit_dependencies,
                workspace_packages,
                &ignore_globs,
            )?;
            result.merge(file_result);
        }

        // Packages without a turbo.json can still be subject to tag rules if
        // the root turbo.json assigns them tags through `packageTags`.
        let current_package_tags = package_tags.get(package_name);
        if current_package_tags.is_some() || ctx.turbo_json_provider.has_turbo_json(package_name) {
            let _span = info_span!("check_package_tags", package = %package_name).entered();
            result.diagnostics.extend(tags::check_package_tags(
                ctx,
                package_tags,
                PackageNode::Workspace(package_name.clone()),
                package_name_source,
                current_package_tags,
                tag_rules.as_ref(),
            )?);
        }

        result.packages_checked = 1;

        Ok(result)
    }

    fn check_package_files<G, T>(
        ctx: &BoundariesContext<'_, G, T>,
        package_name: &PackageName,
        package_directory: &turbopath::AnchoredSystemPath,
        implicit_dependencies: &HashMap<String, Spanned<()>>,
        global_implicit_dependencies: &HashMap<String, Spanned<()>>,
        workspace_packages: &WorkspacePackageDirectories,
        ignore_globs: &[ValidatedGlob],
    ) -> Result<BoundariesResult, Error>
    where
        G: PackageGraphProvider,
        T: TurboJsonProvider,
    {
        let _span = info_span!("check_package_files", package = %package_name).entered();
        let package_root = ctx.repo_root.resolve(package_directory);
        let internal_dependencies = ctx
            .pkg_dep_graph
            .immediate_dependencies(&PackageNode::Workspace(package_name.to_owned()))
            .unwrap_or_default();

        let mut files = {
            let _span = info_span!("globwalk", package = %package_name).entered();
            let include_patterns: [ValidatedGlob; 8] = [
                "**/*.js".parse()?,
                "**/*.jsx".parse()?,
                "**/*.ts".parse()?,
                "**/*.tsx".parse()?,
                "**/*.cjs".parse()?,
                "**/*.mjs".parse()?,
                "**/*.svelte".parse()?,
                "**/*.vue".parse()?,
            ];
            // Files matched by `boundaries.ignore` are excluded from the walk, so
            // they are never parsed and don't count towards `files_checked`.
            let mut exclude_patterns: Vec<ValidatedGlob> =
                vec!["node_modules/**".parse()?, "**/node_modules/**".parse()?];
            exclude_patterns.extend_from_slice(ignore_globs);

            globwalk::globwalk_with_settings(
                &package_root,
                &include_patterns,
                &exclude_patterns,
                globwalk::WalkType::Files,
                Settings::default().ignore_nested_packages(),
            )?
        };

        if !files.is_empty() {
            let unignored_files = ignore::WalkBuilder::new(package_root.as_std_path())
                .hidden(false)
                .ignore(false)
                .git_ignore(true)
                .git_exclude(true)
                .git_global(true)
                .parents(true)
                .require_git(false)
                .follow_links(false)
                .build()
                .filter_map(|entry| match entry {
                    Ok(entry) if entry.file_type().is_some_and(|kind| kind.is_file()) => {
                        Some(Ok(entry.into_path()))
                    }
                    Ok(_) => None,
                    Err(error) => Some(Err(error)),
                })
                .collect::<Result<HashSet<_>, ignore::Error>>()?;
            files.retain(|file| unignored_files.contains(file.as_std_path()));
        }

        // We assume the tsconfig.json is at the root of the package
        let tsconfig_path = package_root.join_component("tsconfig.json");
        let resolver =
            Tracer::create_resolver(tsconfig_path.exists().then(|| tsconfig_path.as_ref()));

        let mut not_supported_extensions = HashSet::new();

        let (js_ts_files, other_files): (Vec<_>, Vec<_>) = files
            .iter()
            .partition(|f| !matches!(f.extension(), Some("svelte" | "vue")));

        for file_path in &other_files {
            if let Some(ext @ ("svelte" | "vue")) = file_path.extension() {
                not_supported_extensions.insert(ext.to_string());
            }
        }

        let dependency_locations = DependencyLocations {
            package: package_name,
            internal_dependencies: &internal_dependencies,
            external_declarations: ctx.pkg_dep_graph.external_declarations(package_name),
            implicit_dependencies,
            global_implicit_dependencies,
            workspace_packages,
        };

        type FileResult = Result<(Vec<BoundariesDiagnostic>, Vec<String>), Error>;
        let file_results: Vec<FileResult> = {
            let _span = info_span!(
                "process_files",
                package = %package_name,
                count = js_ts_files.len()
            )
            .entered();
            js_ts_files
                .par_iter()
                .map(|file_path| {
                    Self::process_file(
                        package_name,
                        &package_root,
                        file_path,
                        dependency_locations,
                        &resolver,
                    )
                })
                .collect()
        };

        let mut result = BoundariesResult::default();
        for file_result in file_results {
            let (diagnostics, warnings) = file_result?;
            result.diagnostics.extend(diagnostics);
            result.warnings.extend(warnings);
        }

        for ext in &not_supported_extensions {
            result.warnings.push(format!(
                "{ext} files are currently not supported, boundaries checks will not apply to them"
            ));
        }

        result.files_checked = files.len();

        Ok(result)
    }

    fn process_file(
        package_name: &PackageName,
        package_root: &AbsoluteSystemPath,
        file_path: &AbsoluteSystemPath,
        dependency_locations: DependencyLocations<'_>,
        resolver: &Resolver,
    ) -> Result<(Vec<BoundariesDiagnostic>, Vec<String>), Error> {
        // Read the file once and share it across every diagnostic it
        // produces. Each emitted error keeps an Arc clone instead of a fresh
        // copy of the whole source, so retained memory scales with the file
        // size rather than file size times error count.
        let file_content: Arc<str> = file_path
            .read_to_string()
            .map_err(|_| Error::FileNotFound(file_path.to_owned()))?
            .into();

        let (imports, comments) = match parse_with_comments(file_path, &file_content) {
            Some(result) => result,
            None => {
                return Ok((
                    vec![BoundariesDiagnostic::ParseError(
                        file_path.to_owned(),
                        "parser panicked".to_string(),
                    )],
                    Vec::new(),
                ));
            }
        };

        let mut diagnostics = Vec::new();
        let mut warnings = Vec::new();

        for import_result in &imports {
            imports::check_import(
                &comments,
                &file_content,
                &mut diagnostics,
                &mut warnings,
                package_name,
                package_root,
                &import_result.specifier,
                &import_result.import_type,
                &import_result.span,
                &import_result.statement_span,
                file_path,
                &file_content,
                dependency_locations,
                resolver,
            )?;
        }

        Ok((diagnostics, warnings))
    }
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    #[test]
    fn finds_string_literal_dynamic_imports() {
        let source = r#"
            import("../package-b/index.ts");
            import('@repo/package-b');
            import(packageName);
        "#;
        let tmp = tempfile::tempdir().unwrap();
        let path = AbsoluteSystemPath::new(tmp.path().to_str().unwrap())
            .unwrap()
            .join_component("index.ts");

        let (imports, _) = parse_with_comments(&path, source).unwrap();
        let specifiers: HashSet<_> = imports
            .iter()
            .map(|import| import.specifier.as_str())
            .collect();

        assert_eq!(imports.len(), 2);
        assert!(specifiers.contains("../package-b/index.ts"));
        assert!(specifiers.contains("@repo/package-b"));
        assert!(
            imports
                .iter()
                .all(|import| import.import_type == ImportType::Value)
        );
    }

    fn ignored_comment_parts(
        source: &str,
    ) -> (Vec<turbo_trace::ImportResult>, Vec<Comment>, String) {
        let tmp = tempfile::tempdir().unwrap();
        let path = AbsoluteSystemPath::new(tmp.path().to_str().unwrap())
            .unwrap()
            .join_component("index.ts");
        let (imports, comments) = parse_with_comments(&path, source).unwrap();
        (imports, comments, source.to_string())
    }

    #[test]
    fn stacked_comments_before_an_import_are_walked_backwards() {
        let (imports, comments, source) = ignored_comment_parts(
            "// @ts-ignore\n// @boundaries-ignore implicit dependency\nimport { foo } from \
             \"bar\";\n",
        );
        assert_eq!(imports.len(), 1);
        let reason =
            BoundariesChecker::get_ignored_comment(&comments, &source, imports[0].statement_span);
        assert_eq!(reason.as_deref(), Some(" implicit dependency"));
    }

    #[test]
    fn blank_line_between_comment_and_import_stops_the_walk() {
        let (imports, comments, source) = ignored_comment_parts(
            "// @boundaries-ignore separated by a blank line\n\nimport { foo } from \"bar\";\n",
        );
        assert_eq!(imports.len(), 1);
        let reason =
            BoundariesChecker::get_ignored_comment(&comments, &source, imports[0].statement_span);
        assert_eq!(reason, None);
    }

    #[test]
    fn comments_after_the_import_are_not_considered() {
        // An import at the top of a file with many trailing comments must
        // only inspect the comments that precede it.
        let trailing: String = (0..50)
            .map(|i| format!("// trailing comment {i}\n"))
            .collect();
        let source = format!(
            "// @boundaries-ignore nearest comment\nimport {{ foo }} from \"bar\";\n{trailing}"
        );
        let (imports, comments, source) = ignored_comment_parts(&source);
        assert_eq!(imports.len(), 1);
        assert_eq!(comments.len(), 51);
        let reason =
            BoundariesChecker::get_ignored_comment(&comments, &source, imports[0].statement_span);
        assert_eq!(reason.as_deref(), Some(" nearest comment"));
    }

    #[test]
    fn blank_line_between_stacked_comments_stops_the_walk() {
        let (imports, comments, source) = ignored_comment_parts(
            "// @boundaries-ignore too far away\n// stacked comment\n\nimport { foo } from \
             \"bar\";\n",
        );
        assert_eq!(imports.len(), 1);
        let reason =
            BoundariesChecker::get_ignored_comment(&comments, &source, imports[0].statement_span);
        assert_eq!(reason, None);
    }

    #[test]
    fn test_potential_package_name() {
        assert!(BoundariesChecker::is_potential_package_name("lodash"));
        assert!(BoundariesChecker::is_potential_package_name(
            "@scope/package"
        ));
        assert!(BoundariesChecker::is_potential_package_name("my-package"));
        assert!(BoundariesChecker::is_potential_package_name("lodash/fp"));
        assert!(BoundariesChecker::is_potential_package_name(
            "@scope/package/sub"
        ));
        assert!(BoundariesChecker::is_potential_package_name(
            "@scope/package/deeply/nested"
        ));
        assert!(!BoundariesChecker::is_potential_package_name("./relative"));
        assert!(!BoundariesChecker::is_potential_package_name("../parent"));
        assert!(!BoundariesChecker::is_potential_package_name("/absolute"));
    }

    #[test]
    fn merge_accumulates_all_fields() {
        let mut a = BoundariesResult {
            files_checked: 5,
            packages_checked: 2,
            import_checks_skipped: false,
            warnings: vec!["warn-a".into()],
            diagnostics: vec![BoundariesDiagnostic::CircularDependency {
                cycle_path: "a -> b -> a".into(),
                members: Vec::new(),
            }],
            suppressed_by_baseline: 1,
        };
        let b = BoundariesResult {
            files_checked: 3,
            packages_checked: 1,
            import_checks_skipped: true,
            warnings: vec!["warn-b1".into(), "warn-b2".into()],
            diagnostics: vec![BoundariesDiagnostic::ParseError(
                AbsoluteSystemPathBuf::new(if cfg!(windows) { "C:\\bad" } else { "/bad" }).unwrap(),
                "oops".into(),
            )],
            suppressed_by_baseline: 2,
        };

        a.merge(b);

        assert_eq!(a.files_checked, 8);
        assert_eq!(a.packages_checked, 3);
        assert!(a.import_checks_skipped);
        assert_eq!(a.warnings.len(), 3);
        assert_eq!(a.diagnostics.len(), 2);
        assert_eq!(a.suppressed_by_baseline, 3);
    }

    #[test]
    fn merge_with_empty_is_identity() {
        let mut result = BoundariesResult {
            files_checked: 10,
            packages_checked: 4,
            import_checks_skipped: false,
            warnings: vec!["w".into()],
            diagnostics: vec![BoundariesDiagnostic::CircularDependency {
                cycle_path: "x -> y -> x".into(),
                members: Vec::new(),
            }],
            suppressed_by_baseline: 0,
        };

        result.merge(BoundariesResult::default());

        assert_eq!(result.files_checked, 10);
        assert_eq!(result.packages_checked, 4);
        assert_eq!(result.warnings.len(), 1);
        assert_eq!(result.diagnostics.len(), 1);
    }

    // Minimal mock providers for integration tests
    struct MockGraph {
        packages: Vec<PackageName>,
        aggregates: HashSet<PackageName>,
        authoritative_directories: HashMap<PackageName, turbopath::AnchoredSystemPathBuf>,
        definition_paths: HashMap<PackageName, turbopath::AnchoredSystemPathBuf>,
        dependencies: HashMap<PackageNode, Vec<PackageNode>>,
    }

    impl MockGraph {
        fn new(packages: Vec<PackageName>) -> Self {
            let authoritative_directories = packages
                .iter()
                .map(|name| {
                    (
                        name.clone(),
                        turbopath::AnchoredSystemPathBuf::from_raw(format!(
                            "packages/{}",
                            name.as_str()
                        ))
                        .unwrap(),
                    )
                })
                .collect();
            let definition_paths = packages
                .iter()
                .map(|name| {
                    (
                        name.clone(),
                        turbopath::AnchoredSystemPathBuf::from_raw(format!(
                            "packages/{}/package.json",
                            name.as_str()
                        ))
                        .unwrap(),
                    )
                })
                .collect();
            Self {
                packages,
                aggregates: HashSet::new(),
                authoritative_directories,
                definition_paths,
                dependencies: HashMap::new(),
            }
        }

        fn with_dependency(mut self, from: &str, to: &str) -> Self {
            self.dependencies
                .entry(PackageNode::Workspace(PackageName::Other(from.into())))
                .or_default()
                .push(PackageNode::Workspace(PackageName::Other(to.into())));
            self
        }

        fn with_aggregate(mut self, name: PackageName) -> Self {
            self.aggregates.insert(name);
            self
        }

        fn with_directory(mut self, name: PackageName, directory: &str) -> Self {
            self.authoritative_directories.insert(
                name,
                turbopath::AnchoredSystemPathBuf::from_raw(directory).unwrap(),
            );
            self
        }

        fn with_definition_path(mut self, name: PackageName, definition_path: &str) -> Self {
            self.definition_paths.insert(
                name,
                turbopath::AnchoredSystemPathBuf::from_raw(definition_path).unwrap(),
            );
            self
        }
    }

    impl PackageGraphProvider for MockGraph {
        fn package_scopes(&self) -> Box<dyn Iterator<Item = PackageScope<'_>> + '_> {
            Box::new(self.packages.iter().map(|name| {
                PackageScope {
                    name: name.clone(),
                    name_source: None,
                    directory: self
                        .authoritative_directories
                        .get(name)
                        .map(|path| path.as_ref())
                        .expect("mock package must have an authoritative directory"),
                    definition_path: self
                        .definition_paths
                        .get(name)
                        .map(|path| path.as_ref())
                        .expect("mock package must have an authoritative definition"),
                    kind: if self.aggregates.contains(name) {
                        PackageGraphNodeKind::Aggregate
                    } else {
                        PackageGraphNodeKind::Package
                    },
                }
            }))
        }

        fn immediate_dependencies(&self, node: &PackageNode) -> Option<HashSet<&PackageNode>> {
            Some(
                self.dependencies
                    .get(node)
                    .map(|dependencies| dependencies.iter().collect())
                    .unwrap_or_default(),
            )
        }

        fn dependencies(&self, node: &PackageNode) -> Box<dyn Iterator<Item = &PackageNode> + '_> {
            Box::new(self.dependencies.get(node).into_iter().flatten())
        }

        fn ancestors(&self, node: &PackageNode) -> Box<dyn Iterator<Item = &PackageNode> + '_> {
            let node = node.clone();
            Box::new(
                self.dependencies
                    .iter()
                    .filter(move |(_, dependencies)| dependencies.contains(&node))
                    .map(|(dependent, _)| dependent),
            )
        }

        fn find_cycles(&self) -> Vec<PackageCycle> {
            Vec::new()
        }
    }

    struct MockTurboJson;

    impl TurboJsonProvider for MockTurboJson {
        fn has_turbo_json(&self, _: &PackageName) -> bool {
            false
        }

        fn boundaries_config(&self, _: &PackageName) -> Option<&BoundariesConfig> {
            None
        }

        fn package_tags(&self, _: &PackageName) -> Option<&Spanned<Vec<Spanned<String>>>> {
            None
        }

        fn implicit_dependencies(&self, _: &PackageName) -> HashMap<String, Spanned<()>> {
            HashMap::new()
        }
    }

    #[test]
    fn check_boundaries_runs_through_rayon() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_root = AbsoluteSystemPath::new(tmp.path().to_str().unwrap()).unwrap();

        // Create two packages, each with one JS file containing a local import
        for pkg in &["pkg-a", "pkg-b"] {
            let pkg_dir = repo_root.join_components(&["packages", pkg]);
            std::fs::create_dir_all(pkg_dir.as_std_path()).unwrap();
            std::fs::create_dir_all(
                pkg_dir
                    .join_components(&["node_modules", "dep"])
                    .as_std_path(),
            )
            .unwrap();
            std::fs::write(
                pkg_dir.join_component("package.json").as_std_path(),
                format!(r#"{{"name": "{pkg}"}}"#),
            )
            .unwrap();
            std::fs::write(
                pkg_dir.join_component("index.ts").as_std_path(),
                "import './local';\n",
            )
            .unwrap();
            std::fs::write(
                pkg_dir
                    .join_components(&["node_modules", "dep", "index.ts"])
                    .as_std_path(),
                "import 'missing';\n",
            )
            .unwrap();
        }

        let packages = vec![
            PackageName::Other("pkg-a".into()),
            PackageName::Other("pkg-b".into()),
        ];

        let graph = MockGraph::new(packages);
        let turbo_json = MockTurboJson;
        let filtered: HashSet<PackageName> = ["pkg-a", "pkg-b"]
            .iter()
            .map(|&n| PackageName::Other(n.into()))
            .collect();

        let ctx = BoundariesContext {
            repo_root,
            pkg_dep_graph: &graph,
            turbo_json_provider: &turbo_json,
            root_boundaries_config: None,
            filtered_pkgs: &filtered,
        };

        let result = BoundariesChecker::check_boundaries(&ctx, false).unwrap();

        assert_eq!(result.packages_checked, 2);
        assert_eq!(result.files_checked, 2);
        // Local imports (./local) should not produce diagnostics
        assert!(
            result.diagnostics.is_empty(),
            "local imports should not produce diagnostics, got: {:?}",
            result.diagnostics.len()
        );
    }

    #[test]
    fn check_boundaries_ignores_gitignored_files() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_root = AbsoluteSystemPath::new(tmp.path().to_str().unwrap()).unwrap();
        let package_name = PackageName::Other("app".into());
        let package_directory = repo_root.join_components(&["packages", "app"]);

        package_directory.create_dir_all().unwrap();
        package_directory
            .join_component("dist")
            .create_dir_all()
            .unwrap();
        repo_root
            .join_component(".gitignore")
            .create_with_contents("dist/\n")
            .unwrap();
        package_directory
            .join_component("package.json")
            .create_with_contents(r#"{"name":"app"}"#)
            .unwrap();
        package_directory
            .join_component("index.ts")
            .create_with_contents("export {};\n")
            .unwrap();
        package_directory
            .join_components(&["dist", "bundle.js"])
            .create_with_contents("import 'undeclared-dependency';\n")
            .unwrap();

        let graph = MockGraph::new(vec![package_name.clone()]);
        let filtered = HashSet::from([package_name]);
        let result = BoundariesChecker::check_boundaries(
            &BoundariesContext {
                repo_root,
                pkg_dep_graph: &graph,
                turbo_json_provider: &MockTurboJson,
                root_boundaries_config: None,
                filtered_pkgs: &filtered,
            },
            false,
        )
        .unwrap();

        assert_eq!(result.files_checked, 1);
        assert!(result.diagnostics.is_empty());
    }

    struct MockTurboJsonWithBoundaries {
        configs: HashMap<PackageName, BoundariesConfig>,
    }

    impl TurboJsonProvider for MockTurboJsonWithBoundaries {
        fn has_turbo_json(&self, _: &PackageName) -> bool {
            false
        }

        fn boundaries_config(&self, pkg: &PackageName) -> Option<&BoundariesConfig> {
            self.configs.get(pkg)
        }

        fn package_tags(&self, _: &PackageName) -> Option<&Spanned<Vec<Spanned<String>>>> {
            None
        }

        fn implicit_dependencies(&self, _: &PackageName) -> HashMap<String, Spanned<()>> {
            HashMap::new()
        }
    }

    fn ignore_config(globs: &[&str]) -> BoundariesConfig {
        BoundariesConfig {
            ignore: Some(Spanned::new(
                globs
                    .iter()
                    .map(|glob| Spanned::new(glob.to_string()))
                    .collect(),
            )),
            ..Default::default()
        }
    }

    /// Creates a package with a clean `index.ts` and the given files, each
    /// importing an undeclared dependency.
    fn create_package_with_violations(repo_root: &AbsoluteSystemPath, name: &str, files: &[&str]) {
        let package_directory = repo_root.join_components(&["packages", name]);
        package_directory.create_dir_all().unwrap();
        package_directory
            .join_component("package.json")
            .create_with_contents(format!(r#"{{"name":"{name}"}}"#))
            .unwrap();
        package_directory
            .join_component("index.ts")
            .create_with_contents("export {};\n")
            .unwrap();
        for file in files {
            let path =
                package_directory.join_unix_path(turbopath::RelativeUnixPath::new(file).unwrap());
            path.ensure_dir().unwrap();
            path.create_with_contents("import 'undeclared-dependency';\n")
                .unwrap();
        }
    }

    fn diagnostic_files(repo_root: &AbsoluteSystemPath, result: &BoundariesResult) -> Vec<String> {
        let mut files: Vec<_> = result
            .diagnostics
            .iter()
            .filter_map(|diagnostic| diagnostic.path_and_span())
            .map(|(path, _)| repo_root.anchor(path).unwrap().to_unix().to_string())
            .collect();
        files.sort();
        files
    }

    #[test_case("./src/routeTree.gen.ts", &["packages/app/src/generated/client.ts"] ; "leading dot slash")]
    #[test_case("src/generated", &["packages/app/src/routeTree.gen.ts"] ; "bare directory")]
    #[test_case("src/generated/", &["packages/app/src/routeTree.gen.ts"] ; "directory with trailing slash")]
    #[test_case("./src/generated", &["packages/app/src/routeTree.gen.ts"] ; "bare directory with leading dot slash")]
    fn check_boundaries_normalizes_ignore_globs(glob: &str, expected: &[&str]) {
        let tmp = tempfile::tempdir().unwrap();
        let repo_root = AbsoluteSystemPath::new(tmp.path().to_str().unwrap()).unwrap();
        let package_name = PackageName::Other("app".into());
        create_package_with_violations(
            repo_root,
            "app",
            &["src/routeTree.gen.ts", "src/generated/client.ts"],
        );

        let graph = MockGraph::new(vec![package_name.clone()]);
        let filtered = HashSet::from([package_name]);
        let root_config = ignore_config(&[glob]);
        let result = BoundariesChecker::check_boundaries(
            &BoundariesContext {
                repo_root,
                pkg_dep_graph: &graph,
                turbo_json_provider: &MockTurboJson,
                root_boundaries_config: Some(&root_config),
                filtered_pkgs: &filtered,
            },
            false,
        )
        .unwrap();

        assert_eq!(diagnostic_files(repo_root, &result), expected);
        assert_eq!(result.files_checked, 2);
    }

    #[test]
    fn check_boundaries_skips_files_matching_root_ignore_globs() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_root = AbsoluteSystemPath::new(tmp.path().to_str().unwrap()).unwrap();
        let package_name = PackageName::Other("app".into());
        create_package_with_violations(
            repo_root,
            "app",
            &[
                "src/routeTree.gen.ts",
                "src/generated/client.ts",
                "src/app.ts",
            ],
        );

        let graph = MockGraph::new(vec![package_name.clone()]);
        let filtered = HashSet::from([package_name]);
        let root_config = ignore_config(&["**/routeTree.gen.ts", "src/generated/**"]);
        let result = BoundariesChecker::check_boundaries(
            &BoundariesContext {
                repo_root,
                pkg_dep_graph: &graph,
                turbo_json_provider: &MockTurboJson,
                root_boundaries_config: Some(&root_config),
                filtered_pkgs: &filtered,
            },
            false,
        )
        .unwrap();

        assert_eq!(
            diagnostic_files(repo_root, &result),
            ["packages/app/src/app.ts"]
        );
        // Ignored files are not walked, so only `index.ts` and `src/app.ts`
        // are counted.
        assert_eq!(result.files_checked, 2);
    }

    #[test]
    fn check_boundaries_combines_root_and_package_ignore_globs() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_root = AbsoluteSystemPath::new(tmp.path().to_str().unwrap()).unwrap();
        let app = PackageName::Other("app".into());
        let lib = PackageName::Other("lib".into());
        for name in ["app", "lib"] {
            create_package_with_violations(repo_root, name, &["root.gen.ts", "package.gen.ts"]);
        }

        let graph = MockGraph::new(vec![app.clone(), lib.clone()]);
        let filtered = HashSet::from([app.clone(), lib]);
        let root_config = ignore_config(&["root.gen.ts"]);
        let turbo_json = MockTurboJsonWithBoundaries {
            configs: HashMap::from([(app, ignore_config(&["package.gen.ts"]))]),
        };
        let result = BoundariesChecker::check_boundaries(
            &BoundariesContext {
                repo_root,
                pkg_dep_graph: &graph,
                turbo_json_provider: &turbo_json,
                root_boundaries_config: Some(&root_config),
                filtered_pkgs: &filtered,
            },
            false,
        )
        .unwrap();

        // `app` ignores both files, `lib` only ignores the root glob.
        assert_eq!(
            diagnostic_files(repo_root, &result),
            ["packages/lib/package.gen.ts"]
        );
        assert_eq!(result.files_checked, 3);
    }

    #[test_case("src/[" ; "unclosed character class")]
    #[test_case("!**/*.gen.ts" ; "negation")]
    #[test_case("" ; "empty")]
    #[test_case("." ; "package directory")]
    #[test_case("src/.." ; "package directory after cleaning")]
    #[test_case(".." ; "parent directory")]
    #[test_case("../**" ; "parent directory glob")]
    #[test_case("./../other/**" ; "parent directory after cleaning")]
    fn check_boundaries_rejects_invalid_ignore_globs(glob: &str) {
        let tmp = tempfile::tempdir().unwrap();
        let repo_root = AbsoluteSystemPath::new(tmp.path().to_str().unwrap()).unwrap();
        let package_name = PackageName::Other("app".into());
        create_package_with_violations(repo_root, "app", &[]);

        let graph = MockGraph::new(vec![package_name.clone()]);
        let filtered = HashSet::from([package_name.clone()]);
        let turbo_json = MockTurboJsonWithBoundaries {
            configs: HashMap::from([(package_name, ignore_config(&[glob]))]),
        };
        let result = BoundariesChecker::check_boundaries(
            &BoundariesContext {
                repo_root,
                pkg_dep_graph: &graph,
                turbo_json_provider: &turbo_json,
                root_boundaries_config: None,
                filtered_pkgs: &filtered,
            },
            false,
        );

        match result {
            Err(Error::InvalidIgnoreGlob(invalid)) => assert_eq!(invalid.glob, glob),
            Err(error) => panic!("expected InvalidIgnoreGlob, got {error}"),
            Ok(_) => panic!("expected InvalidIgnoreGlob, got Ok"),
        }
    }

    #[test]
    fn check_boundaries_reports_every_package_across_progress_batches() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_root = AbsoluteSystemPath::new(tmp.path().to_str().unwrap()).unwrap();
        let packages: Vec<_> = (0..=PROGRESS_UPDATE_BATCH_SIZE)
            .map(|index| PackageName::Other(format!("pkg-{index}")))
            .collect();

        for package in &packages {
            let package_directory = repo_root.join_components(&["packages", package.as_str()]);
            package_directory.create_dir_all().unwrap();
            package_directory
                .join_component("package.json")
                .create_with_contents(format!(r#"{{"name":"{package}"}}"#))
                .unwrap();
            package_directory
                .join_component("index.ts")
                .create_with_contents("export {};\n")
                .unwrap();
        }

        let graph = MockGraph::new(packages.clone());
        let filtered = packages.into_iter().collect();
        let result = BoundariesChecker::check_boundaries(
            &BoundariesContext {
                repo_root,
                pkg_dep_graph: &graph,
                turbo_json_provider: &MockTurboJson,
                root_boundaries_config: None,
                filtered_pkgs: &filtered,
            },
            true,
        )
        .unwrap();

        assert_eq!(result.packages_checked, PROGRESS_UPDATE_BATCH_SIZE + 1);
        assert_eq!(result.files_checked, PROGRESS_UPDATE_BATCH_SIZE + 1);
        assert!(result.diagnostics.is_empty());
    }

    #[test]
    fn check_boundaries_excludes_aggregate_scopes() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_root = AbsoluteSystemPath::new(tmp.path().to_str().unwrap()).unwrap();
        let aggregate_name = PackageName::Other("cargo-workspace".into());
        let graph =
            MockGraph::new(vec![aggregate_name.clone()]).with_aggregate(aggregate_name.clone());
        let filtered = HashSet::from([aggregate_name]);
        let result = BoundariesChecker::check_boundaries(
            &BoundariesContext {
                repo_root,
                pkg_dep_graph: &graph,
                turbo_json_provider: &MockTurboJson,
                root_boundaries_config: None,
                filtered_pkgs: &filtered,
            },
            false,
        )
        .unwrap();

        assert_eq!(result.packages_checked, 0);
        assert_eq!(result.files_checked, 0);
    }

    #[test]
    fn check_boundaries_excludes_non_package_json_definitions() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_root = AbsoluteSystemPath::new(tmp.path().to_str().unwrap()).unwrap();
        let package_name = PackageName::Other("rust-crate".into());
        let graph = MockGraph::new(vec![package_name.clone()])
            .with_definition_path(package_name.clone(), "crates/rust-crate/Cargo.toml");
        let filtered = HashSet::from([package_name]);

        let result = BoundariesChecker::check_boundaries(
            &BoundariesContext {
                repo_root,
                pkg_dep_graph: &graph,
                turbo_json_provider: &MockTurboJson,
                root_boundaries_config: None,
                filtered_pkgs: &filtered,
            },
            false,
        )
        .unwrap();

        assert_eq!(result.packages_checked, 0);
        assert_eq!(result.files_checked, 0);
    }

    #[test]
    fn check_boundaries_selects_package_json_in_mixed_definitions() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_root = AbsoluteSystemPath::new(tmp.path().to_str().unwrap()).unwrap();
        let web_name = PackageName::Other("web".into());
        let rust_name = PackageName::Other("rust-crate".into());
        let web_directory = repo_root.join_components(&["packages", "web"]);
        web_directory.create_dir_all().unwrap();
        web_directory
            .join_component("index.ts")
            .create_with_contents("export {};\n")
            .unwrap();

        let graph = MockGraph::new(vec![web_name.clone(), rust_name.clone()])
            .with_definition_path(rust_name.clone(), "crates/rust-crate/Cargo.toml");
        let filtered = HashSet::from([web_name, rust_name]);

        let result = BoundariesChecker::check_boundaries(
            &BoundariesContext {
                repo_root,
                pkg_dep_graph: &graph,
                turbo_json_provider: &MockTurboJson,
                root_boundaries_config: None,
                filtered_pkgs: &filtered,
            },
            false,
        )
        .unwrap();

        assert_eq!(result.packages_checked, 1);
        assert_eq!(result.files_checked, 1);
    }

    #[test]
    fn authoritative_package_does_not_require_compatibility_payload() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_root = AbsoluteSystemPath::new(tmp.path().to_str().unwrap()).unwrap();
        let package_name = PackageName::Other("authoritative-web".into());
        let graph = MockGraph::new(vec![package_name.clone()]);
        let filtered = HashSet::from([package_name.clone()]);

        let result = BoundariesChecker::check_boundaries(
            &BoundariesContext {
                repo_root,
                pkg_dep_graph: &graph,
                turbo_json_provider: &MockTurboJson,
                root_boundaries_config: None,
                filtered_pkgs: &filtered,
            },
            false,
        )
        .unwrap();

        assert_eq!(result.packages_checked, 1);
    }

    #[test]
    fn check_boundaries_scans_authoritative_package_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_root = AbsoluteSystemPath::new(tmp.path().to_str().unwrap()).unwrap();
        let package_name = PackageName::Other("web".into());
        repo_root
            .join_components(&["actual", "web"])
            .create_dir_all()
            .unwrap();
        repo_root
            .join_components(&["actual", "web", "index.ts"])
            .create_with_contents("export {};\n")
            .unwrap();
        let graph = MockGraph::new(vec![package_name.clone()])
            .with_directory(package_name.clone(), "actual/web");
        let filtered = HashSet::from([package_name]);

        let result = BoundariesChecker::check_boundaries(
            &BoundariesContext {
                repo_root,
                pkg_dep_graph: &graph,
                turbo_json_provider: &MockTurboJson,
                root_boundaries_config: None,
                filtered_pkgs: &filtered,
            },
            false,
        )
        .unwrap();

        assert_eq!(result.packages_checked, 1);
        assert_eq!(result.files_checked, 1);
    }

    /// Runs `check_boundaries` on `web` in a workspace of `web`, `ui` and
    /// `utils` (all under `packages/`) plus a non-package `shared/` directory.
    /// `web`'s tsconfig has a path alias into each of them and `web` depends
    /// only on `ui`. Returns the diagnostic messages for `web`'s `source`.
    fn check_web_with_tsconfig_aliases(source: &str) -> Vec<String> {
        let tmp = tempfile::tempdir().unwrap();
        // Canonicalize to match the resolver's symlink-resolved paths.
        let root = dunce::canonicalize(tmp.path()).unwrap();
        let repo_root = AbsoluteSystemPath::new(root.to_str().unwrap()).unwrap();

        for package in ["web", "ui", "utils"] {
            let package_directory = repo_root.join_components(&["packages", package]);
            package_directory
                .join_component("src")
                .create_dir_all()
                .unwrap();
            package_directory
                .join_component("package.json")
                .create_with_contents(format!(r#"{{"name":"{package}"}}"#))
                .unwrap();
            package_directory
                .join_components(&["src", "index.ts"])
                .create_with_contents("export const x = 1;\n")
                .unwrap();
        }
        repo_root.join_component("shared").create_dir_all().unwrap();
        repo_root
            .join_components(&["shared", "index.ts"])
            .create_with_contents("export const x = 1;\n")
            .unwrap();

        let web = repo_root.join_components(&["packages", "web"]);
        web.join_component("tsconfig.json")
            .create_with_contents(
                r#"{ "compilerOptions": { "paths": {
                    "@ui/*": ["../ui/src/*"],
                    "@utils/*": ["../utils/src/*"],
                    "@shared/*": ["../../shared/*"]
                } } }"#,
            )
            .unwrap();
        web.join_components(&["src", "index.ts"])
            .create_with_contents(source)
            .unwrap();

        let packages = ["web", "ui", "utils"].map(|name| PackageName::Other(name.into()));
        let graph = MockGraph::new(packages.to_vec())
            .with_dependency(packages[0].as_str(), packages[1].as_str());
        let filtered = HashSet::from([packages[0].clone()]);
        let result = BoundariesChecker::check_boundaries(
            &BoundariesContext {
                repo_root,
                pkg_dep_graph: &graph,
                turbo_json_provider: &MockTurboJson,
                root_boundaries_config: None,
                filtered_pkgs: &filtered,
            },
            false,
        )
        .unwrap();

        result
            .diagnostics
            .iter()
            .map(|diagnostic| diagnostic.to_string())
            .collect()
    }

    /// Regression test: a tsconfig path alias that resolves into a declared
    /// workspace dependency is an import of that package, not an import that
    /// leaves the package.
    #[test]
    fn tsconfig_alias_into_declared_workspace_dependency_is_allowed() {
        let diagnostics = check_web_with_tsconfig_aliases("import { x } from \"@ui/index\";\n");

        assert_eq!(diagnostics, Vec::<String>::new());
    }

    /// A tsconfig path alias into a workspace package that isn't a dependency
    /// is reported as an undeclared dependency on that package.
    #[test]
    fn tsconfig_alias_into_undeclared_workspace_package_is_not_a_dependency() {
        let diagnostics = check_web_with_tsconfig_aliases("import { x } from \"@utils/index\";\n");

        assert_eq!(
            diagnostics,
            ["cannot import package `utils` because it is not a dependency"]
        );
    }

    /// A tsconfig path alias that resolves outside of every workspace package
    /// still leaves the package.
    #[test]
    fn tsconfig_alias_outside_workspace_packages_leaves_the_package() {
        let diagnostics = check_web_with_tsconfig_aliases("import { x } from \"@shared/index\";\n");

        assert_eq!(diagnostics, ["import `@shared/index` leaves the package"]);
    }

    #[test]
    fn check_boundaries_skips_import_checks_when_disabled() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_root = AbsoluteSystemPath::new(tmp.path().to_str().unwrap()).unwrap();
        let package_name = PackageName::Other("app".into());
        let package_directory = repo_root.join_components(&["packages", "app"]);
        package_directory.create_dir_all().unwrap();
        package_directory
            .join_component("package.json")
            .create_with_contents(r#"{"name":"app"}"#)
            .unwrap();
        package_directory
            .join_component("index.ts")
            .create_with_contents("import 'undeclared-dependency';\nimport '../outside';\n")
            .unwrap();

        let graph = MockGraph::new(vec![package_name.clone()]);
        let filtered = HashSet::from([package_name]);
        let check = |import_checks: Option<bool>| {
            let root_boundaries_config = BoundariesConfig {
                import_checks: import_checks.map(Spanned::new),
                ..Default::default()
            };
            BoundariesChecker::check_boundaries(
                &BoundariesContext {
                    repo_root,
                    pkg_dep_graph: &graph,
                    turbo_json_provider: &MockTurboJson,
                    root_boundaries_config: Some(&root_boundaries_config),
                    filtered_pkgs: &filtered,
                },
                false,
            )
            .unwrap()
        };

        for enabled in [None, Some(true)] {
            let result = check(enabled);
            assert!(!result.import_checks_skipped);
            assert_eq!(result.files_checked, 1);
            assert_eq!(result.diagnostics.len(), 2);
        }

        let result = check(Some(false));
        assert!(result.import_checks_skipped);
        assert_eq!(result.packages_checked, 1);
        assert_eq!(result.files_checked, 0);
        assert!(
            result.diagnostics.is_empty(),
            "import diagnostics should not be reported when import checks are disabled"
        );
    }

    fn root_boundaries_config(json: &str) -> BoundariesConfig {
        use turborepo_errors::WithMetadata;

        let (config, errors) = turborepo_errors::json::deserialize_from_json_str::<BoundariesConfig>(
            json,
            biome_json_parser::JsonParserOptions::default(),
            "turbo.json",
        );
        assert!(errors.is_empty(), "invalid test config");
        let mut config = config.unwrap();
        config.add_text(Arc::from(json));
        config.add_path("turbo.json".into());
        config
    }

    #[test]
    fn central_tags_apply_to_packages_without_turbo_json() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_root = AbsoluteSystemPath::new(tmp.path().to_str().unwrap()).unwrap();
        let storefront = PackageName::Other("storefront".into());
        let unsafe_lib = PackageName::Other("unsafe-lib".into());
        let graph = MockGraph::new(vec![storefront.clone(), unsafe_lib.clone()])
            .with_directory(storefront.clone(), "apps/storefront")
            .with_directory(unsafe_lib.clone(), "packages/unsafe-lib")
            .with_dependency("storefront", "unsafe-lib");
        let config = root_boundaries_config(
            r#"{
                "packageTags": {
                    "apps/*": ["web"],
                    "packages/unsafe-*": ["unsafe"]
                },
                "tags": {
                    "web": { "dependencies": { "deny": ["unsafe"] } }
                }
            }"#,
        );
        let filtered = HashSet::from([storefront, unsafe_lib]);

        let result = BoundariesChecker::check_boundaries(
            &BoundariesContext {
                repo_root,
                pkg_dep_graph: &graph,
                turbo_json_provider: &MockTurboJson,
                root_boundaries_config: Some(&config),
                filtered_pkgs: &filtered,
            },
            false,
        )
        .unwrap();

        assert!(result.warnings.is_empty(), "{:?}", result.warnings);
        assert_eq!(result.diagnostics.len(), 1, "{:?}", result.diagnostics);
        let BoundariesDiagnostic::DeniedTag {
            source_package_name,
            package_name,
            tag,
            span,
            text,
            ..
        } = &result.diagnostics[0]
        else {
            panic!("expected denied-tag diagnostic");
        };
        assert_eq!(source_package_name.as_str(), "storefront");
        assert_eq!(package_name.as_str(), "unsafe-lib");
        assert_eq!(tag, "unsafe");
        // The offending tag is reported where it is assigned: the root
        // turbo.json's `packageTags`.
        assert_eq!(text.name(), "turbo.json");
        let span = span.unwrap();
        let assigned = &text.inner()[span.offset()..span.offset() + span.len()];
        assert_eq!(assigned, "\"unsafe\"");
        let assigned_in = &text.inner()[..span.offset()];
        assert!(assigned_in.ends_with(r#""packages/unsafe-*": ["#));
    }

    /// Counts tag lookups so tests can assert that tags are not loaded when
    /// no rule reads them.
    #[derive(Default)]
    struct CountingTurboJson {
        package_tags_calls: std::sync::atomic::AtomicUsize,
    }

    impl TurboJsonProvider for CountingTurboJson {
        fn has_turbo_json(&self, _: &PackageName) -> bool {
            false
        }

        fn boundaries_config(&self, _: &PackageName) -> Option<&BoundariesConfig> {
            None
        }

        fn package_tags(&self, _: &PackageName) -> Option<&Spanned<Vec<Spanned<String>>>> {
            self.package_tags_calls
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            None
        }

        fn implicit_dependencies(&self, _: &PackageName) -> HashMap<String, Spanned<()>> {
            HashMap::new()
        }
    }

    #[test]
    fn package_tags_are_only_loaded_when_rules_read_them() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_root = AbsoluteSystemPath::new(tmp.path().to_str().unwrap()).unwrap();
        let packages = vec![
            PackageName::Other("pkg-a".into()),
            PackageName::Other("pkg-b".into()),
        ];
        let graph = MockGraph::new(packages.clone()).with_dependency("pkg-a", "pkg-b");
        let filtered: HashSet<_> = packages.into_iter().collect();
        let check = |config: Option<&BoundariesConfig>| {
            let provider = CountingTurboJson::default();
            BoundariesChecker::check_boundaries(
                &BoundariesContext {
                    repo_root,
                    pkg_dep_graph: &graph,
                    turbo_json_provider: &provider,
                    root_boundaries_config: config,
                    filtered_pkgs: &filtered,
                },
                false,
            )
            .unwrap();
            provider
                .package_tags_calls
                .load(std::sync::atomic::Ordering::Relaxed)
        };

        assert_eq!(check(None), 0);
        assert_eq!(check(Some(&root_boundaries_config(r#"{ "tags": {} }"#))), 0);
        assert_eq!(
            check(Some(&root_boundaries_config(
                r#"{ "tags": { "web": { "dependencies": { "deny": ["node"] } } } }"#
            ))),
            2
        );
    }

    #[test]
    fn package_tags_problems_are_reported_once() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_root = AbsoluteSystemPath::new(tmp.path().to_str().unwrap()).unwrap();
        let web = PackageName::Other("web".into());
        let graph = MockGraph::new(vec![web.clone()]);
        let config = root_boundaries_config(
            r#"{ "packageTags": { "packages/[": ["broken"], "services/*": ["service"] } }"#,
        );
        let filtered = HashSet::from([web]);

        let result = BoundariesChecker::check_boundaries(
            &BoundariesContext {
                repo_root,
                pkg_dep_graph: &graph,
                turbo_json_provider: &MockTurboJson,
                root_boundaries_config: Some(&config),
                filtered_pkgs: &filtered,
            },
            false,
        )
        .unwrap();

        assert_eq!(result.diagnostics.len(), 1);
        assert!(matches!(
            &result.diagnostics[0],
            BoundariesDiagnostic::InvalidPackageTagsGlob { glob, .. } if glob == "packages/["
        ));
        assert_eq!(
            result.warnings,
            vec![
                "`boundaries.packageTags` glob `services/*` does not match any package directory"
                    .to_string()
            ]
        );
    }

    #[test]
    fn merge_preserves_ordering() {
        let mut a = BoundariesResult {
            diagnostics: vec![BoundariesDiagnostic::CircularDependency {
                cycle_path: "first".into(),
                members: Vec::new(),
            }],
            ..Default::default()
        };
        let b = BoundariesResult {
            diagnostics: vec![BoundariesDiagnostic::CircularDependency {
                cycle_path: "second".into(),
                members: Vec::new(),
            }],
            ..Default::default()
        };

        a.merge(b);

        match (&a.diagnostics[0], &a.diagnostics[1]) {
            (
                BoundariesDiagnostic::CircularDependency {
                    cycle_path: first, ..
                },
                BoundariesDiagnostic::CircularDependency {
                    cycle_path: second, ..
                },
            ) => {
                assert_eq!(first, "first");
                assert_eq!(second, "second");
            }
            _ => panic!("expected CircularDependency variants"),
        }
    }
}
