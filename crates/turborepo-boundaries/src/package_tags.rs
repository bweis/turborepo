//! Resolution of the boundaries tags assigned to each package.
//!
//! A package's tags are the union of the tags listed in its own `turbo.json`
//! and the tags assigned to its directory by `boundaries.packageTags` in the
//! root `turbo.json`. Tags are resolved once per `turbo boundaries` invocation
//! so that the rule checks, which look up the tags of every transitive
//! dependency and dependent, only perform a map lookup.

use std::collections::{HashMap, HashSet};

use turbopath::AnchoredSystemPath;
use turborepo_errors::Spanned;
use turborepo_repository::package_graph::PackageName;
use wax::{Glob, Program};

use crate::{BoundariesDiagnostic, TurboJsonProvider, config::PackageTagsMap};

/// Tags as they appear in configuration: the outer span locates the list as
/// a whole, each inner span locates an individual tag.
pub(crate) type Tags = Spanned<Vec<Spanned<String>>>;

/// The resolved tags of every package in the repository.
#[derive(Debug, Default)]
pub(crate) struct PackageTagIndex {
    tags: HashMap<PackageName, Tags>,
}

/// The result of resolving package tags: the index along with any problems
/// found in `boundaries.packageTags`.
pub(crate) struct ResolvedPackageTags {
    pub(crate) index: PackageTagIndex,
    pub(crate) diagnostics: Vec<BoundariesDiagnostic>,
    pub(crate) warnings: Vec<String>,
}

struct PackageTagsEntry<'a> {
    pattern: &'a str,
    glob: Glob<'a>,
    tags: &'a Tags,
    matched: bool,
}

impl PackageTagIndex {
    /// Resolves the tags of each package.
    ///
    /// `package_tags_config` is the `boundaries.packageTags` field of the root
    /// `turbo.json`. `packages` must contain every package in the repository
    /// (not just the ones being checked), since rules look up the tags of
    /// dependencies and dependents.
    pub(crate) fn resolve<'a, T: TurboJsonProvider>(
        turbo_json_provider: &T,
        package_tags_config: Option<&'a Spanned<PackageTagsMap>>,
        packages: impl IntoIterator<Item = (&'a PackageName, &'a AnchoredSystemPath)>,
    ) -> ResolvedPackageTags {
        let mut diagnostics = Vec::new();
        let mut entries = Vec::new();
        for (pattern, tags) in package_tags_config.into_iter().flat_map(|c| c.as_inner()) {
            match compile_glob(pattern) {
                Ok(glob) => entries.push(PackageTagsEntry {
                    pattern,
                    glob,
                    tags,
                    matched: false,
                }),
                Err(reason) => {
                    let (span, text) = tags.span_and_text("turbo.json");
                    diagnostics.push(BoundariesDiagnostic::InvalidPackageTagsGlob {
                        glob: pattern.clone(),
                        reason,
                        span,
                        text: crate::into_shared_source(text),
                    });
                }
            }
        }

        let mut index = PackageTagIndex::default();
        for (name, directory) in packages {
            let own_tags = turbo_json_provider.package_tags(name);
            // `packageTags` assigns tags to workspace packages only. The root
            // package's empty directory would otherwise match globs such as
            // `**`, which would also hide them from the unmatched-glob warning.
            let is_root = *name == PackageName::Root || directory.as_str().is_empty();
            let directory = directory.to_unix();
            let mut matching = entries
                .iter_mut()
                .filter(|entry| !is_root && entry.glob.is_match(directory.as_str()))
                .peekable();

            // Packages without any tag source keep having no entry, which
            // lets diagnostics explain that no tags were found.
            if own_tags.is_none() && matching.peek().is_none() {
                continue;
            }

            let mut seen = HashSet::new();
            let mut tags = Vec::new();
            // Diagnostics about the tag list as a whole point at the package's
            // own `tags`, or at the first matching glob if it has none.
            let mut list_span = own_tags.map(|own_tags| own_tags.to(()));
            for tag in own_tags
                .into_iter()
                .flat_map(|own_tags| own_tags.as_inner())
            {
                if seen.insert(tag.as_inner().as_str()) {
                    tags.push(tag.clone());
                }
            }
            for entry in matching {
                entry.matched = true;
                let entry_tags: &'a Tags = entry.tags;
                list_span.get_or_insert_with(|| entry_tags.to(()));
                for tag in entry_tags.as_inner() {
                    if seen.insert(tag.as_inner().as_str()) {
                        tags.push(tag.clone());
                    }
                }
            }

            let list_span = list_span.unwrap_or_default();
            index.tags.insert(name.clone(), list_span.to(tags));
        }

        let warnings = entries
            .iter()
            .filter(|entry| !entry.matched)
            .map(|entry| {
                format!(
                    "`boundaries.packageTags` glob `{}` does not match any package directory",
                    entry.pattern
                )
            })
            .collect();

        ResolvedPackageTags {
            index,
            diagnostics,
            warnings,
        }
    }

    /// Returns the resolved tags of `package`, or `None` if the package has
    /// no `tags` in its `turbo.json` and no glob in `packageTags` matches it.
    pub(crate) fn get(&self, package: &PackageName) -> Option<&Tags> {
        self.tags.get(package)
    }
}

fn compile_glob(pattern: &str) -> Result<Glob<'_>, String> {
    if pattern.starts_with('!') {
        return Err("negated globs are not supported".to_string());
    }
    let normalized = pattern.strip_prefix("./").unwrap_or(pattern);
    let normalized = normalized.strip_suffix('/').unwrap_or(normalized);
    if normalized.is_empty() {
        return Err("glob must not be empty".to_string());
    }
    Glob::new(normalized).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use turbopath::AnchoredSystemPathBuf;
    use turborepo_errors::WithMetadata;

    use super::*;
    use crate::BoundariesConfig;

    #[derive(Default)]
    struct MockTurboJson {
        tags: HashMap<PackageName, Tags>,
    }

    impl TurboJsonProvider for MockTurboJson {
        fn has_turbo_json(&self, pkg: &PackageName) -> bool {
            self.tags.contains_key(pkg)
        }

        fn boundaries_config(&self, _: &PackageName) -> Option<&BoundariesConfig> {
            None
        }

        fn package_tags(&self, pkg: &PackageName) -> Option<&Tags> {
            self.tags.get(pkg)
        }

        fn implicit_dependencies(&self, _: &PackageName) -> HashMap<String, Spanned<()>> {
            HashMap::new()
        }
    }

    fn package_tags_config(json: &str) -> Spanned<PackageTagsMap> {
        let json = format!(r#"{{"packageTags": {json}}}"#);
        let (config, _) = turborepo_errors::json::deserialize_from_json_str::<BoundariesConfig>(
            &json,
            biome_json_parser::JsonParserOptions::default(),
            "turbo.json",
        );
        let mut config = config.unwrap();
        let text: Arc<str> = json.into();
        config.add_text(text);
        config.add_path("turbo.json".into());
        config.package_tags.unwrap()
    }

    fn resolve(
        provider: &MockTurboJson,
        config: Option<&Spanned<PackageTagsMap>>,
        packages: &[(&str, &str)],
    ) -> ResolvedPackageTags {
        let packages: Vec<_> = packages
            .iter()
            .map(|(name, directory)| {
                (
                    PackageName::Other(name.to_string()),
                    AnchoredSystemPathBuf::from_raw(directory).unwrap(),
                )
            })
            .collect();
        PackageTagIndex::resolve(
            provider,
            config,
            packages
                .iter()
                .map(|(name, directory)| (name, directory.as_ref())),
        )
    }

    fn tags_of(index: &PackageTagIndex, package: &str) -> Option<Vec<String>> {
        index
            .get(&PackageName::Other(package.into()))
            .map(|tags| tags.iter().map(|tag| tag.as_inner().clone()).collect())
    }

    #[test]
    fn assigns_tags_by_directory_glob() {
        let config = package_tags_config(
            r#"{
                "apps/web-*": ["browser"],
                "packages/server/*": ["node"],
                "packages/**": ["library"]
            }"#,
        );
        let resolved = resolve(
            &MockTurboJson::default(),
            Some(&config),
            &[
                ("web-shop", "apps/web-shop"),
                ("docs", "apps/docs"),
                ("api", "packages/server/api"),
                ("ui", "packages/ui"),
            ],
        );

        assert!(resolved.diagnostics.is_empty());
        assert!(resolved.warnings.is_empty(), "{:?}", resolved.warnings);
        assert_eq!(
            tags_of(&resolved.index, "web-shop"),
            Some(vec!["browser".into()])
        );
        assert_eq!(tags_of(&resolved.index, "docs"), None);
        let mut api_tags = tags_of(&resolved.index, "api").unwrap();
        api_tags.sort();
        assert_eq!(api_tags, vec!["library".to_string(), "node".to_string()]);
        assert_eq!(tags_of(&resolved.index, "ui"), Some(vec!["library".into()]));
    }

    #[test]
    fn single_star_does_not_cross_directories() {
        let config = package_tags_config(r#"{"packages/*": ["shallow"]}"#);
        let resolved = resolve(
            &MockTurboJson::default(),
            Some(&config),
            &[("nested", "packages/group/nested")],
        );

        assert_eq!(tags_of(&resolved.index, "nested"), None);
        assert_eq!(resolved.warnings.len(), 1);
    }

    #[test]
    fn root_package_is_never_matched() {
        let config = package_tags_config(r#"{"**": ["everything"]}"#);
        let root_directory = AnchoredSystemPathBuf::from_raw("").unwrap();
        let resolved = PackageTagIndex::resolve(
            &MockTurboJson::default(),
            Some(&config),
            [(&PackageName::Root, root_directory.as_ref())],
        );

        assert!(resolved.index.get(&PackageName::Root).is_none());
        // With only the root package, `**` matches nothing and is reported.
        assert_eq!(
            resolved.warnings,
            vec![
                "`boundaries.packageTags` glob `**` does not match any package directory"
                    .to_string()
            ]
        );
    }

    #[test]
    fn unions_and_deduplicates_with_package_turbo_json_tags() {
        let config = package_tags_config(r#"{"packages/*": ["library", "shared"]}"#);
        let mut provider = MockTurboJson::default();
        provider.tags.insert(
            PackageName::Other("ui".into()),
            Spanned::new(vec![
                Spanned::new("react".into()),
                Spanned::new("library".into()),
            ]),
        );
        let resolved = resolve(&provider, Some(&config), &[("ui", "packages/ui")]);

        assert_eq!(
            tags_of(&resolved.index, "ui"),
            Some(vec!["react".into(), "library".into(), "shared".into()])
        );
        // A tag declared in the package's own turbo.json keeps pointing there.
        let tags = resolved
            .index
            .get(&PackageName::Other("ui".into()))
            .unwrap();
        assert_eq!(tags.as_inner()[1].path, None);
    }

    #[test]
    fn central_tags_point_at_root_turbo_json() {
        let config = package_tags_config(r#"{"packages/*": ["library"]}"#);
        let resolved = resolve(
            &MockTurboJson::default(),
            Some(&config),
            &[("ui", "packages/ui")],
        );

        let tags = resolved
            .index
            .get(&PackageName::Other("ui".into()))
            .unwrap();
        let (span, text) = tags.as_inner()[0].span_and_text("turbo.json");
        let span = span.unwrap();
        assert_eq!(text.name(), "turbo.json");
        assert_eq!(
            &text.inner()[span.offset()..span.offset() + span.len()],
            "\"library\""
        );
        let (list_span, _) = tags.span_and_text("turbo.json");
        assert!(list_span.is_some());
    }

    #[test]
    fn package_without_tag_sources_has_no_entry() {
        let resolved = resolve(&MockTurboJson::default(), None, &[("ui", "packages/ui")]);
        assert_eq!(tags_of(&resolved.index, "ui"), None);
        assert!(resolved.warnings.is_empty());
    }

    #[test]
    fn package_with_empty_own_tags_keeps_its_entry() {
        let mut provider = MockTurboJson::default();
        provider
            .tags
            .insert(PackageName::Other("ui".into()), Spanned::new(Vec::new()));
        let resolved = resolve(&provider, None, &[("ui", "packages/ui")]);
        assert_eq!(tags_of(&resolved.index, "ui"), Some(Vec::new()));
    }

    #[test]
    fn accepts_leading_dot_slash_and_trailing_slash() {
        let config = package_tags_config(r#"{"./apps/*/": ["app"]}"#);
        let resolved = resolve(
            &MockTurboJson::default(),
            Some(&config),
            &[("web", "apps/web")],
        );
        assert_eq!(tags_of(&resolved.index, "web"), Some(vec!["app".into()]));
    }

    #[test]
    fn reports_invalid_globs() {
        let config = package_tags_config(r#"{"apps/[": ["app"], "!apps/*": ["app"], "": ["x"]}"#);
        let resolved = resolve(
            &MockTurboJson::default(),
            Some(&config),
            &[("web", "apps/web")],
        );

        let invalid: Vec<_> = resolved
            .diagnostics
            .iter()
            .map(|diagnostic| match diagnostic {
                BoundariesDiagnostic::InvalidPackageTagsGlob { glob, span, .. } => {
                    assert!(span.is_some());
                    glob.as_str()
                }
                other => panic!("unexpected diagnostic {other:?}"),
            })
            .collect();
        assert_eq!(invalid, vec!["", "!apps/*", "apps/["]);
        // Invalid globs are reported once, not also as unmatched.
        assert!(resolved.warnings.is_empty());
        assert_eq!(tags_of(&resolved.index, "web"), None);
    }

    #[test]
    fn warns_about_globs_matching_no_package() {
        let config = package_tags_config(r#"{"apps/*": ["app"], "services/*": ["service"]}"#);
        let resolved = resolve(
            &MockTurboJson::default(),
            Some(&config),
            &[("web", "apps/web")],
        );
        assert_eq!(
            resolved.warnings,
            vec![
                "`boundaries.packageTags` glob `services/*` does not match any package directory"
                    .to_string()
            ]
        );
    }
}
