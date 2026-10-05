use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use camino::Utf8Path;
use miette::{NamedSource, SourceSpan};
use oxc_ast::ast::Comment;
use oxc_span::Span;
use tracing::debug;
use turbo_trace::ImportType;
use turbopath::{
    AbsoluteSystemPath, AbsoluteSystemPathBuf, AnchoredSystemPathBuf, PathRelation,
    RelativeUnixPath,
};
use turborepo_errors::Spanned;
use turborepo_repository::{
    external_resolution::PackageExternalDeclarations,
    package_graph::{PackageName, PackageNode},
};
use unrs_resolver::{ResolveError, Resolver};

use crate::{BoundariesChecker, BoundariesDiagnostic, Error};

/// All the places a dependency can be declared
#[derive(Clone, Copy)]
pub struct DependencyLocations<'a> {
    // The containing package's name. We allow a package to import itself per JavaScript convention
    pub(crate) package: &'a PackageName,
    pub(crate) internal_dependencies: &'a HashSet<&'a PackageNode>,
    pub(crate) external_declarations: PackageExternalDeclarations<'a>,
    pub(crate) implicit_dependencies: &'a HashMap<String, Spanned<()>>,
    pub(crate) global_implicit_dependencies: &'a HashMap<String, Spanned<()>>,
    // The directories of every workspace package, used to attribute files that
    // a tsconfig path alias resolves to back to the package that owns them
    pub(crate) workspace_packages: &'a WorkspacePackageDirectories,
}

impl<'a> DependencyLocations<'a> {
    /// Go through all the possible places a package could be declared to see if
    /// it's a valid import. We don't use `unrs_resolver` because there are some
    /// cases where you can resolve a package that isn't declared properly.
    fn is_dependency(&self, package_name: &PackageNode) -> bool {
        // The containing package's name. We allow a package to import itself per
        // JavaScript convention
        self.package == package_name.as_package_name()
            || self.internal_dependencies.contains(package_name)
            || self.external_declarations.iter().any(|declaration| {
                let package_name = package_name.as_package_name().as_str();
                declaration.declaration_name() == package_name
                    || declaration.package_name() == package_name
            })
            || self
                .implicit_dependencies
                .contains_key(package_name.as_package_name().as_str())
            || self
                .global_implicit_dependencies
                .contains_key(package_name.as_package_name().as_str())
    }
}

/// Maps workspace package directories to their package names, so a resolved
/// file path can be attributed to the workspace package that contains it.
///
/// Built once per `check_boundaries` run and shared across all packages.
#[derive(Debug, Default)]
pub struct WorkspacePackageDirectories {
    by_directory: HashMap<AbsoluteSystemPathBuf, PackageName>,
}

impl WorkspacePackageDirectories {
    /// Directories are canonicalized (once, here) because the resolver
    /// returns symlink-resolved paths. Directories that can't be
    /// canonicalized are kept as given.
    pub(crate) fn new(
        packages: impl IntoIterator<Item = (AbsoluteSystemPathBuf, PackageName)>,
    ) -> Self {
        Self {
            by_directory: packages
                .into_iter()
                .map(|(directory, name)| (directory.to_realpath().unwrap_or(directory), name))
                .collect(),
        }
    }

    /// Returns the workspace package whose directory contains `path`, which
    /// is expected to be symlink-resolved. When packages are nested, the
    /// deepest (most specific) package wins.
    ///
    /// Costs one hash lookup per ancestor of `path`.
    pub(crate) fn package_containing(&self, path: &AbsoluteSystemPath) -> Option<&PackageName> {
        path.ancestors()
            .find_map(|directory| self.by_directory.get(directory))
    }
}

/// Checks if the given import can be resolved as a tsconfig path alias via the
/// resolver, e.g. `@/types/foo` -> `./src/foo` or `features/foo` ->
/// `./src/features/foo`, and if so, checks the resolved path against package
/// boundaries.
///
/// Called for all non-relative imports in `check_import`. This allows tsconfig
/// `paths` entries — whether they shadow package-name-shaped specifiers or use
/// non-package-name patterns like `!` or `@/foo` — to be recognised as local
/// imports instead of being incorrectly flagged as undeclared dependencies.
///
/// Returns `Ok((true, diag))` if the import was resolved as a tsconfig path
/// alias. If the alias resolves outside the current package:
/// - into another workspace package, it is treated as an import of that package
///   and validated against declared dependencies (see
///   [`check_aliased_workspace_import`]),
/// - outside of every workspace package, or via a specifier that itself walks
///   up with `..` segments (e.g. `@/../../packages/ui/src`), it produces an
///   `ImportLeavesPackage` diagnostic via [`check_file_import`].
///
/// Returns `Ok((false, None))` if the resolved path goes through
/// `node_modules` (a real npm package) or if the resolver could not resolve
/// the import. The caller should then fall through to `check_package_import`.
fn check_import_as_tsconfig_path_alias(
    resolver: &Resolver,
    package_root: &AbsoluteSystemPath,
    span: SourceSpan,
    file_path: &AbsoluteSystemPath,
    file_content: &Arc<str>,
    import: &str,
    dependency_locations: DependencyLocations<'_>,
) -> Result<(bool, Option<BoundariesDiagnostic>), Error> {
    // Safety guard — relative imports are resolved as file imports elsewhere.
    if import.starts_with('.') {
        return Ok((false, None));
    }

    let dir = file_path
        .parent()
        .ok_or_else(|| Error::NoParentDir(file_path.to_owned()))?;

    match resolver.resolve(dir, import) {
        Ok(resolution) => {
            // If the resolved path goes through node_modules, the import
            // resolved to a real npm package rather than a tsconfig path alias
            // pointing to a local file.  Return false so the caller falls
            // through to `check_package_import`.
            let path = resolution.path();
            if path.components().any(|c| c.as_os_str() == "node_modules") {
                return Ok((false, None));
            }
            // Workspace packages are symlinked in node_modules, so the
            // resolved path won't contain `node_modules` after symlink
            // resolution. Detect these by checking if the resolution's
            // package.json name matches the import's package name — if so,
            // the resolver found the actual package, not a tsconfig alias.
            if BoundariesChecker::is_potential_package_name(import) {
                let import_pkg_name = get_package_name(import);
                if let Some(pkg_json) = resolution.package_json()
                    && pkg_json.name() == Some(import_pkg_name)
                {
                    return Ok((false, None));
                }
            }
            let Some(utf8_path) = Utf8Path::from_path(path) else {
                return Ok((
                    true,
                    Some(BoundariesDiagnostic::InvalidPath {
                        path: path.to_string_lossy().to_string(),
                    }),
                ));
            };
            let resolved_import_path = AbsoluteSystemPath::new(utf8_path)?;
            let diag = check_file_import(
                file_path,
                package_root,
                dependency_locations.package,
                import,
                resolved_import_path,
                span,
                file_content,
            )?;
            // An alias that leaves the current package but lands inside another
            // workspace package is an import of that package, not a path that
            // escapes the workspace. Specifiers that walk up with `..` are
            // still reaching into another package by path, so they keep the
            // `ImportLeavesPackage` diagnostic.
            let diag = match diag {
                Some(BoundariesDiagnostic::ImportLeavesPackage { .. })
                    if !import.split('/').any(|segment| segment == "..") =>
                {
                    match dependency_locations
                        .workspace_packages
                        .package_containing(resolved_import_path)
                    {
                        Some(target_package) => check_aliased_workspace_import(
                            import,
                            target_package,
                            span,
                            file_path,
                            file_content,
                            dependency_locations,
                        ),
                        None => diag,
                    }
                }
                diag => diag,
            };
            Ok((true, diag))
        }
        // Expected resolution failures — the import isn't a tsconfig alias.
        Err(
            ResolveError::NotFound(_)
            | ResolveError::MatchedAliasNotFound(_, _)
            | ResolveError::Builtin { .. }
            | ResolveError::Ignored(_)
            | ResolveError::Specifier(_),
        ) => Ok((false, None)),
        // Unexpected errors (I/O, broken tsconfig, etc.) — log for debugging
        // but still fall through to check_package_import.
        Err(e) => {
            debug!(
                import = %import,
                error = %e,
                "tsconfig path alias resolution failed unexpectedly, \
                 falling through to package import check"
            );
            Ok((false, None))
        }
    }
}

/// Validates a single import statement against package boundaries.
///
/// Dispatches to one of three paths:
/// 1. Relative imports (`./`, `../`) — validates the resolved path stays within
///    the package via [`check_file_import`].
/// 2. Non-relative imports — first tries
///    [`check_import_as_tsconfig_path_alias`] to resolve tsconfig `paths`
///    entries, then for package-name-shaped imports falls through to
///    [`check_package_import`] (validates the import is a declared dependency).
/// 3. Non-relative, non-package-name imports that don't resolve as tsconfig
///    aliases — skipped (no diagnostic).
///
/// Respects `@boundaries-ignore` comments placed above the import statement.
#[expect(clippy::too_many_arguments)]
pub(crate) fn check_import(
    comments: &[Comment],
    source_text: &str,
    diagnostics: &mut Vec<BoundariesDiagnostic>,
    warnings: &mut Vec<String>,
    package_name: &PackageName,
    package_root: &AbsoluteSystemPath,
    import: &str,
    import_type: &ImportType,
    span: &Span,
    statement_span: &Span,
    file_path: &AbsoluteSystemPath,
    file_content: &Arc<str>,
    dependency_locations: DependencyLocations<'_>,
    resolver: &Resolver,
) -> Result<(), Error> {
    // If the import is prefixed with `@boundaries-ignore`, we ignore it, but print
    // a warning
    match BoundariesChecker::get_ignored_comment(comments, source_text, *statement_span) {
        Some(reason) if reason.is_empty() => {
            warnings.push(
                "@boundaries-ignore requires a reason, e.g. `// @boundaries-ignore implicit \
                 dependency`"
                    .to_string(),
            );
        }
        Some(_) => {
            let line = source_text[..span.start as usize]
                .chars()
                .filter(|&c| c == '\n')
                .count();
            warnings.push(format!("ignoring import on line {line} in {file_path}"));

            return Ok(());
        }
        None => {}
    }

    let start = span.start as usize;
    let end = span.end as usize;

    let span = SourceSpan::new(start.into(), end - start);

    let check_result = if import.starts_with(".") {
        // Relative file import
        let import_path = RelativeUnixPath::new(import)?;
        let dir_path = file_path
            .parent()
            .ok_or_else(|| Error::NoParentDir(file_path.to_owned()))?;
        let resolved_import_path = dir_path.join_unix_path(import_path).clean()?;
        check_file_import(
            file_path,
            package_root,
            package_name,
            import,
            &resolved_import_path,
            span,
            file_content,
        )?
    } else {
        // Non-relative import: try tsconfig alias resolution first. This
        // handles both package-name-shaped imports (where the alias may
        // shadow a package name) and non-package-name imports (like `!` or
        // `@/foo`) that can only be tsconfig aliases.
        let (resolved, diag) = check_import_as_tsconfig_path_alias(
            resolver,
            package_root,
            span,
            file_path,
            file_content,
            import,
            dependency_locations,
        )?;
        if resolved {
            diagnostics.extend(diag);
            return Ok(());
        }
        if BoundariesChecker::is_potential_package_name(import) {
            check_package_import(
                import,
                *import_type,
                span,
                file_path,
                file_content,
                dependency_locations,
                resolver,
            )
        } else {
            None
        }
    };

    diagnostics.extend(check_result);

    Ok(())
}

/// Validates an import that a tsconfig path alias resolved into
/// `target_package`'s directory. The import is allowed if `target_package` is
/// a declared dependency of the importing package; otherwise it is reported as
/// an undeclared package import.
fn check_aliased_workspace_import(
    import: &str,
    target_package: &PackageName,
    span: SourceSpan,
    file_path: &AbsoluteSystemPath,
    file_content: &Arc<str>,
    dependency_locations: DependencyLocations<'_>,
) -> Option<BoundariesDiagnostic> {
    let target_node = PackageNode::Workspace(target_package.clone());
    if dependency_locations.is_dependency(&target_node) {
        return None;
    }

    Some(BoundariesDiagnostic::PackageNotFound {
        path: file_path.to_owned(),
        name: target_package.to_string(),
        help: Some(format!(
            "`{import}` is a tsconfig path alias that resolves into package `{target_package}`"
        )),
        span,
        text: NamedSource::new(file_path.as_str(), file_content.clone()),
    })
}

/// Checks whether a resolved file import stays within the package boundary.
///
/// Returns `Some(BoundariesDiagnostic::ImportLeavesPackage)` if the resolved
/// path falls outside `package_path`, `None` otherwise.
pub(crate) fn check_file_import(
    file_path: &AbsoluteSystemPath,
    package_path: &AbsoluteSystemPath,
    package_name: &PackageName,
    import: &str,
    resolved_import_path: &AbsoluteSystemPath,
    source_span: SourceSpan,
    file_content: &Arc<str>,
) -> Result<Option<BoundariesDiagnostic>, Error> {
    // We have to check for this case because `relation_to_path` returns `Parent` if
    // the paths are equal and there's nothing wrong with importing the
    // package you're in.
    if resolved_import_path.as_str() == package_path.as_str() {
        return Ok(None);
    }
    // Imports that resolve into `node_modules` point at vendored
    // dependencies, not at another workspace package's source. Generated
    // code (e.g. SvelteKit's `.svelte-kit` output) commonly imports
    // dependencies via long relative paths like
    // `../../../node_modules/@sveltejs/kit/...`, which should not be
    // flagged as leaving the package.
    if resolved_import_path
        .components()
        .any(|c| c.as_str() == "node_modules")
    {
        return Ok(None);
    }
    // We use `relation_to_path` and not `contains` because `contains`
    // panics on invalid paths with too many `..` components
    if !matches!(
        package_path.relation_to_path(resolved_import_path),
        PathRelation::Parent
    ) {
        let resolved_import_path =
            AnchoredSystemPathBuf::relative_path_between(package_path, resolved_import_path)
                .to_string();

        Ok(Some(BoundariesDiagnostic::ImportLeavesPackage {
            path: file_path.to_owned(),
            import: import.to_string(),
            resolved_import_path,
            package_name: package_name.to_owned(),
            span: source_span,
            text: NamedSource::new(file_path.as_str(), file_content.clone()),
        }))
    } else {
        Ok(None)
    }
}

/// Returns true if the import specifier refers to a Bun runtime builtin module.
///
/// Bun provides its own built-in modules (`bun`, `bun:test`, `bun:sqlite`,
/// etc.) that are available at runtime but are not Node.js builtins. Without
/// this check, a project with `@types/bun` in devDependencies would incorrectly
/// flag `import { $ } from "bun"` as a type-only import.
fn is_bun_builtin(import: &str) -> bool {
    import == "bun" || import.starts_with("bun:")
}

/// Returns true if the import specifier refers to the VS Code extension host
/// module. The `vscode` module is injected by the VS Code runtime and doesn't
/// exist on npm — extensions only have `@types/vscode` in devDependencies for
/// type-checking. Without this check, `import { window } from "vscode"` would
/// be incorrectly flagged as needing a type-only import.
fn is_vscode_module(import: &str) -> bool {
    import == "vscode"
}

/// Extracts the npm package name from an import specifier.
///
/// For scoped packages (`@scope/name/path`), returns `@scope/name`.
/// For unscoped packages (`name/path`), returns `name`.
/// For bare imports without subpaths, returns the import as-is.
pub(crate) fn get_package_name(import: &str) -> &str {
    if import.starts_with("@") {
        // Find the second '/' for scoped packages: @scope/name/path -> @scope/name
        match import.find('/') {
            Some(first_slash) => match import[first_slash + 1..].find('/') {
                Some(second_slash) => &import[..first_slash + 1 + second_slash],
                None => import,
            },
            None => import,
        }
    } else {
        import
            .split_once("/")
            .map(|(name, _)| name)
            .unwrap_or(import)
    }
}

pub(crate) fn check_package_import(
    import: &str,
    import_type: ImportType,
    span: SourceSpan,
    file_path: &AbsoluteSystemPath,
    file_content: &Arc<str>,
    dependency_locations: DependencyLocations<'_>,
    resolver: &Resolver,
) -> Option<BoundariesDiagnostic> {
    let package_name = get_package_name(import);

    if package_name.starts_with("@types/") && matches!(import_type, ImportType::Value) {
        return Some(BoundariesDiagnostic::NotTypeOnlyImport {
            path: file_path.to_owned(),
            import: import.to_string(),
            span,
            text: NamedSource::new(file_path.as_str(), file_content.clone()),
        });
    }
    let package_node = PackageNode::Workspace(PackageName::Other(package_name.to_string()));
    let folder = file_path.parent()?;
    let is_valid_dependency = dependency_locations.is_dependency(&package_node);

    if !is_valid_dependency
        && !is_bun_builtin(import)
        && !is_vscode_module(import)
        && !matches!(
            resolver.resolve(folder, import),
            Err(ResolveError::Builtin { .. })
        )
    {
        // Check the @types package
        let types_package_node =
            PackageNode::Workspace(PackageName::Other(format!("@types/{}", package_name)));
        let is_types_dependency = dependency_locations.is_dependency(&types_package_node);

        if is_types_dependency {
            return match import_type {
                ImportType::Type => None,
                ImportType::Value => Some(BoundariesDiagnostic::NotTypeOnlyImport {
                    path: file_path.to_owned(),
                    import: import.to_string(),
                    span,
                    text: NamedSource::new(file_path.as_str(), file_content.clone()),
                }),
            };
        }

        return Some(BoundariesDiagnostic::PackageNotFound {
            path: file_path.to_owned(),
            name: package_node.to_string(),
            help: None,
            span,
            text: NamedSource::new(file_path.as_str(), file_content.clone()),
        });
    }

    None
}

#[cfg(test)]
mod test {
    use std::{collections::BTreeMap, sync::LazyLock};

    use test_case::test_case;
    use turbo_trace::Tracer;
    use turborepo_repository::{
        external_resolution::ExternalDeclaration, package_json::PackageJson,
    };

    use super::*;
    use crate::BoundariesResult;

    static NO_INTERNAL_DEPENDENCIES: LazyLock<HashSet<&'static PackageNode>> =
        LazyLock::new(HashSet::new);
    static NO_IMPLICIT_DEPENDENCIES: LazyLock<HashMap<String, Spanned<()>>> =
        LazyLock::new(HashMap::new);
    static NO_WORKSPACE_PACKAGES: LazyLock<WorkspacePackageDirectories> =
        LazyLock::new(WorkspacePackageDirectories::default);

    /// Dependency locations for a package that declares no dependencies and
    /// lives in a workspace with no other known packages.
    fn no_dependencies(package: &PackageName) -> DependencyLocations<'_> {
        DependencyLocations {
            package,
            internal_dependencies: &NO_INTERNAL_DEPENDENCIES,
            external_declarations: PackageExternalDeclarations::new(&[], package.as_str()),
            implicit_dependencies: &NO_IMPLICIT_DEPENDENCIES,
            global_implicit_dependencies: &NO_IMPLICIT_DEPENDENCIES,
            workspace_packages: &NO_WORKSPACE_PACKAGES,
        }
    }

    fn declarations(package_json: &PackageJson) -> Vec<ExternalDeclaration> {
        package_json
            .dependencies_with_kind()
            .map(|(name, specifier, kind)| {
                ExternalDeclaration::new("my-app", name, name, specifier, kind)
            })
            .collect()
    }

    #[test]
    fn declaration_projection_accepts_alias_targets_and_all_dependency_kinds() {
        let package = PackageName::from("my-app");
        let declarations = vec![
            ExternalDeclaration::new(
                "my-app",
                "alias",
                "target",
                "npm:target@1",
                turborepo_repository::relationships::DependencyKind::Production,
            ),
            ExternalDeclaration::new(
                "my-app",
                "optional",
                "optional",
                "1",
                turborepo_repository::relationships::DependencyKind::Optional,
            ),
            ExternalDeclaration::new(
                "my-app",
                "peer",
                "peer",
                "1",
                turborepo_repository::relationships::DependencyKind::Peer { optional: false },
            ),
        ];
        let locations = DependencyLocations {
            package: &package,
            internal_dependencies: &HashSet::new(),
            external_declarations: PackageExternalDeclarations::new(&declarations, "my-app"),
            implicit_dependencies: &HashMap::new(),
            global_implicit_dependencies: &HashMap::new(),
            workspace_packages: &WorkspacePackageDirectories::default(),
        };

        for dependency in ["alias", "target", "optional", "peer"] {
            assert!(
                locations.is_dependency(&PackageNode::Workspace(PackageName::from(dependency)))
            );
        }
    }

    #[test_case("bun", true ; "bun bare import")]
    #[test_case("bun:test", true ; "bun test module")]
    #[test_case("bun:sqlite", true ; "bun sqlite module")]
    #[test_case("bun:ffi", true ; "bun ffi module")]
    #[test_case("bun:jsc", true ; "bun jsc module")]
    #[test_case("bunny", false ; "package starting with bun")]
    #[test_case("bun-framework", false ; "package with bun prefix")]
    #[test_case("@types/bun", false ; "types for bun")]
    #[test_case("react", false ; "unrelated package")]
    fn test_is_bun_builtin(import: &str, expected: bool) {
        assert_eq!(is_bun_builtin(import), expected);
    }

    #[test_case("vscode", true ; "vscode module")]
    #[test_case("vscode-languageclient", false ; "vscode prefixed package")]
    #[test_case("@types/vscode", false ; "types for vscode")]
    fn test_is_vscode_module(import: &str, expected: bool) {
        assert_eq!(is_vscode_module(import), expected);
    }

    #[test_case("", ""; "empty")]
    #[test_case("ship", "ship"; "basic")]
    #[test_case("@types/ship", "@types/ship"; "types")]
    #[test_case("@scope/ship", "@scope/ship"; "scoped")]
    #[test_case("@scope/foo/bar", "@scope/foo"; "scoped with path")]
    #[test_case("foo/bar", "foo"; "regular with path")]
    #[test_case("foo/", "foo"; "trailing slash")]
    #[test_case("foo/bar/baz", "foo"; "multiple slashes")]
    fn test_get_package_name(import: &str, expected: &str) {
        assert_eq!(get_package_name(import), expected);
    }

    fn make_tsconfig_alias_test_args(
        import: &str,
    ) -> (
        Resolver,
        PackageName,
        SourceSpan,
        Arc<str>,
        BoundariesResult,
    ) {
        let resolver = Tracer::create_resolver(None);
        let package_name = PackageName::from("test-pkg");
        let span = SourceSpan::new(0.into(), 0);
        let file_content: Arc<str> = format!("import {{ x }} from \"{import}\";").into();
        let result = BoundariesResult::default();
        (resolver, package_name, span, file_content, result)
    }

    // Package-name-shaped imports that have no matching tsconfig alias and no
    // corresponding file on disk should still return `false` so that the caller
    // can fall through to `check_package_import`.
    #[test_case("react" ; "bare package name")]
    #[test_case("lodash" ; "bare package name lodash")]
    #[test_case("@scope/package" ; "scoped package name")]
    #[test_case("@types/node" ; "types package name")]
    #[test_case("lodash/fp" ; "subpath import")]
    #[test_case("@scope/package/sub" ; "scoped subpath import")]
    #[test_case("@scope/package/deeply/nested" ; "scoped deeply nested subpath import")]
    fn tsconfig_alias_check_returns_false_for_unresolvable_package_imports(import: &str) {
        let (resolver, package_name, span, file_content, _result) =
            make_tsconfig_alias_test_args(import);
        let tmp = tempfile::tempdir().unwrap();
        let package_root = AbsoluteSystemPath::new(tmp.path().to_str().unwrap()).unwrap();
        let file_path = package_root.join_component("index.ts");
        std::fs::write(file_path.as_std_path(), file_content.as_bytes()).unwrap();

        let (resolved, diag) = check_import_as_tsconfig_path_alias(
            &resolver,
            package_root,
            span,
            &file_path,
            &file_content,
            import,
            no_dependencies(&package_name),
        )
        .unwrap();

        assert!(
            !resolved,
            "package import {import:?} with no tsconfig alias should not be resolved"
        );
        assert!(diag.is_none());
    }

    #[test_case("./foo" ; "relative current dir")]
    #[test_case("../bar" ; "relative parent dir")]
    #[test_case("./deeply/nested/module" ; "relative deeply nested")]
    fn tsconfig_alias_check_skips_relative_imports(import: &str) {
        let (resolver, package_name, span, file_content, _result) =
            make_tsconfig_alias_test_args(import);
        let tmp = tempfile::tempdir().unwrap();
        let package_root = AbsoluteSystemPath::new(tmp.path().to_str().unwrap()).unwrap();
        let file_path = package_root.join_component("index.ts");
        std::fs::write(file_path.as_std_path(), file_content.as_bytes()).unwrap();

        let (resolved, diag) = check_import_as_tsconfig_path_alias(
            &resolver,
            package_root,
            span,
            &file_path,
            &file_content,
            import,
            no_dependencies(&package_name),
        )
        .unwrap();

        assert!(
            !resolved,
            "relative import {import:?} should not be resolved as tsconfig alias"
        );
        assert!(diag.is_none());
    }

    #[test]
    fn bun_import_not_flagged_as_type_only_when_types_bun_is_dependency() {
        let tmp = tempfile::tempdir().unwrap();
        let root = AbsoluteSystemPath::new(tmp.path().to_str().unwrap()).unwrap();
        let file_path = root.join_component("index.ts");
        let file_content: Arc<str> = "import { $, which } from \"bun\";".into();
        std::fs::write(file_path.as_std_path(), file_content.as_bytes()).unwrap();

        let resolver = Tracer::create_resolver(None);
        let package_name = PackageName::from("my-app");

        // `@types/bun` is listed as a devDependency, but `bun` itself is not
        let mut dev_deps = BTreeMap::new();
        dev_deps.insert("@types/bun".to_string(), "latest".to_string());
        let package_json = PackageJson {
            dev_dependencies: Some(dev_deps),
            ..Default::default()
        };

        let internal_deps = HashSet::new();
        let implicit_deps = HashMap::new();
        let global_implicit_deps = HashMap::new();
        let declarations = declarations(&package_json);

        let dependency_locations = DependencyLocations {
            package: &package_name,
            internal_dependencies: &internal_deps,
            external_declarations: PackageExternalDeclarations::new(&declarations, "my-app"),
            implicit_dependencies: &implicit_deps,
            global_implicit_dependencies: &global_implicit_deps,
            workspace_packages: &WorkspacePackageDirectories::default(),
        };

        let span = SourceSpan::new(0.into(), file_content.len());
        let result = check_package_import(
            "bun",
            ImportType::Value,
            span,
            &file_path,
            &file_content,
            dependency_locations,
            &resolver,
        );

        assert!(
            result.is_none(),
            "import from 'bun' should not be flagged even when @types/bun is a devDependency"
        );
    }

    #[test]
    fn vscode_import_not_flagged_as_type_only_when_types_vscode_is_dependency() {
        let tmp = tempfile::tempdir().unwrap();
        let root = AbsoluteSystemPath::new(tmp.path().to_str().unwrap()).unwrap();
        let file_path = root.join_component("extension.ts");
        let file_content: Arc<str> = "import { window, commands } from \"vscode\";".into();
        std::fs::write(file_path.as_std_path(), file_content.as_bytes()).unwrap();

        let resolver = Tracer::create_resolver(None);
        let package_name = PackageName::from("my-vscode-ext");

        let mut dev_deps = BTreeMap::new();
        dev_deps.insert("@types/vscode".to_string(), "1.85.0".to_string());
        let package_json = PackageJson {
            dev_dependencies: Some(dev_deps),
            ..Default::default()
        };

        let internal_deps = HashSet::new();
        let implicit_deps = HashMap::new();
        let global_implicit_deps = HashMap::new();
        let declarations = declarations(&package_json);

        let dependency_locations = DependencyLocations {
            package: &package_name,
            internal_dependencies: &internal_deps,
            external_declarations: PackageExternalDeclarations::new(&declarations, "my-app"),
            implicit_dependencies: &implicit_deps,
            global_implicit_dependencies: &global_implicit_deps,
            workspace_packages: &WorkspacePackageDirectories::default(),
        };

        let span = SourceSpan::new(0.into(), file_content.len());
        let result = check_package_import(
            "vscode",
            ImportType::Value,
            span,
            &file_path,
            &file_content,
            dependency_locations,
            &resolver,
        );

        assert!(
            result.is_none(),
            "import from 'vscode' should not be flagged even when @types/vscode is a devDependency"
        );
    }

    #[test]
    fn types_only_package_still_flagged_for_non_bun() {
        let tmp = tempfile::tempdir().unwrap();
        let root = AbsoluteSystemPath::new(tmp.path().to_str().unwrap()).unwrap();
        let file_path = root.join_component("index.ts");
        let file_content: Arc<str> = "import { Ship } from \"ship\";".into();
        std::fs::write(file_path.as_std_path(), file_content.as_bytes()).unwrap();

        let resolver = Tracer::create_resolver(None);
        let package_name = PackageName::from("my-app");

        // Only @types/ship exists, not ship itself
        let mut dev_deps = BTreeMap::new();
        dev_deps.insert("@types/ship".to_string(), "*".to_string());
        let package_json = PackageJson {
            dev_dependencies: Some(dev_deps),
            ..Default::default()
        };

        let internal_deps = HashSet::new();
        let implicit_deps = HashMap::new();
        let global_implicit_deps = HashMap::new();
        let declarations = declarations(&package_json);

        let dependency_locations = DependencyLocations {
            package: &package_name,
            internal_dependencies: &internal_deps,
            external_declarations: PackageExternalDeclarations::new(&declarations, "my-app"),
            implicit_dependencies: &implicit_deps,
            global_implicit_dependencies: &global_implicit_deps,
            workspace_packages: &WorkspacePackageDirectories::default(),
        };

        let span = SourceSpan::new(0.into(), file_content.len());
        let result = check_package_import(
            "ship",
            ImportType::Value,
            span,
            &file_path,
            &file_content,
            dependency_locations,
            &resolver,
        );

        assert!(
            result.is_some(),
            "import from 'ship' should still be flagged when only @types/ship is a dependency"
        );
        assert!(matches!(
            result.unwrap(),
            BoundariesDiagnostic::NotTypeOnlyImport { .. }
        ));
    }

    #[test]
    fn tsconfig_alias_resolves_path_alias() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        // Create a tsconfig with a path alias
        let tsconfig = root.join("tsconfig.json");
        std::fs::write(
            &tsconfig,
            r#"{ "compilerOptions": { "paths": { "@/*": ["./*"] } } }"#,
        )
        .unwrap();

        // Create the target file the alias should resolve to
        std::fs::create_dir_all(root.join("utils")).unwrap();
        std::fs::write(root.join("utils").join("helper.ts"), "export const x = 1;").unwrap();

        // Create the source file
        let file_content: Arc<str> = "import { x } from \"@/utils/helper\";".into();
        std::fs::write(root.join("index.ts"), file_content.as_bytes()).unwrap();

        let package_root = AbsoluteSystemPath::new(root.to_str().unwrap()).unwrap();
        let tsconfig_path = AbsoluteSystemPath::new(tsconfig.to_str().unwrap()).unwrap();
        let file_path = package_root.join_component("index.ts");
        let package_name = PackageName::from("test-pkg");
        let span = SourceSpan::new(0.into(), 0);

        let resolver = Tracer::create_resolver(Some(tsconfig_path));

        let (resolved, _diag) = check_import_as_tsconfig_path_alias(
            &resolver,
            package_root,
            span,
            &file_path,
            &file_content,
            "@/utils/helper",
            no_dependencies(&package_name),
        )
        .unwrap();

        assert!(
            resolved,
            "@/utils/helper should be resolved as a tsconfig path alias"
        );
    }

    /// Regression test: an import whose specifier looks like a bare package
    /// name (e.g. `features/feature-a`) but is actually a tsconfig `paths`
    /// alias pointing to a local source file must be resolved as a tsconfig
    /// alias (returning `true`) rather than being incorrectly forwarded to
    /// `check_package_import` and flagged as an undeclared dependency.
    ///
    /// See: <https://github.com/vercel/turborepo/issues/11906>
    #[test]
    fn tsconfig_alias_resolves_package_name_shaped_path_alias() {
        let tmp = tempfile::tempdir().unwrap();
        // Canonicalize to match the resolver's symlink-resolved paths
        // (e.g. /tmp → /private/tmp on macOS). Uses dunce to avoid
        // \\?\ prefix on Windows which breaks path comparison.
        let root = dunce::canonicalize(tmp.path()).unwrap();

        // Mimic a tsconfig that maps `*` to `./src/*`, turning bare specifiers
        // like `features/feature-a` into local imports.
        let tsconfig = root.join("tsconfig.json");
        std::fs::write(
            &tsconfig,
            r#"{ "compilerOptions": { "paths": { "*": ["./src/*"] } } }"#,
        )
        .unwrap();

        // Create the target file the alias should resolve to
        std::fs::create_dir_all(root.join("src").join("features")).unwrap();
        std::fs::write(
            root.join("src").join("features").join("feature-a.ts"),
            "export const featureA = true;",
        )
        .unwrap();

        // Create the source file that imports via the alias
        let file_content: Arc<str> = "import { featureA } from \"features/feature-a\";".into();
        std::fs::write(root.join("index.ts"), file_content.as_bytes()).unwrap();

        let package_root = AbsoluteSystemPath::new(root.to_str().unwrap()).unwrap();
        let tsconfig_path = AbsoluteSystemPath::new(tsconfig.to_str().unwrap()).unwrap();
        let file_path = package_root.join_component("index.ts");
        let package_name = PackageName::from("test-pkg");
        let span = SourceSpan::new(0.into(), 0);

        let resolver = Tracer::create_resolver(Some(tsconfig_path));

        let (resolved, diag) = check_import_as_tsconfig_path_alias(
            &resolver,
            package_root,
            span,
            &file_path,
            &file_content,
            "features/feature-a",
            no_dependencies(&package_name),
        )
        .unwrap();

        assert!(
            resolved,
            "features/feature-a with a tsconfig `*` alias should be resolved as a local import"
        );
        assert!(
            diag.is_none(),
            "expected no boundary violations for a locally-aliased import"
        );
    }

    /// Regression test: a tsconfig path alias must still be resolved as a local
    /// import even when the package root contains a `package.json` file.
    ///
    /// Previously, using `resolution.package_json().is_some()` caused the check
    /// to incorrectly treat tsconfig aliases as npm packages in any real
    /// project that has a `package.json` in its root directory.
    #[test]
    fn tsconfig_alias_resolves_with_package_json_present() {
        let tmp = tempfile::tempdir().unwrap();
        // Canonicalize to match the resolver's symlink-resolved paths
        // (e.g. /tmp → /private/tmp on macOS). Uses dunce to avoid
        // \\?\ prefix on Windows which breaks path comparison.
        let root = dunce::canonicalize(tmp.path()).unwrap();

        // Create a package.json so unrs_resolver can find it during resolution
        std::fs::write(
            root.join("package.json"),
            r#"{ "name": "test-pkg", "version": "1.0.0" }"#,
        )
        .unwrap();

        let tsconfig = root.join("tsconfig.json");
        std::fs::write(
            &tsconfig,
            r#"{ "compilerOptions": { "paths": { "@/*": ["./*"] } } }"#,
        )
        .unwrap();

        std::fs::create_dir_all(root.join("utils")).unwrap();
        std::fs::write(root.join("utils").join("helper.ts"), "export const x = 1;").unwrap();

        let file_content: Arc<str> = "import { x } from \"@/utils/helper\";".into();
        std::fs::write(root.join("index.ts"), file_content.as_bytes()).unwrap();

        let package_root = AbsoluteSystemPath::new(root.to_str().unwrap()).unwrap();
        let tsconfig_path = AbsoluteSystemPath::new(tsconfig.to_str().unwrap()).unwrap();
        let file_path = package_root.join_component("index.ts");
        let package_name = PackageName::from("test-pkg");
        let span = SourceSpan::new(0.into(), 0);

        let resolver = Tracer::create_resolver(Some(tsconfig_path));

        let (resolved, diag) = check_import_as_tsconfig_path_alias(
            &resolver,
            package_root,
            span,
            &file_path,
            &file_content,
            "@/utils/helper",
            no_dependencies(&package_name),
        )
        .unwrap();

        assert!(
            resolved,
            "@/utils/helper should be resolved as a tsconfig path alias even when package.json is \
             present"
        );
        assert!(
            diag.is_none(),
            "expected no boundary violations for a locally-aliased import"
        );
    }

    /// When the resolver resolves an import to a path inside `node_modules`,
    /// the function must return `false` so the caller falls through to
    /// `check_package_import` for dependency-declaration validation.
    #[test]
    fn tsconfig_alias_check_returns_false_for_node_modules_resolution() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        // Wildcard alias that could match anything
        let tsconfig = root.join("tsconfig.json");
        std::fs::write(
            &tsconfig,
            r#"{ "compilerOptions": { "paths": { "*": ["./src/*"] } } }"#,
        )
        .unwrap();

        // Create a real node_modules package so the resolver can find it
        std::fs::create_dir_all(root.join("node_modules").join("some-pkg")).unwrap();
        std::fs::write(
            root.join("node_modules").join("some-pkg").join("index.js"),
            "module.exports = {};",
        )
        .unwrap();
        std::fs::write(
            root.join("node_modules")
                .join("some-pkg")
                .join("package.json"),
            r#"{ "name": "some-pkg", "main": "index.js" }"#,
        )
        .unwrap();

        let file_content: Arc<str> = r#"import { x } from "some-pkg";"#.into();
        std::fs::write(root.join("index.ts"), file_content.as_bytes()).unwrap();

        let package_root = AbsoluteSystemPath::new(root.to_str().unwrap()).unwrap();
        let tsconfig_path = AbsoluteSystemPath::new(tsconfig.to_str().unwrap()).unwrap();
        let file_path = package_root.join_component("index.ts");
        let package_name = PackageName::from("test-pkg");
        let span = SourceSpan::new(0.into(), 0);

        let resolver = Tracer::create_resolver(Some(tsconfig_path));

        let (resolved, diag) = check_import_as_tsconfig_path_alias(
            &resolver,
            package_root,
            span,
            &file_path,
            &file_content,
            "some-pkg",
            no_dependencies(&package_name),
        )
        .unwrap();

        assert!(
            !resolved,
            "import resolving to node_modules must not be treated as a tsconfig alias"
        );
        assert!(diag.is_none());
    }

    /// A tsconfig alias that resolves to a file outside the package root should
    /// still be treated as a resolved alias (returns `true`), but produce an
    /// `ImportLeavesPackage` diagnostic.
    #[test]
    fn tsconfig_alias_flags_boundary_violation_for_out_of_package_resolution() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        // Shared file outside the package root
        let shared_dir = root.join("shared");
        std::fs::create_dir_all(&shared_dir).unwrap();
        std::fs::write(shared_dir.join("utils.ts"), "export const x = 1;").unwrap();

        // Package directory with a tsconfig alias pointing outside
        let pkg_dir = root.join("packages").join("my-app");
        std::fs::create_dir_all(&pkg_dir).unwrap();

        let tsconfig = pkg_dir.join("tsconfig.json");
        std::fs::write(
            &tsconfig,
            r#"{ "compilerOptions": { "paths": { "@shared/*": ["../../shared/*"] } } }"#,
        )
        .unwrap();

        let file_content: Arc<str> = r#"import { x } from "@shared/utils";"#.into();
        std::fs::write(pkg_dir.join("index.ts"), file_content.as_bytes()).unwrap();

        let package_root = AbsoluteSystemPath::new(pkg_dir.to_str().unwrap()).unwrap();
        let tsconfig_path = AbsoluteSystemPath::new(tsconfig.to_str().unwrap()).unwrap();
        let file_path = package_root.join_component("index.ts");
        let package_name = PackageName::from("my-app");
        let span = SourceSpan::new(0.into(), 0);

        let resolver = Tracer::create_resolver(Some(tsconfig_path));

        let (resolved, diag) = check_import_as_tsconfig_path_alias(
            &resolver,
            package_root,
            span,
            &file_path,
            &file_content,
            "@shared/utils",
            no_dependencies(&package_name),
        )
        .unwrap();

        assert!(
            resolved,
            "@shared/utils should be resolved as a tsconfig path alias"
        );
        assert!(
            diag.is_some(),
            "expected an ImportLeavesPackage diagnostic for an out-of-package alias"
        );
    }

    /// A workspace with two packages, `web` (`packages/web`) and `@repo/ui`
    /// (`packages/ui`), plus a `shared` directory that belongs to no package.
    /// `web`'s tsconfig aliases into both.
    struct AliasWorkspace {
        _tmp: tempfile::TempDir,
        web_root: AbsoluteSystemPathBuf,
        file_path: AbsoluteSystemPathBuf,
        file_content: Arc<str>,
        resolver: Resolver,
        workspace_packages: WorkspacePackageDirectories,
    }

    impl AliasWorkspace {
        fn new(import: &str) -> Self {
            let tmp = tempfile::tempdir().expect("create temp workspace");
            // Canonicalize to match the resolver's symlink-resolved paths.
            let root = dunce::canonicalize(tmp.path()).expect("canonicalize temp workspace");
            let root = AbsoluteSystemPathBuf::try_from(root).expect("absolute utf-8 root");

            let ui_root = root.join_components(&["packages", "ui"]);
            ui_root
                .join_component("src")
                .create_dir_all()
                .expect("create ui src");
            ui_root
                .join_component("package.json")
                .create_with_contents(r#"{ "name": "@repo/ui" }"#)
                .expect("write ui package.json");
            ui_root
                .join_components(&["src", "button.ts"])
                .create_with_contents("export const Button = 1;")
                .expect("write ui source");

            let shared = root.join_component("shared");
            shared.create_dir_all().expect("create shared dir");
            shared
                .join_component("utils.ts")
                .create_with_contents("export const x = 1;")
                .expect("write shared source");

            let web_root = root.join_components(&["packages", "web"]);
            web_root.create_dir_all().expect("create web dir");
            web_root
                .join_component("package.json")
                .create_with_contents(r#"{ "name": "web" }"#)
                .expect("write web package.json");
            let tsconfig = web_root.join_component("tsconfig.json");
            tsconfig
                .create_with_contents(
                    r#"{ "compilerOptions": { "paths": {
                        "@/*": ["./*"],
                        "@ui/*": ["../ui/src/*"],
                        "@repo/ui/*": ["../ui/src/*"],
                        "@shared/*": ["../../shared/*"]
                    } } }"#,
                )
                .expect("write web tsconfig");

            let file_content: Arc<str> = format!(r#"import {{ x }} from "{import}";"#).into();
            let file_path = web_root.join_component("index.ts");
            file_path
                .create_with_contents(file_content.as_bytes())
                .expect("write web source");

            let workspace_packages = WorkspacePackageDirectories::new([
                (web_root.clone(), PackageName::from("web")),
                (ui_root, PackageName::from("@repo/ui")),
            ]);

            Self {
                _tmp: tmp,
                resolver: Tracer::create_resolver(Some(&tsconfig)),
                web_root,
                file_path,
                file_content,
                workspace_packages,
            }
        }

        fn check(
            &self,
            import: &str,
            internal_dependencies: &HashSet<&PackageNode>,
        ) -> (bool, Option<BoundariesDiagnostic>) {
            let package_name = PackageName::from("web");
            let dependency_locations = DependencyLocations {
                internal_dependencies,
                workspace_packages: &self.workspace_packages,
                ..no_dependencies(&package_name)
            };
            check_import_as_tsconfig_path_alias(
                &self.resolver,
                &self.web_root,
                SourceSpan::new(0.into(), 0),
                &self.file_path,
                &self.file_content,
                import,
                dependency_locations,
            )
            .expect("check tsconfig path alias")
        }
    }

    /// An alias that resolves into a declared workspace dependency is an
    /// import of that package and is allowed.
    #[test]
    fn tsconfig_alias_into_declared_workspace_package_is_allowed() {
        let workspace = AliasWorkspace::new("@ui/button");
        let ui = PackageNode::Workspace(PackageName::from("@repo/ui"));
        let internal_dependencies = HashSet::from([&ui]);

        let (resolved, diag) = workspace.check("@ui/button", &internal_dependencies);

        assert!(
            resolved,
            "@ui/button should resolve through the tsconfig alias"
        );
        assert!(
            diag.is_none(),
            "alias into a declared dependency should not be flagged, got {diag:?}"
        );
    }

    /// An alias that resolves into a workspace package that isn't a dependency
    /// is reported as an undeclared import of that package, not as leaving
    /// the package.
    #[test]
    fn tsconfig_alias_into_undeclared_workspace_package_names_the_package() {
        let workspace = AliasWorkspace::new("@ui/button");

        let (resolved, diag) = workspace.check("@ui/button", &HashSet::new());

        assert!(
            resolved,
            "@ui/button should resolve through the tsconfig alias"
        );
        let Some(BoundariesDiagnostic::PackageNotFound { name, help, .. }) = diag else {
            panic!("expected PackageNotFound, got {diag:?}");
        };
        assert_eq!(name, "@repo/ui");
        assert_eq!(
            help.as_deref(),
            Some("`@ui/button` is a tsconfig path alias that resolves into package `@repo/ui`")
        );
    }

    /// An alias that resolves outside of every workspace package still leaves
    /// the package.
    #[test]
    fn tsconfig_alias_outside_all_workspace_packages_leaves_the_package() {
        let workspace = AliasWorkspace::new("@shared/utils");

        let (resolved, diag) = workspace.check("@shared/utils", &HashSet::new());

        assert!(
            resolved,
            "@shared/utils should resolve through the tsconfig alias"
        );
        assert!(
            matches!(diag, Some(BoundariesDiagnostic::ImportLeavesPackage { .. })),
            "expected ImportLeavesPackage, got {diag:?}"
        );
    }

    /// A specifier that walks up out of the package with `..` is reaching into
    /// another package by path, even if it starts with an alias prefix.
    #[test]
    fn tsconfig_alias_with_parent_segments_into_workspace_package_leaves_the_package() {
        let import = "@/../ui/src/button";
        let workspace = AliasWorkspace::new(import);
        let ui = PackageNode::Workspace(PackageName::from("@repo/ui"));
        let internal_dependencies = HashSet::from([&ui]);

        let (resolved, diag) = workspace.check(import, &internal_dependencies);

        assert!(
            resolved,
            "{import} should resolve through the tsconfig alias"
        );
        assert!(
            matches!(diag, Some(BoundariesDiagnostic::ImportLeavesPackage { .. })),
            "expected ImportLeavesPackage, got {diag:?}"
        );
    }

    /// An alias that mirrors the target package's own name is left to
    /// `check_package_import`, which validates it as a package import.
    #[test]
    fn tsconfig_alias_matching_target_package_name_falls_through() {
        let workspace = AliasWorkspace::new("@repo/ui/button");

        let (resolved, diag) = workspace.check("@repo/ui/button", &HashSet::new());

        assert!(!resolved, "package-named alias should fall through");
        assert!(diag.is_none());
    }

    /// The resolver returns symlink-resolved paths, so a package whose
    /// directory is a symlink must still be found by its real location.
    #[cfg(unix)]
    #[test]
    fn workspace_package_lookup_resolves_symlinked_package_directories() {
        let tmp = tempfile::tempdir().expect("create temp workspace");
        let root = dunce::canonicalize(tmp.path()).expect("canonicalize temp workspace");
        let root = AbsoluteSystemPathBuf::try_from(root).expect("absolute utf-8 root");
        let real_ui = root.join_components(&["vendor", "ui"]);
        real_ui
            .join_component("src")
            .create_dir_all()
            .expect("create real ui dir");
        let packages = root.join_component("packages");
        packages.create_dir_all().expect("create packages dir");
        let linked_ui = packages.join_component("ui");
        std::os::unix::fs::symlink(real_ui.as_std_path(), linked_ui.as_std_path())
            .expect("symlink ui package");

        let lookup = WorkspacePackageDirectories::new([(linked_ui, PackageName::from("ui"))]);

        assert_eq!(
            lookup.package_containing(&real_ui.join_components(&["src", "button.ts"])),
            Some(&PackageName::from("ui"))
        );
    }

    #[test]
    fn workspace_package_lookup_prefers_deepest_package() {
        let tmp = tempfile::tempdir().expect("create temp workspace");
        let root = AbsoluteSystemPath::new(tmp.path().to_str().expect("utf-8 temp path"))
            .expect("absolute temp path");
        let outer = root.join_components(&["packages", "outer"]);
        let inner = outer.join_components(&["nested", "inner"]);
        let lookup = WorkspacePackageDirectories::new([
            (outer.clone(), PackageName::from("outer")),
            (inner.clone(), PackageName::from("inner")),
        ]);

        assert_eq!(
            lookup.package_containing(&inner.join_components(&["src", "a.ts"])),
            Some(&PackageName::from("inner"))
        );
        assert_eq!(
            lookup.package_containing(&outer.join_components(&["src", "a.ts"])),
            Some(&PackageName::from("outer"))
        );
        assert_eq!(
            lookup.package_containing(&inner),
            Some(&PackageName::from("inner"))
        );
        assert_eq!(
            lookup.package_containing(&root.join_components(&["shared", "a.ts"])),
            None
        );
        // A sibling whose name shares a prefix with a package is not inside it.
        assert_eq!(
            lookup.package_containing(&root.join_components(&["packages", "outer-two", "a.ts"])),
            None
        );
    }

    #[test_case("test/*", "./test/*", "test/factories/item.factory.ts", "test/factories/item.factory.js" ; "js to ts")]
    #[test_case("@/*", "./src/*", "src/helper.mts", "@/helper.mjs" ; "mjs to mts")]
    #[test_case("@/*", "./src/*", "src/helper.cts", "@/helper.cjs" ; "cjs to cts")]
    fn tsconfig_alias_resolves_typescript_extension_aliases(
        alias_key: &str,
        alias_target: &str,
        target_file: &str,
        import: &str,
    ) {
        let tmp = tempfile::tempdir().expect("create temp project");
        let root = dunce::canonicalize(tmp.path()).expect("canonicalize temp project");

        let tsconfig = root.join("tsconfig.json");
        let tsconfig_content = format!(
            r#"{{ "compilerOptions": {{ "module": "nodenext", "moduleResolution": "nodenext", "paths": {{ "{alias_key}": ["{alias_target}"] }} }} }}"#
        );
        std::fs::write(&tsconfig, tsconfig_content).expect("write tsconfig");

        let target_path = root.join(target_file);
        std::fs::create_dir_all(target_path.parent().expect("target file has parent"))
            .expect("create target directory");
        std::fs::write(&target_path, "export const x = 1;").expect("write target file");

        let file_content: Arc<str> = format!(r#"import {{ x }} from "{import}";"#).into();
        std::fs::write(root.join("index.ts"), file_content.as_bytes()).expect("write source file");

        let package_root = AbsoluteSystemPath::new(root.to_str().expect("root path is utf-8"))
            .expect("root path is absolute");
        let tsconfig_path =
            AbsoluteSystemPath::new(tsconfig.to_str().expect("tsconfig path is utf-8"))
                .expect("tsconfig path is absolute");
        let file_path = package_root.join_component("index.ts");
        let package_name = PackageName::from("test-pkg");
        let span = SourceSpan::new(0.into(), 0);

        let resolver = Tracer::create_resolver(Some(tsconfig_path));

        let (resolved, diag) = check_import_as_tsconfig_path_alias(
            &resolver,
            package_root,
            span,
            &file_path,
            &file_content,
            import,
            no_dependencies(&package_name),
        )
        .expect("check tsconfig path alias");

        assert!(
            resolved,
            "{import} should resolve through the tsconfig alias"
        );
        assert!(diag.is_none());
    }

    /// Relative imports that resolve into `node_modules` (e.g. SvelteKit's
    /// generated `../../../node_modules/@sveltejs/kit/...` imports) must not
    /// be flagged as leaving the package.
    #[test]
    fn file_import_into_node_modules_is_not_a_violation() {
        let tmp = tempfile::tempdir().expect("create temp project");
        let root = dunce::canonicalize(tmp.path()).expect("canonicalize temp project");

        let repo_root = AbsoluteSystemPath::new(root.to_str().expect("root path is utf-8"))
            .expect("root path is absolute");
        let package_root = repo_root.join_components(&["apps", "web"]);
        let file_path =
            package_root.join_components(&[".svelte-kit", "generated", "nodes", "1.js"]);
        let resolved_import_path = repo_root.join_components(&[
            "node_modules",
            "@sveltejs",
            "kit",
            "src",
            "runtime",
            "components",
            "error.svelte",
        ]);

        let diag = check_file_import(
            &file_path,
            &package_root,
            &PackageName::from("web"),
            "../../../../node_modules/@sveltejs/kit/src/runtime/components/error.svelte",
            &resolved_import_path,
            SourceSpan::new(0.into(), 0),
            &Arc::from(""),
        )
        .expect("check file import");

        assert!(
            diag.is_none(),
            "imports resolving into node_modules should not be flagged"
        );
    }

    /// Relative imports that resolve outside the package (and not into
    /// `node_modules`) must still be flagged as leaving the package.
    #[test]
    fn file_import_outside_package_is_still_a_violation() {
        let tmp = tempfile::tempdir().expect("create temp project");
        let root = dunce::canonicalize(tmp.path()).expect("canonicalize temp project");

        let repo_root = AbsoluteSystemPath::new(root.to_str().expect("root path is utf-8"))
            .expect("root path is absolute");
        let package_root = repo_root.join_components(&["apps", "web"]);
        let file_path = package_root.join_component("index.ts");
        let resolved_import_path = repo_root.join_components(&["apps", "docs", "utils.ts"]);

        let diag = check_file_import(
            &file_path,
            &package_root,
            &PackageName::from("web"),
            "../docs/utils",
            &resolved_import_path,
            SourceSpan::new(0.into(), 0),
            &Arc::from(""),
        )
        .expect("check file import");

        assert!(
            matches!(diag, Some(BoundariesDiagnostic::ImportLeavesPackage { .. })),
            "imports resolving outside the package should still be flagged"
        );
    }

    /// Every diagnostic produced for a file must share the same source
    /// allocation, so retaining N errors costs one copy of the file, not N.
    #[test]
    fn diagnostics_share_file_source() {
        let tmp = tempfile::tempdir().unwrap();
        let root = AbsoluteSystemPath::new(tmp.path().to_str().unwrap()).unwrap();
        let file_path = root.join_component("index.ts");
        let file_content: Arc<str> =
            "import { a } from \"undeclared-a\"; import { b } from \"undeclared-b\";".into();

        let resolver = Tracer::create_resolver(None);
        let package_name = PackageName::from("my-app");
        let package_json = PackageJson::default();
        let internal_deps = HashSet::new();
        let implicit_deps = HashMap::new();
        let global_implicit_deps = HashMap::new();
        let declarations = declarations(&package_json);
        let dependency_locations = DependencyLocations {
            package: &package_name,
            internal_dependencies: &internal_deps,
            external_declarations: PackageExternalDeclarations::new(&declarations, "my-app"),
            implicit_dependencies: &implicit_deps,
            global_implicit_dependencies: &global_implicit_deps,
            workspace_packages: &WorkspacePackageDirectories::default(),
        };

        let mut sources = Vec::new();
        for import in ["undeclared-a", "undeclared-b"] {
            if let Some(diag) = check_package_import(
                import,
                ImportType::Value,
                SourceSpan::new(0.into(), 0),
                &file_path,
                &file_content,
                dependency_locations,
                &resolver,
            ) {
                let BoundariesDiagnostic::PackageNotFound { text, .. } = diag else {
                    panic!("expected PackageNotFound");
                };
                sources.push(text);
            }
        }

        assert_eq!(sources.len(), 2);
        assert!(Arc::ptr_eq(sources[0].inner(), sources[1].inner()));
    }
}
