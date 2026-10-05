use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use miette::NamedSource;
use tracing::info_span;
use turborepo_errors::Spanned;
use turborepo_repository::{
    external_resolution::ExternalDeclaration,
    package_graph::{PackageName, PackageNode},
    relationships::DependencyKind,
};
use wax::{Glob, Program};

use crate::{
    BoundariesContext, BoundariesDiagnostic, Error, PackageGraphProvider, SecondaryDiagnostic,
    TurboJsonProvider,
    config::{Permissions, Rule},
};

pub type ProcessedRulesMap = HashMap<String, ProcessedRule>;

/// Which side of a package's relationships a set of permissions applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RuleRelation {
    Dependencies,
    Dependents,
}

pub struct ProcessedRule {
    span: Spanned<()>,
    pub dependencies: Option<ProcessedPermissions>,
    pub dependents: Option<ProcessedPermissions>,
}

impl ProcessedRule {
    /// Processes a tag rule, pushing any configuration errors (e.g. invalid
    /// `denyPackages` patterns) into `diagnostics`.
    pub(crate) fn new(rule: Spanned<Rule>, diagnostics: &mut Vec<BoundariesDiagnostic>) -> Self {
        let (rule, span) = rule.split();
        Self {
            span,
            dependencies: rule.dependencies.map(|dependencies| {
                ProcessedPermissions::new(
                    dependencies.into_inner(),
                    RuleRelation::Dependencies,
                    diagnostics,
                )
            }),
            dependents: rule.dependents.map(|dependents| {
                ProcessedPermissions::new(
                    dependents.into_inner(),
                    RuleRelation::Dependents,
                    diagnostics,
                )
            }),
        }
    }
}

pub struct ProcessedPermissions {
    pub allow: Option<Spanned<HashSet<String>>>,
    pub deny: Option<Spanned<HashSet<String>>>,
    pub deny_packages: Option<DeniedPackages>,
}

impl ProcessedPermissions {
    /// Processes permissions for the given relation, pushing any configuration
    /// errors into `diagnostics`. Invalid `denyPackages` entries are dropped
    /// after being reported so the remaining rules still apply.
    pub(crate) fn new(
        permissions: Permissions,
        relation: RuleRelation,
        diagnostics: &mut Vec<BoundariesDiagnostic>,
    ) -> Self {
        let deny_packages = permissions
            .deny_packages
            .and_then(|deny_packages| match relation {
                RuleRelation::Dependencies => Some(DeniedPackages::new(deny_packages, diagnostics)),
                RuleRelation::Dependents => {
                    let (span, text) = {
                        let (span, text) = deny_packages.span_and_text("turbo.json");
                        (span, crate::into_shared_source(text))
                    };
                    diagnostics.push(BoundariesDiagnostic::DenyPackagesInDependents { span, text });
                    None
                }
            });

        Self {
            allow: permissions
                .allow
                .map(|allow| allow.map(|allow| allow.into_iter().flatten().collect())),
            deny: permissions
                .deny
                .map(|deny| deny.map(|deny| deny.into_iter().flatten().collect())),
            deny_packages,
        }
    }
}

/// Precompiled `denyPackages` patterns.
pub struct DeniedPackages {
    patterns: Vec<DeniedPackagePattern>,
}

struct DeniedPackagePattern {
    pattern: Spanned<String>,
    /// `None` for literal package names, which are compared directly.
    glob: Option<Glob<'static>>,
}

impl DeniedPackagePattern {
    fn matches(&self, package_name: &str) -> bool {
        match &self.glob {
            Some(glob) => glob.is_match(package_name),
            None => self.pattern.as_inner() == package_name,
        }
    }
}

/// Characters that can appear in an npm package name. A pattern made only of
/// these is a literal name and doesn't need a glob matcher.
fn is_literal_package_name(pattern: &str) -> bool {
    pattern.bytes().all(|byte| {
        byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'@' | b'/')
    })
}

impl DeniedPackages {
    fn new(
        patterns: Spanned<Vec<Spanned<String>>>,
        diagnostics: &mut Vec<BoundariesDiagnostic>,
    ) -> Self {
        let patterns = patterns
            .into_inner()
            .into_iter()
            .filter_map(|pattern| {
                if is_literal_package_name(pattern.as_inner()) {
                    return Some(DeniedPackagePattern {
                        pattern,
                        glob: None,
                    });
                }
                match Glob::new(pattern.as_inner()) {
                    Ok(glob) => {
                        let glob = glob.into_owned();
                        Some(DeniedPackagePattern {
                            pattern,
                            glob: Some(glob),
                        })
                    }
                    Err(err) => {
                        let (span, text) = {
                            let (span, text) = pattern.span_and_text("turbo.json");
                            (span, crate::into_shared_source(text))
                        };
                        diagnostics.push(BoundariesDiagnostic::InvalidDenyPackagesPattern {
                            pattern: pattern.as_inner().clone(),
                            reason: err.to_string(),
                            span,
                            text,
                        });
                        None
                    }
                }
            })
            .collect();
        Self { patterns }
    }

    /// Returns the first pattern that matches the declaration, either by the
    /// name it is declared under in `package.json` or, for npm aliases (e.g.
    /// `"db": "npm:pg@^8"`), by the real package name.
    fn find_match(
        &self,
        declaration_name: &str,
        aliased_package: Option<&str>,
    ) -> Option<&DeniedPackagePattern> {
        self.patterns.iter().find(|pattern| {
            pattern.matches(declaration_name)
                || aliased_package.is_some_and(|aliased| pattern.matches(aliased))
        })
    }
}

/// Returns the real package name of an npm alias specifier, e.g. `pg` for
/// `npm:pg@^8` or `@scope/pkg` for `npm:@scope/pkg@1.0.0`. Returns `None` for
/// specifiers that aren't aliases, including plain npm ranges like
/// `npm:^1.0.0`.
fn npm_alias_target(specifier: &str) -> Option<&str> {
    let rest = specifier.strip_prefix("npm:")?;
    let version_separator = match rest.strip_prefix('@') {
        Some(scoped) => scoped.find('@').map(|index| index + 1),
        None => rest.find('@'),
    };
    let name = version_separator.map_or(rest, |index| &rest[..index]);
    crate::is_valid_package_name(name).then_some(name)
}

/// The real package an external declaration refers to, if it differs from the
/// name it is declared under.
fn aliased_package(declaration: &ExternalDeclaration) -> Option<&str> {
    let declaration_name = declaration.declaration_name();
    npm_alias_target(declaration.specifier())
        .or(Some(declaration.package_name()))
        .filter(|name| *name != declaration_name)
}

fn dependency_kind_field(kind: DependencyKind) -> &'static str {
    match kind {
        DependencyKind::Production => "dependencies",
        DependencyKind::Development => "devDependencies",
        DependencyKind::Optional => "optionalDependencies",
        DependencyKind::Peer { .. } => "peerDependencies",
    }
}

/// Checks the external dependencies declared by `pkg` and by each of its
/// transitive workspace dependencies against `denyPackages`.
fn check_denied_packages<G, T>(
    ctx: &BoundariesContext<'_, G, T>,
    diagnostics: &mut Vec<BoundariesDiagnostic>,
    denied_packages: &DeniedPackages,
    pkg: &PackageNode,
    package_name_source: Option<&Spanned<()>>,
    dependencies: &[&PackageNode],
) where
    G: PackageGraphProvider,
    T: TurboJsonProvider,
{
    if denied_packages.patterns.is_empty() {
        return;
    }
    let source_package_name = pkg.as_package_name();

    // Check the package itself first, then its workspace dependencies in a
    // stable order so output is deterministic.
    let mut workspace_dependencies: Vec<&PackageName> = dependencies
        .iter()
        .filter_map(|dependency| match dependency {
            PackageNode::Workspace(name) => Some(name),
            PackageNode::Root => None,
        })
        .collect();
    workspace_dependencies.sort();

    for declared_by in std::iter::once(source_package_name).chain(workspace_dependencies) {
        for declaration in ctx.pkg_dep_graph.external_declarations(declared_by).iter() {
            let declaration_name = declaration.declaration_name();
            let aliased_package = aliased_package(declaration);
            let Some(pattern) = denied_packages.find_match(declaration_name, aliased_package)
            else {
                continue;
            };

            let (span, text) = package_name_source
                .map(|name| name.span_and_text("package.json"))
                .map(|(span, text)| (span, crate::into_shared_source(text)))
                .unwrap_or_else(|| (None, NamedSource::new("package.json", Arc::from(""))));
            let (pattern_span, pattern_text) = {
                let (span, text) = pattern.pattern.span_and_text("turbo.json");
                (span, crate::into_shared_source(text))
            };

            let mut help = Vec::new();
            if declared_by != source_package_name {
                help.push(format!(
                    "`{source_package_name}` depends on `{declared_by}`, which declares \
                     `{declaration_name}`"
                ));
            }
            if let Some(aliased_package) = aliased_package {
                help.push(format!(
                    "`{declaration_name}` is an alias of `{aliased_package}`"
                ));
            }

            diagnostics.push(BoundariesDiagnostic::DeniedPackage {
                source_package_name: source_package_name.clone(),
                declared_by: declared_by.clone(),
                dependency: declaration_name.to_string(),
                dependency_kind: dependency_kind_field(declaration.kind()),
                pattern: pattern.pattern.as_inner().clone(),
                span,
                text,
                help: (!help.is_empty()).then(|| help.join("\n")),
                secondary: [SecondaryDiagnostic::DeniedPackagePattern {
                    pattern: pattern.pattern.as_inner().clone(),
                    span: pattern_span,
                    text: pattern_text,
                }],
            });
        }
    }
}

/// Loops through the tags of a package that is related to `package_name`
/// (i.e. either a dependency or a dependent) and checks if the tag is
/// allowed or denied by the rules in `allow_list` and `deny_list`.
fn validate_relation<G, T>(
    _ctx: &BoundariesContext<'_, G, T>,
    package_name: &PackageName,
    package_name_source: Option<&Spanned<()>>,
    relation_package_name: &PackageName,
    tags: Option<&Spanned<Vec<Spanned<String>>>>,
    allow_list: Option<&Spanned<HashSet<String>>>,
    deny_list: Option<&Spanned<HashSet<String>>>,
) -> Result<Option<BoundariesDiagnostic>, Error>
where
    G: PackageGraphProvider,
    T: TurboJsonProvider,
{
    // We allow "punning" the package name as a tag, so if the allow list contains
    // the package name, then we have a tag in the allow list
    // Likewise, if the allow list is empty, then we vacuously have a tag in the
    // allow list
    let mut has_tag_in_allowlist =
        allow_list.is_none_or(|allow_list| allow_list.contains(relation_package_name.as_str()));
    let tags_span = tags.map(|tags| tags.to(())).unwrap_or_default();
    if let Some(deny_list) = deny_list
        && deny_list.contains(relation_package_name.as_str())
    {
        let (span, text) = package_name_source
            .map(|name| name.span_and_text("package.json"))
            .map(|(span, text)| (span, crate::into_shared_source(text)))
            .unwrap_or_else(|| (None, NamedSource::new("package.json", Arc::from(""))));
        let deny_list_spanned = deny_list.to(());
        let (deny_list_span, deny_list_text) = {
            let (span, text) = deny_list_spanned.span_and_text("turbo.json");
            (span, crate::into_shared_source(text))
        };

        return Ok(Some(BoundariesDiagnostic::DeniedTag {
            source_package_name: package_name.clone(),
            package_name: relation_package_name.clone(),
            tag: relation_package_name.to_string(),
            span,
            text,
            secondary: [SecondaryDiagnostic::Denylist {
                span: deny_list_span,
                text: deny_list_text,
            }],
        }));
    }

    for tag in tags.into_iter().flatten().flatten() {
        if let Some(allow_list) = allow_list
            && allow_list.contains(tag.as_inner())
        {
            has_tag_in_allowlist = true;
        }

        if let Some(deny_list) = deny_list
            && deny_list.contains(tag.as_inner())
        {
            let (span, text) = {
                let (span, text) = tag.span_and_text("turbo.json");
                (span, crate::into_shared_source(text))
            };
            let deny_list_spanned = deny_list.to(());
            let (deny_list_span, deny_list_text) = {
                let (span, text) = deny_list_spanned.span_and_text("turbo.json");
                (span, crate::into_shared_source(text))
            };

            return Ok(Some(BoundariesDiagnostic::DeniedTag {
                source_package_name: package_name.clone(),
                package_name: relation_package_name.clone(),
                tag: tag.as_inner().to_string(),
                span,
                text,
                secondary: [SecondaryDiagnostic::Denylist {
                    span: deny_list_span,
                    text: deny_list_text,
                }],
            }));
        }
    }

    if !has_tag_in_allowlist {
        let (span, text) = {
            let (span, text) = tags_span.span_and_text("turbo.json");
            (span, crate::into_shared_source(text))
        };
        let help = span.is_none().then(|| {
            format!("`{relation_package_name}` doesn't any tags defined in its `turbo.json` file")
        });

        let allow_list_spanned = allow_list
            .map(|allow_list| allow_list.to(()))
            .unwrap_or_default();
        let (allow_list_span, allow_list_text) = {
            let (span, text) = allow_list_spanned.span_and_text("turbo.json");
            (span, crate::into_shared_source(text))
        };

        return Ok(Some(BoundariesDiagnostic::NoTagInAllowlist {
            source_package_name: package_name.clone(),
            package_name: relation_package_name.clone(),
            help,
            span,
            text,
            secondary: [SecondaryDiagnostic::Allowlist {
                span: allow_list_span,
                text: allow_list_text,
            }],
        }));
    }

    Ok(None)
}

struct CachedRelations<'a, 'b> {
    dependencies: &'a [&'b PackageNode],
    ancestors: &'a [&'b PackageNode],
}

/// Check tag rules against precomputed dependency/ancestor sets.
///
/// Unlike the previous version that called `ctx.pkg_dep_graph.dependencies()`
/// per invocation (triggering a full DFS each time), this takes the already-
/// computed sets to avoid redundant graph traversals when multiple tags share
/// the same package.
fn check_tag_with_cache<G, T>(
    ctx: &BoundariesContext<'_, G, T>,
    diagnostics: &mut Vec<BoundariesDiagnostic>,
    dependencies: Option<&ProcessedPermissions>,
    dependents: Option<&ProcessedPermissions>,
    pkg: &PackageNode,
    package_name_source: Option<&Spanned<()>>,
    cached: &CachedRelations<'_, '_>,
) -> Result<(), Error>
where
    G: PackageGraphProvider,
    T: TurboJsonProvider,
{
    if let Some(dependency_permissions) = dependencies {
        if let Some(denied_packages) = &dependency_permissions.deny_packages {
            check_denied_packages(
                ctx,
                diagnostics,
                denied_packages,
                pkg,
                package_name_source,
                cached.dependencies,
            );
        }

        for dependency in cached.dependencies {
            if matches!(dependency, PackageNode::Root) {
                continue;
            }

            let dependency_tags = ctx
                .turbo_json_provider
                .package_tags(dependency.as_package_name());

            diagnostics.extend(validate_relation(
                ctx,
                pkg.as_package_name(),
                package_name_source,
                dependency.as_package_name(),
                dependency_tags,
                dependency_permissions.allow.as_ref(),
                dependency_permissions.deny.as_ref(),
            )?);
        }
    }

    if let Some(dependent_permissions) = dependents {
        for dependent in cached.ancestors {
            if matches!(dependent, PackageNode::Root) {
                continue;
            }
            let dependent_tags = ctx
                .turbo_json_provider
                .package_tags(dependent.as_package_name());
            diagnostics.extend(validate_relation(
                ctx,
                pkg.as_package_name(),
                package_name_source,
                dependent.as_package_name(),
                dependent_tags,
                dependent_permissions.allow.as_ref(),
                dependent_permissions.deny.as_ref(),
            )?)
        }
    }

    Ok(())
}

fn check_if_package_name_is_tag(
    tags_rules: &ProcessedRulesMap,
    pkg: &PackageNode,
    package_name_source: Option<&Spanned<()>>,
) -> Option<BoundariesDiagnostic> {
    let rule = tags_rules.get(pkg.as_package_name().as_str())?;
    let (tag_span, tag_text) = {
        let (span, text) = rule.span.span_and_text("turbo.json");
        (span, crate::into_shared_source(text))
    };
    let (package_span, package_text) = package_name_source
        .map(|name| name.span_and_text("package.json"))
        .map(|(span, text)| (span, crate::into_shared_source(text)))
        .unwrap_or_else(|| (None, NamedSource::new("package.json", Arc::from(""))));
    Some(BoundariesDiagnostic::TagSharesPackageName {
        tag: pkg.as_package_name().to_string(),
        package: pkg.as_package_name().to_string(),
        tag_span,
        tag_text,
        secondary: [SecondaryDiagnostic::PackageDefinedHere {
            package: pkg.as_package_name().to_string(),
            package_span,
            package_text,
        }],
    })
}

/// Returns true if any tag rule (either from the package's boundaries config
/// or from the global tag rules) needs dependency checking.
fn needs_dependencies<G, T>(
    ctx: &BoundariesContext<'_, G, T>,
    pkg: &PackageNode,
    current_package_tags: Option<&Spanned<Vec<Spanned<String>>>>,
    tags_rules: Option<&ProcessedRulesMap>,
) -> bool
where
    G: PackageGraphProvider,
    T: TurboJsonProvider,
{
    let pkg_boundaries = ctx
        .turbo_json_provider
        .boundaries_config(pkg.as_package_name());
    if let Some(b) = pkg_boundaries
        && b.dependencies.is_some()
    {
        return true;
    }
    if let Some(rules) = tags_rules {
        for tag in current_package_tags.into_iter().flatten().flatten() {
            if let Some(rule) = rules.get(tag.as_inner())
                && rule.dependencies.is_some()
            {
                return true;
            }
        }
    }
    false
}

/// Returns true if any tag rule needs ancestor (dependent) checking.
fn needs_ancestors<G, T>(
    ctx: &BoundariesContext<'_, G, T>,
    pkg: &PackageNode,
    current_package_tags: Option<&Spanned<Vec<Spanned<String>>>>,
    tags_rules: Option<&ProcessedRulesMap>,
) -> bool
where
    G: PackageGraphProvider,
    T: TurboJsonProvider,
{
    let pkg_boundaries = ctx
        .turbo_json_provider
        .boundaries_config(pkg.as_package_name());
    if let Some(b) = pkg_boundaries
        && b.dependents.is_some()
    {
        return true;
    }
    if let Some(rules) = tags_rules {
        for tag in current_package_tags.into_iter().flatten().flatten() {
            if let Some(rule) = rules.get(tag.as_inner())
                && rule.dependents.is_some()
            {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
fn needs_graph_traversal_for<G, T>(
    ctx: &BoundariesContext<'_, G, T>,
    pkg: &PackageNode,
    current_package_tags: Option<&Spanned<Vec<Spanned<String>>>>,
    tags_rules: Option<&ProcessedRulesMap>,
) -> (bool, bool)
where
    G: PackageGraphProvider,
    T: TurboJsonProvider,
{
    (
        needs_dependencies(ctx, pkg, current_package_tags, tags_rules),
        needs_ancestors(ctx, pkg, current_package_tags, tags_rules),
    )
}

pub(crate) fn check_package_tags<G, T>(
    ctx: &BoundariesContext<'_, G, T>,
    pkg: PackageNode,
    package_name_source: Option<&Spanned<()>>,
    current_package_tags: Option<&Spanned<Vec<Spanned<String>>>>,
    tags_rules: Option<&ProcessedRulesMap>,
) -> Result<Vec<BoundariesDiagnostic>, Error>
where
    G: PackageGraphProvider,
    T: TurboJsonProvider,
{
    let _span = info_span!("check_package_tags", package = %pkg.as_package_name()).entered();
    let mut diagnostics = Vec::new();

    // Compute transitive dependencies and ancestors once, then reuse across
    // all tag rules for this package. Each call to `dependencies()` /
    // `ancestors()` does a full DFS — caching them here avoids O(tags * (V+E))
    // redundant traversals.
    let cached_deps: Vec<&PackageNode> =
        if needs_dependencies(ctx, &pkg, current_package_tags, tags_rules) {
            let _span =
                info_span!("compute_dependencies", package = %pkg.as_package_name()).entered();
            ctx.pkg_dep_graph.dependencies(&pkg).collect()
        } else {
            Vec::new()
        };

    let cached_ancestors: Vec<&PackageNode> =
        if needs_ancestors(ctx, &pkg, current_package_tags, tags_rules) {
            let _span = info_span!("compute_ancestors", package = %pkg.as_package_name()).entered();
            ctx.pkg_dep_graph.ancestors(&pkg).collect()
        } else {
            Vec::new()
        };

    let cached = CachedRelations {
        dependencies: &cached_deps,
        ancestors: &cached_ancestors,
    };

    // Load boundaries config for this package (matches original behavior)
    let package_boundaries = ctx
        .turbo_json_provider
        .boundaries_config(pkg.as_package_name());

    if let Some(boundaries) = package_boundaries {
        if let Some(tags) = &boundaries.tags {
            let (span, text) = {
                let (span, text) = tags.span_and_text("turbo.json");
                (span, crate::into_shared_source(text))
            };
            diagnostics.push(BoundariesDiagnostic::PackageBoundariesHasTags { span, text });
        }
        let dependencies = boundaries.dependencies.clone().map(|deps| {
            ProcessedPermissions::new(
                deps.into_inner(),
                RuleRelation::Dependencies,
                &mut diagnostics,
            )
        });
        let dependents = boundaries.dependents.clone().map(|deps| {
            ProcessedPermissions::new(
                deps.into_inner(),
                RuleRelation::Dependents,
                &mut diagnostics,
            )
        });

        check_tag_with_cache(
            ctx,
            &mut diagnostics,
            dependencies.as_ref(),
            dependents.as_ref(),
            &pkg,
            package_name_source,
            &cached,
        )?;
    }

    if let Some(tags_rules) = tags_rules {
        // We don't allow tags to share the same name as the package
        // because we allow package names to be used as a tag
        diagnostics.extend(check_if_package_name_is_tag(
            tags_rules,
            &pkg,
            package_name_source,
        ));

        for tag in current_package_tags.into_iter().flatten().flatten() {
            if let Some(rule) = tags_rules.get(tag.as_inner()) {
                check_tag_with_cache(
                    ctx,
                    &mut diagnostics,
                    rule.dependencies.as_ref(),
                    rule.dependents.as_ref(),
                    &pkg,
                    package_name_source,
                    &cached,
                )?;
            }
        }
    }

    Ok(diagnostics)
}

#[cfg(test)]
mod tests {
    use turborepo_repository::package_graph::{PackageGraphNodeKind, PackageName, PackageNode};

    use super::*;
    use crate::{BoundariesConfig, BoundariesContext, PackageGraphProvider, TurboJsonProvider};

    // Minimal mock graph that tracks packages, dependencies, and ancestors.
    struct MockGraph {
        packages: Vec<(
            PackageName,
            turbopath::AnchoredSystemPathBuf,
            turbopath::AnchoredSystemPathBuf,
        )>,
        deps: HashMap<PackageNode, Vec<PackageNode>>,
        ancestors: HashMap<PackageNode, Vec<PackageNode>>,
        external_declarations: Vec<ExternalDeclaration>,
    }

    impl MockGraph {
        fn new() -> Self {
            Self {
                packages: Vec::new(),
                deps: HashMap::new(),
                ancestors: HashMap::new(),
                external_declarations: Vec::new(),
            }
        }

        /// Declares an external dependency `declaration_name` (resolving to
        /// the npm package `package_name`) in the package.json of `package`.
        fn add_external(
            &mut self,
            package: &str,
            declaration_name: &str,
            package_name: &str,
            kind: DependencyKind,
        ) {
            self.add_external_with_specifier(
                package,
                declaration_name,
                package_name,
                "^1.0.0",
                kind,
            );
        }

        fn add_external_with_specifier(
            &mut self,
            package: &str,
            declaration_name: &str,
            package_name: &str,
            specifier: &str,
            kind: DependencyKind,
        ) {
            self.external_declarations.push(ExternalDeclaration::new(
                package,
                declaration_name,
                package_name,
                specifier,
                kind,
            ));
        }

        fn add_package(&mut self, name: &str) {
            let pkg_name = PackageName::Other(name.into());
            self.packages.push((
                pkg_name,
                turbopath::AnchoredSystemPathBuf::from_raw(format!("packages/{name}")).unwrap(),
                turbopath::AnchoredSystemPathBuf::from_raw(format!("packages/{name}/package.json"))
                    .unwrap(),
            ));
        }

        fn add_dep(&mut self, from: &str, to: &str) {
            let from_node = PackageNode::Workspace(PackageName::Other(from.into()));
            let to_node = PackageNode::Workspace(PackageName::Other(to.into()));
            self.deps
                .entry(from_node.clone())
                .or_default()
                .push(to_node.clone());
            self.ancestors.entry(to_node).or_default().push(from_node);
        }
    }

    impl PackageGraphProvider for MockGraph {
        fn package_scopes(&self) -> Box<dyn Iterator<Item = crate::PackageScope<'_>> + '_> {
            Box::new(
                self.packages
                    .iter()
                    .map(|(name, directory, definition_path)| crate::PackageScope {
                        name: name.clone(),
                        name_source: None,
                        directory,
                        definition_path,
                        kind: PackageGraphNodeKind::Package,
                    }),
            )
        }

        fn external_declarations<'a>(
            &'a self,
            name: &'a PackageName,
        ) -> turborepo_repository::external_resolution::PackageExternalDeclarations<'a> {
            turborepo_repository::external_resolution::PackageExternalDeclarations::new(
                &self.external_declarations,
                name.as_str(),
            )
        }

        fn immediate_dependencies(&self, _node: &PackageNode) -> Option<HashSet<&PackageNode>> {
            None
        }

        fn dependencies(&self, node: &PackageNode) -> Box<dyn Iterator<Item = &PackageNode> + '_> {
            match self.deps.get(node) {
                Some(deps) => Box::new(deps.iter()),
                None => Box::new(std::iter::empty()),
            }
        }

        fn ancestors(&self, node: &PackageNode) -> Box<dyn Iterator<Item = &PackageNode> + '_> {
            match self.ancestors.get(node) {
                Some(anc) => Box::new(anc.iter()),
                None => Box::new(std::iter::empty()),
            }
        }

        fn find_cycles(&self) -> Vec<Vec<PackageName>> {
            Vec::new()
        }
    }

    struct MockTurboJson {
        configs: HashMap<PackageName, BoundariesConfig>,
        tags: HashMap<PackageName, Spanned<Vec<Spanned<String>>>>,
    }

    impl MockTurboJson {
        fn new() -> Self {
            Self {
                configs: HashMap::new(),
                tags: HashMap::new(),
            }
        }

        fn set_boundaries(&mut self, pkg: &str, config: BoundariesConfig) {
            self.configs.insert(PackageName::Other(pkg.into()), config);
        }

        fn set_tags(&mut self, pkg: &str, tags: Vec<&str>) {
            let spanned_tags: Vec<Spanned<String>> =
                tags.into_iter().map(|t| Spanned::new(t.into())).collect();
            self.tags
                .insert(PackageName::Other(pkg.into()), Spanned::new(spanned_tags));
        }
    }

    impl TurboJsonProvider for MockTurboJson {
        fn has_turbo_json(&self, pkg: &PackageName) -> bool {
            self.configs.contains_key(pkg) || self.tags.contains_key(pkg)
        }

        fn boundaries_config(&self, pkg: &PackageName) -> Option<&BoundariesConfig> {
            self.configs.get(pkg)
        }

        fn package_tags(&self, pkg: &PackageName) -> Option<&Spanned<Vec<Spanned<String>>>> {
            self.tags.get(pkg)
        }

        fn implicit_dependencies(&self, _pkg: &PackageName) -> HashMap<String, Spanned<()>> {
            HashMap::new()
        }
    }

    fn make_permissions(allow: Option<Vec<&str>>, deny: Option<Vec<&str>>) -> Permissions {
        Permissions {
            allow: allow.map(|tags| {
                Spanned::new(tags.into_iter().map(|t| Spanned::new(t.into())).collect())
            }),
            deny: deny.map(|tags| {
                Spanned::new(tags.into_iter().map(|t| Spanned::new(t.into())).collect())
            }),
            deny_packages: None,
        }
    }

    fn make_repo_root() -> turbopath::AbsoluteSystemPathBuf {
        #[cfg(unix)]
        {
            turbopath::AbsoluteSystemPathBuf::new("/tmp/test-repo").unwrap()
        }
        #[cfg(windows)]
        {
            turbopath::AbsoluteSystemPathBuf::new("C:\\tmp\\test-repo").unwrap()
        }
    }

    fn package_name_source() -> Spanned<()> {
        Spanned::new(())
            .with_range(9..16)
            .with_text(r#"{"name": "pkg-a"}"#)
            .with_path("packages/pkg-a/package.json".into())
    }

    #[test]
    fn package_name_collision_uses_authoritative_provenance() {
        let rules = [(
            "pkg-a".to_string(),
            ProcessedRule {
                span: Spanned::new(()),
                dependencies: None,
                dependents: None,
            },
        )]
        .into();
        let pkg = PackageNode::Workspace(PackageName::Other("pkg-a".into()));
        let source = package_name_source();

        let diagnostic = check_if_package_name_is_tag(&rules, &pkg, Some(&source)).unwrap();
        let BoundariesDiagnostic::TagSharesPackageName { secondary, .. } = diagnostic else {
            panic!("expected package-name collision diagnostic");
        };
        let SecondaryDiagnostic::PackageDefinedHere {
            package_span,
            package_text,
            ..
        } = &secondary[0]
        else {
            panic!("expected package definition provenance");
        };

        assert_eq!(package_span.unwrap().offset(), 9);
        assert_eq!(package_span.unwrap().len(), 7);
        assert_eq!(package_text.name(), "packages/pkg-a/package.json");
    }

    #[test]
    fn denied_package_name_uses_authoritative_provenance() {
        let graph = MockGraph::new();
        let turbo_json = MockTurboJson::new();
        let repo_root = make_repo_root();
        let filtered = HashSet::new();
        let ctx = BoundariesContext {
            repo_root: &repo_root,
            pkg_dep_graph: &graph,
            turbo_json_provider: &turbo_json,
            root_boundaries_config: None,
            filtered_pkgs: &filtered,
        };
        let source = package_name_source();
        let deny = Spanned::new(HashSet::from(["pkg-b".to_string()]));

        let diagnostic = validate_relation(
            &ctx,
            &PackageName::Other("pkg-a".into()),
            Some(&source),
            &PackageName::Other("pkg-b".into()),
            None,
            None,
            Some(&deny),
        )
        .unwrap()
        .unwrap();
        let BoundariesDiagnostic::DeniedTag { span, text, .. } = diagnostic else {
            panic!("expected denied-tag diagnostic");
        };

        assert_eq!(span.unwrap().offset(), 9);
        assert_eq!(span.unwrap().len(), 7);
        assert_eq!(text.name(), "packages/pkg-a/package.json");
    }

    // -- needs_dependencies / needs_ancestors tests --

    #[test]
    fn needs_traversal_false_when_no_rules() {
        let graph = MockGraph::new();
        let turbo_json = MockTurboJson::new();
        let repo_root = make_repo_root();
        let filtered = HashSet::new();
        let ctx = BoundariesContext {
            repo_root: &repo_root,
            pkg_dep_graph: &graph,
            turbo_json_provider: &turbo_json,
            root_boundaries_config: None,
            filtered_pkgs: &filtered,
        };
        let pkg = PackageNode::Workspace(PackageName::Other("pkg-a".into()));

        let (need_deps, need_anc) = needs_graph_traversal_for(&ctx, &pkg, None, None);
        assert!(!need_deps);
        assert!(!need_anc);
    }

    #[test]
    fn needs_traversal_true_from_package_boundaries_config() {
        let graph = MockGraph::new();
        let mut turbo_json = MockTurboJson::new();
        turbo_json.set_boundaries(
            "pkg-a",
            BoundariesConfig {
                dependencies: Some(Spanned::new(make_permissions(Some(vec!["allowed"]), None))),
                dependents: Some(Spanned::new(make_permissions(None, Some(vec!["denied"])))),
                ..Default::default()
            },
        );
        let repo_root = make_repo_root();
        let filtered = HashSet::new();
        let ctx = BoundariesContext {
            repo_root: &repo_root,
            pkg_dep_graph: &graph,
            turbo_json_provider: &turbo_json,
            root_boundaries_config: None,
            filtered_pkgs: &filtered,
        };
        let pkg = PackageNode::Workspace(PackageName::Other("pkg-a".into()));

        let (need_deps, need_anc) = needs_graph_traversal_for(&ctx, &pkg, None, None);
        assert!(
            need_deps,
            "should need deps when boundaries config has dependencies"
        );
        assert!(
            need_anc,
            "should need ancestors when boundaries config has dependents"
        );
    }

    #[test]
    fn needs_traversal_true_from_tag_rules() {
        let graph = MockGraph::new();
        let turbo_json = MockTurboJson::new();
        let repo_root = make_repo_root();
        let filtered = HashSet::new();
        let ctx = BoundariesContext {
            repo_root: &repo_root,
            pkg_dep_graph: &graph,
            turbo_json_provider: &turbo_json,
            root_boundaries_config: None,
            filtered_pkgs: &filtered,
        };
        let pkg = PackageNode::Workspace(PackageName::Other("pkg-a".into()));

        let tag_rules: ProcessedRulesMap = [(
            "my-tag".into(),
            ProcessedRule {
                span: Spanned::new(()),
                dependencies: Some(ProcessedPermissions {
                    allow: None,
                    deny: None,
                    deny_packages: None,
                }),
                dependents: None,
            },
        )]
        .into();

        let tags = Spanned::new(vec![Spanned::new("my-tag".into())]);

        let (need_deps, need_anc) =
            needs_graph_traversal_for(&ctx, &pkg, Some(&tags), Some(&tag_rules));
        assert!(need_deps, "should need deps when tag rule has dependencies");
        assert!(
            !need_anc,
            "should not need ancestors when no dependents rule"
        );
    }

    #[test]
    fn needs_traversal_only_dependents_from_tag_rules() {
        let graph = MockGraph::new();
        let turbo_json = MockTurboJson::new();
        let repo_root = make_repo_root();
        let filtered = HashSet::new();
        let ctx = BoundariesContext {
            repo_root: &repo_root,
            pkg_dep_graph: &graph,
            turbo_json_provider: &turbo_json,
            root_boundaries_config: None,
            filtered_pkgs: &filtered,
        };
        let pkg = PackageNode::Workspace(PackageName::Other("pkg-a".into()));

        let tag_rules: ProcessedRulesMap = [(
            "my-tag".into(),
            ProcessedRule {
                span: Spanned::new(()),
                dependencies: None,
                dependents: Some(ProcessedPermissions {
                    allow: None,
                    deny: None,
                    deny_packages: None,
                }),
            },
        )]
        .into();

        let tags = Spanned::new(vec![Spanned::new("my-tag".into())]);

        let (need_deps, need_anc) =
            needs_graph_traversal_for(&ctx, &pkg, Some(&tags), Some(&tag_rules));
        assert!(!need_deps);
        assert!(need_anc);
    }

    // -- DFS caching / check_package_tags tests --

    #[test]
    fn cached_deps_used_across_multiple_tag_rules() {
        // Graph: pkg-a -> pkg-b, pkg-a -> pkg-c
        // pkg-b has tag "lib", pkg-c has tag "util"
        // pkg-a has two tags: "tag1" (allow deps with "lib") and "tag2" (allow deps
        // with "util") Both rules should see the same cached dependency set.
        let mut graph = MockGraph::new();
        graph.add_package("pkg-a");
        graph.add_package("pkg-b");
        graph.add_package("pkg-c");
        graph.add_dep("pkg-a", "pkg-b");
        graph.add_dep("pkg-a", "pkg-c");

        let mut turbo_json = MockTurboJson::new();
        turbo_json.set_tags("pkg-a", vec!["tag1", "tag2"]);
        turbo_json.set_tags("pkg-b", vec!["lib"]);
        turbo_json.set_tags("pkg-c", vec!["util"]);

        let repo_root = make_repo_root();
        let filtered = HashSet::new();
        let ctx = BoundariesContext {
            repo_root: &repo_root,
            pkg_dep_graph: &graph,
            turbo_json_provider: &turbo_json,
            root_boundaries_config: None,
            filtered_pkgs: &filtered,
        };

        // tag1 allows only "lib", tag2 allows only "util"
        let tag_rules: ProcessedRulesMap = [
            (
                "tag1".into(),
                ProcessedRule {
                    span: Spanned::new(()),
                    dependencies: Some(ProcessedPermissions {
                        allow: Some(Spanned::new(["lib".into()].into())),
                        deny: None,
                        deny_packages: None,
                    }),
                    dependents: None,
                },
            ),
            (
                "tag2".into(),
                ProcessedRule {
                    span: Spanned::new(()),
                    dependencies: Some(ProcessedPermissions {
                        allow: Some(Spanned::new(["util".into()].into())),
                        deny: None,
                        deny_packages: None,
                    }),
                    dependents: None,
                },
            ),
        ]
        .into();

        let pkg = PackageNode::Workspace(PackageName::Other("pkg-a".into()));
        let tags = turbo_json.package_tags(&PackageName::Other("pkg-a".into()));

        let diagnostics = check_package_tags(&ctx, pkg, None, tags, Some(&tag_rules)).unwrap();

        // tag1 allows "lib": pkg-b has "lib" (ok), pkg-c has "util" (violation)
        // tag2 allows "util": pkg-c has "util" (ok), pkg-b has "lib" (violation)
        // So we expect 2 NoTagInAllowlist diagnostics
        let allowlist_violations: Vec<_> = diagnostics
            .iter()
            .filter(|d| matches!(d, BoundariesDiagnostic::NoTagInAllowlist { .. }))
            .collect();
        assert_eq!(
            allowlist_violations.len(),
            2,
            "expected 2 allowlist violations from 2 tag rules seeing same cached deps, got: {}",
            allowlist_violations.len()
        );
    }

    #[test]
    fn no_dfs_when_no_rules_need_it() {
        // If no rules require dependency/ancestor checks, the DFS should be skipped.
        // We verify by giving pkg-a dependencies but no rules that would trigger
        // checking.
        let mut graph = MockGraph::new();
        graph.add_package("pkg-a");
        graph.add_package("pkg-b");
        graph.add_dep("pkg-a", "pkg-b");

        let mut turbo_json = MockTurboJson::new();
        turbo_json.set_tags("pkg-a", vec!["my-tag"]);

        let repo_root = make_repo_root();
        let filtered = HashSet::new();
        let ctx = BoundariesContext {
            repo_root: &repo_root,
            pkg_dep_graph: &graph,
            turbo_json_provider: &turbo_json,
            root_boundaries_config: None,
            filtered_pkgs: &filtered,
        };

        // Tag rule has no dependencies or dependents fields
        let tag_rules: ProcessedRulesMap = [(
            "my-tag".into(),
            ProcessedRule {
                span: Spanned::new(()),
                dependencies: None,
                dependents: None,
            },
        )]
        .into();

        let pkg = PackageNode::Workspace(PackageName::Other("pkg-a".into()));
        let tags = turbo_json.package_tags(&PackageName::Other("pkg-a".into()));

        let diagnostics = check_package_tags(&ctx, pkg, None, tags, Some(&tag_rules)).unwrap();

        // No dependency/dependent rules → no violations possible from tag checking
        let tag_violations: Vec<_> = diagnostics
            .iter()
            .filter(|d| {
                matches!(
                    d,
                    BoundariesDiagnostic::NoTagInAllowlist { .. }
                        | BoundariesDiagnostic::DeniedTag { .. }
                )
            })
            .collect();
        assert!(
            tag_violations.is_empty(),
            "expected no tag violations when rules don't check deps/dependents"
        );
    }

    #[test]
    fn check_tag_with_cache_skips_root_nodes() {
        let graph = MockGraph::new();
        let turbo_json = MockTurboJson::new();
        let repo_root = make_repo_root();
        let filtered = HashSet::new();
        let ctx = BoundariesContext {
            repo_root: &repo_root,
            pkg_dep_graph: &graph,
            turbo_json_provider: &turbo_json,
            root_boundaries_config: None,
            filtered_pkgs: &filtered,
        };

        let pkg = PackageNode::Workspace(PackageName::Other("pkg-a".into()));
        let perms = ProcessedPermissions {
            allow: Some(Spanned::new(["allowed-tag".into()].into())),
            deny: None,
            deny_packages: None,
        };

        // Include Root in the cached deps — it should be skipped
        let root = PackageNode::Root;
        let cached_deps = vec![&root];
        let mut diagnostics = Vec::new();

        check_tag_with_cache(
            &ctx,
            &mut diagnostics,
            Some(&perms),
            None,
            &pkg,
            None,
            &CachedRelations {
                dependencies: &cached_deps,
                ancestors: &[],
            },
        )
        .unwrap();

        assert!(
            diagnostics.is_empty(),
            "Root node should be skipped, producing no diagnostics"
        );
    }

    // -- denyPackages tests --

    fn deny_packages_rule(patterns: &[&str]) -> (ProcessedRulesMap, Vec<BoundariesDiagnostic>) {
        let mut diagnostics = Vec::new();
        let rule = Rule {
            dependencies: Some(Spanned::new(Permissions {
                deny_packages: Some(Spanned::new(
                    patterns
                        .iter()
                        .map(|pattern| Spanned::new(pattern.to_string()))
                        .collect(),
                )),
                ..Default::default()
            })),
            dependents: None,
        };
        let rules = [(
            "browser".to_string(),
            ProcessedRule::new(Spanned::new(rule), &mut diagnostics),
        )]
        .into();
        (rules, diagnostics)
    }

    /// Runs `check_package_tags` for `pkg`, which is tagged `browser`.
    fn check_browser_package(
        graph: &MockGraph,
        rules: &ProcessedRulesMap,
        pkg: &str,
    ) -> Vec<BoundariesDiagnostic> {
        let mut turbo_json = MockTurboJson::new();
        turbo_json.set_tags(pkg, vec!["browser"]);
        let repo_root = make_repo_root();
        let filtered = HashSet::new();
        let ctx = BoundariesContext {
            repo_root: &repo_root,
            pkg_dep_graph: graph,
            turbo_json_provider: &turbo_json,
            root_boundaries_config: None,
            filtered_pkgs: &filtered,
        };
        let pkg_name = PackageName::Other(pkg.into());
        let tags = turbo_json.package_tags(&pkg_name);
        check_package_tags(
            &ctx,
            PackageNode::Workspace(pkg_name.clone()),
            None,
            tags,
            Some(rules),
        )
        .unwrap()
    }

    /// Returns `(declared_by, dependency, dependency_kind, pattern)` for every
    /// `DeniedPackage` diagnostic.
    fn denied_packages(
        diagnostics: &[BoundariesDiagnostic],
    ) -> Vec<(String, String, &'static str, String)> {
        diagnostics
            .iter()
            .filter_map(|diagnostic| match diagnostic {
                BoundariesDiagnostic::DeniedPackage {
                    declared_by,
                    dependency,
                    dependency_kind,
                    pattern,
                    ..
                } => Some((
                    declared_by.to_string(),
                    dependency.clone(),
                    *dependency_kind,
                    pattern.clone(),
                )),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn deny_packages_reports_direct_dependency() {
        let mut graph = MockGraph::new();
        graph.add_package("web");
        graph.add_external("web", "pg", "pg", DependencyKind::Production);
        graph.add_external("web", "react", "react", DependencyKind::Production);
        let (rules, config_diagnostics) = deny_packages_rule(&["pg"]);
        assert!(config_diagnostics.is_empty());

        let diagnostics = check_browser_package(&graph, &rules, "web");

        assert_eq!(
            denied_packages(&diagnostics),
            vec![(
                "web".to_string(),
                "pg".to_string(),
                "dependencies",
                "pg".to_string()
            )]
        );
        assert_eq!(
            diagnostics[0].to_string(),
            "Package `web` depends on denied package `pg` (declared in `dependencies` of `web`)"
        );
    }

    #[test]
    fn deny_packages_reports_transitive_workspace_dependency() {
        // web -> ui -> db, and db declares pg. The mock graph's `dependencies`
        // returns the transitive set as-is, so list both edges from web.
        let mut graph = MockGraph::new();
        graph.add_package("web");
        graph.add_package("ui");
        graph.add_package("db");
        graph.add_dep("web", "ui");
        graph.add_dep("web", "db");
        graph.add_dep("ui", "db");
        graph.add_external("db", "pg", "pg", DependencyKind::Production);
        graph.add_external(
            "ui",
            "react",
            "react",
            DependencyKind::Peer { optional: false },
        );
        let (rules, _) = deny_packages_rule(&["pg"]);

        let diagnostics = check_browser_package(&graph, &rules, "web");

        assert_eq!(
            denied_packages(&diagnostics),
            vec![(
                "db".to_string(),
                "pg".to_string(),
                "dependencies",
                "pg".to_string()
            )]
        );
        let BoundariesDiagnostic::DeniedPackage { help, .. } = &diagnostics[0] else {
            panic!("expected denied package diagnostic");
        };
        assert_eq!(
            help.as_deref(),
            Some("`web` depends on `db`, which declares `pg`")
        );
        assert_eq!(
            diagnostics[0].to_string(),
            "Package `web` depends on denied package `pg` (declared in `dependencies` of `db`)"
        );
    }

    #[test]
    fn deny_packages_matches_globs() {
        let mut graph = MockGraph::new();
        graph.add_package("web");
        for name in [
            "@aws-sdk/client-s3",
            "@aws-sdk-fake/client",
            "@other/aws-sdk",
            "drizzle-orm",
            "drizzle",
            "@scope/drizzle-orm",
        ] {
            graph.add_external("web", name, name, DependencyKind::Production);
        }
        let (rules, config_diagnostics) = deny_packages_rule(&["@aws-sdk/*", "drizzle-*"]);
        assert!(config_diagnostics.is_empty());

        let diagnostics = check_browser_package(&graph, &rules, "web");

        let mut denied: Vec<_> = denied_packages(&diagnostics)
            .into_iter()
            .map(|(_, dependency, _, pattern)| (dependency, pattern))
            .collect();
        denied.sort();
        assert_eq!(
            denied,
            vec![
                ("@aws-sdk/client-s3".to_string(), "@aws-sdk/*".to_string()),
                ("drizzle-orm".to_string(), "drizzle-*".to_string()),
            ]
        );
    }

    #[test]
    fn deny_packages_matches_npm_alias_target() {
        // "database": "npm:pg@^8"
        let mut graph = MockGraph::new();
        graph.add_package("web");
        graph.add_external("web", "database", "pg", DependencyKind::Production);
        let (rules, _) = deny_packages_rule(&["pg"]);

        let diagnostics = check_browser_package(&graph, &rules, "web");

        assert_eq!(
            denied_packages(&diagnostics),
            vec![(
                "web".to_string(),
                "database".to_string(),
                "dependencies",
                "pg".to_string()
            )]
        );
        let BoundariesDiagnostic::DeniedPackage { help, .. } = &diagnostics[0] else {
            panic!("expected denied package diagnostic");
        };
        assert_eq!(help.as_deref(), Some("`database` is an alias of `pg`"));
    }

    #[test]
    fn deny_packages_matches_npm_alias_specifier() {
        // JavaScript package graphs record npm aliases under the declared name
        // and keep the real package in the specifier:
        // "database": "npm:pg@^8", "s3": "npm:@aws-sdk/client-s3"
        let mut graph = MockGraph::new();
        graph.add_package("web");
        graph.add_external_with_specifier(
            "web",
            "database",
            "database",
            "npm:pg@^8",
            DependencyKind::Production,
        );
        graph.add_external_with_specifier(
            "web",
            "s3",
            "s3",
            "npm:@aws-sdk/client-s3",
            DependencyKind::Production,
        );
        // A plain npm range is not an alias.
        graph.add_external_with_specifier(
            "web",
            "react",
            "react",
            "npm:^19.0.0",
            DependencyKind::Production,
        );
        let (rules, _) = deny_packages_rule(&["pg", "@aws-sdk/*", "react-*"]);

        let diagnostics = check_browser_package(&graph, &rules, "web");

        let mut denied: Vec<_> = denied_packages(&diagnostics)
            .into_iter()
            .map(|(_, dependency, _, pattern)| (dependency, pattern))
            .collect();
        denied.sort();
        assert_eq!(
            denied,
            vec![
                ("database".to_string(), "pg".to_string()),
                ("s3".to_string(), "@aws-sdk/*".to_string()),
            ]
        );
    }

    #[test_case::test_case("npm:pg@^8", Some("pg") ; "unscoped")]
    #[test_case::test_case("npm:pg", Some("pg") ; "unscoped without version")]
    #[test_case::test_case("npm:@aws-sdk/client-s3@3.0.0", Some("@aws-sdk/client-s3") ; "scoped")]
    #[test_case::test_case("npm:@aws-sdk/client-s3", Some("@aws-sdk/client-s3") ; "scoped without version")]
    #[test_case::test_case("npm:^1.0.0", None ; "range")]
    #[test_case::test_case("npm:*", None ; "wildcard")]
    #[test_case::test_case("^1.0.0", None ; "not npm protocol")]
    fn npm_alias_target_parses_specifiers(specifier: &str, expected: Option<&str>) {
        assert_eq!(npm_alias_target(specifier), expected);
    }

    #[test]
    fn deny_packages_matches_alias_name() {
        // "pg": "npm:@neondatabase/serverless@^1" is still denied by "pg"
        let mut graph = MockGraph::new();
        graph.add_package("web");
        graph.add_external(
            "web",
            "pg",
            "@neondatabase/serverless",
            DependencyKind::Production,
        );
        let (rules, _) = deny_packages_rule(&["pg"]);

        let diagnostics = check_browser_package(&graph, &rules, "web");

        assert_eq!(denied_packages(&diagnostics).len(), 1);
    }

    #[test]
    fn deny_packages_reports_all_dependency_kinds() {
        let mut graph = MockGraph::new();
        graph.add_package("web");
        graph.add_external("web", "pg", "pg", DependencyKind::Development);
        graph.add_external("web", "pg-native", "pg-native", DependencyKind::Optional);
        graph.add_external(
            "web",
            "drizzle-orm",
            "drizzle-orm",
            DependencyKind::Peer { optional: true },
        );
        let (rules, _) = deny_packages_rule(&["pg", "pg-*", "drizzle-*"]);

        let diagnostics = check_browser_package(&graph, &rules, "web");

        let mut kinds: Vec<_> = denied_packages(&diagnostics)
            .into_iter()
            .map(|(_, dependency, kind, _)| (dependency, kind))
            .collect();
        kinds.sort();
        assert_eq!(
            kinds,
            vec![
                ("drizzle-orm".to_string(), "peerDependencies"),
                ("pg".to_string(), "devDependencies"),
                ("pg-native".to_string(), "optionalDependencies"),
            ]
        );
    }

    #[test]
    fn deny_packages_has_no_false_positives() {
        // Similar names, dependents, and unrelated packages are not reported.
        let mut graph = MockGraph::new();
        graph.add_package("web");
        graph.add_package("server");
        graph.add_package("other");
        graph.add_dep("server", "web");
        graph.add_external("web", "pg-boss", "pg-boss", DependencyKind::Production);
        graph.add_external("web", "@types/pg", "@types/pg", DependencyKind::Development);
        graph.add_external("web", "pgx", "pgx", DependencyKind::Production);
        // A dependent of web declaring pg is not web's dependency.
        graph.add_external("server", "pg", "pg", DependencyKind::Production);
        // Neither is an unrelated package.
        graph.add_external("other", "pg", "pg", DependencyKind::Production);
        let (rules, _) = deny_packages_rule(&["pg"]);

        let diagnostics = check_browser_package(&graph, &rules, "web");

        assert!(
            diagnostics.is_empty(),
            "expected no diagnostics, got: {diagnostics:?}"
        );
    }

    #[test]
    fn deny_packages_skips_root_node() {
        let mut graph = MockGraph::new();
        graph.add_package("web");
        graph.deps.insert(
            PackageNode::Workspace(PackageName::Other("web".into())),
            vec![PackageNode::Root],
        );
        graph.add_external("//", "pg", "pg", DependencyKind::Development);
        let (rules, _) = deny_packages_rule(&["pg"]);

        let diagnostics = check_browser_package(&graph, &rules, "web");

        assert!(denied_packages(&diagnostics).is_empty());
    }

    #[test]
    fn deny_packages_in_package_boundaries_config() {
        let mut graph = MockGraph::new();
        graph.add_package("web");
        graph.add_package("db");
        graph.add_dep("web", "db");
        graph.add_external("db", "pg", "pg", DependencyKind::Production);

        let mut turbo_json = MockTurboJson::new();
        turbo_json.set_boundaries(
            "web",
            BoundariesConfig {
                dependencies: Some(Spanned::new(Permissions {
                    deny_packages: Some(Spanned::new(vec![Spanned::new("pg".into())])),
                    ..Default::default()
                })),
                ..Default::default()
            },
        );
        let repo_root = make_repo_root();
        let filtered = HashSet::new();
        let ctx = BoundariesContext {
            repo_root: &repo_root,
            pkg_dep_graph: &graph,
            turbo_json_provider: &turbo_json,
            root_boundaries_config: None,
            filtered_pkgs: &filtered,
        };
        let pkg = PackageNode::Workspace(PackageName::Other("web".into()));

        let (need_deps, _) = needs_graph_traversal_for(&ctx, &pkg, None, None);
        assert!(need_deps, "denyPackages must compute the transitive set");

        let diagnostics = check_package_tags(&ctx, pkg, None, None, None).unwrap();

        assert_eq!(
            denied_packages(&diagnostics),
            vec![(
                "db".to_string(),
                "pg".to_string(),
                "dependencies",
                "pg".to_string()
            )]
        );
    }

    #[test]
    fn deny_packages_in_dependents_is_rejected() {
        let mut diagnostics = Vec::new();
        let rule = Rule {
            dependencies: None,
            dependents: Some(Spanned::new(Permissions {
                deny_packages: Some(Spanned::new(vec![Spanned::new("pg".into())])),
                ..Default::default()
            })),
        };

        let processed = ProcessedRule::new(Spanned::new(rule), &mut diagnostics);

        assert!(
            processed
                .dependents
                .is_some_and(|dependents| dependents.deny_packages.is_none())
        );
        assert!(matches!(
            diagnostics.as_slice(),
            [BoundariesDiagnostic::DenyPackagesInDependents { .. }]
        ));
    }

    #[test]
    fn deny_packages_reports_invalid_globs_and_keeps_valid_ones() {
        let (rules, config_diagnostics) = deny_packages_rule(&["{pg", "drizzle-*"]);

        assert!(matches!(
            config_diagnostics.as_slice(),
            [BoundariesDiagnostic::InvalidDenyPackagesPattern { pattern, .. }] if pattern == "{pg"
        ));

        let mut graph = MockGraph::new();
        graph.add_package("web");
        graph.add_external(
            "web",
            "drizzle-orm",
            "drizzle-orm",
            DependencyKind::Production,
        );
        let diagnostics = check_browser_package(&graph, &rules, "web");
        assert_eq!(denied_packages(&diagnostics).len(), 1);
    }
}
