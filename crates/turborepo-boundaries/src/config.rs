use std::{collections::BTreeMap, sync::Arc};

use biome_deserialize_macros::Deserializable;
use schemars::JsonSchema;
use serde::Serialize;
use ts_rs::TS;
use turborepo_errors::{Spanned, WithMetadata};

/// Configuration for `turbo boundaries`.
///
/// Allows users to restrict a package's dependencies and dependents.
#[derive(Serialize, Default, Debug, Clone, Deserializable, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[schemars(rename_all = "camelCase")]
#[ts(export)]
pub struct BoundariesConfig {
    /// The boundaries rules for tags.
    ///
    /// Restricts which packages can import a tag and which packages a tag can
    /// import.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub tags: Option<Spanned<RulesMap>>,

    /// Declares any implicit dependencies, i.e. any dependency not declared in
    /// a `package.json`.
    ///
    /// These can include dependencies automatically injected by a framework or
    /// a testing library.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub implicit_dependencies: Option<Spanned<Vec<Spanned<String>>>>,

    /// Rules for a package's dependencies.
    ///
    /// Restricts which packages this package can import.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub dependencies: Option<Spanned<Permissions>>,

    /// Rules for a package's dependents.
    ///
    /// Restricts which packages can import this package.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub dependents: Option<Spanned<Permissions>>,

    /// Globs for files that should be skipped by the import checks.
    ///
    /// Globs are relative to the package directory. In the root `turbo.json`
    /// they apply to every package, and in a package's `turbo.json` they apply
    /// to that package in addition to the root globs. Useful for generated
    /// files that cannot carry `@boundaries-ignore` comments.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub ignore: Option<Spanned<Vec<Spanned<String>>>>,
    /// Whether to check each package's source file imports.
    ///
    /// When `false`, imports are not checked for leaving the package, for
    /// referencing undeclared dependencies, or for missing `type` qualifiers on
    /// type declaration package imports. Tag rules and circular dependency
    /// detection still run. Only allowed in the root `turbo.json`.
    ///
    /// Defaults to `true`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub import_checks: Option<Spanned<bool>>,
}

impl BoundariesConfig {
    /// Returns whether import checks are enabled. Defaults to `true`.
    pub fn import_checks_enabled(&self) -> bool {
        self.import_checks
            .as_ref()
            .is_none_or(|import_checks| *import_checks.as_inner())
    }
}

/// A map of tag names to their boundary rules.
pub type RulesMap = BTreeMap<String, Spanned<Rule>>;

/// Boundary rules for a tag.
///
/// Restricts which packages a tag can import and which packages can import this
/// tag.
#[derive(Serialize, Default, Debug, Clone, Deserializable, PartialEq, JsonSchema, TS)]
#[schemars(rename = "TagRules")]
#[ts(export, rename = "TagRules")]
pub struct Rule {
    /// Rules for a tag's dependencies.
    ///
    /// Restricts which packages a tag can import.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub dependencies: Option<Spanned<Permissions>>,

    /// Rules for a tag's dependents.
    ///
    /// Restricts which packages can import this tag.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub dependents: Option<Spanned<Permissions>>,
}

/// Permission rules for boundaries.
#[derive(Serialize, Default, Debug, Clone, Deserializable, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[schemars(rename_all = "camelCase")]
#[ts(export)]
pub struct Permissions {
    /// Lists which tags are allowed.
    ///
    /// Any tag not included will be banned. If omitted, all tags are permitted.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub allow: Option<Spanned<Vec<Spanned<String>>>>,

    /// Lists which tags are banned.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub deny: Option<Spanned<Vec<Spanned<String>>>>,

    /// Lists external (npm) packages that are banned, by name or glob (e.g.
    /// `"pg"`, `"@aws-sdk/*"`, `"drizzle-*"`).
    ///
    /// Checked against the external dependencies declared in the
    /// `package.json` of the package and of each of its transitive workspace
    /// dependencies. Only valid in `dependencies` rules.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub deny_packages: Option<Spanned<Vec<Spanned<String>>>>,
}

impl WithMetadata for BoundariesConfig {
    fn add_text(&mut self, text: Arc<str>) {
        self.tags.add_text(text.clone());
        if let Some(tags) = &mut self.tags {
            for rule in tags.as_inner_mut().values_mut() {
                rule.add_text(text.clone());
                rule.value.add_text(text.clone());
            }
        }
        self.implicit_dependencies.add_text(text.clone());
        if let Some(implicit_dependencies) = &mut self.implicit_dependencies {
            for dep in implicit_dependencies.as_inner_mut() {
                dep.add_text(text.clone());
            }
        }
        self.ignore.add_text(text.clone());
        if let Some(ignore) = &mut self.ignore {
            for glob in ignore.as_inner_mut() {
                glob.add_text(text.clone());
            }
        }
        for permissions in [&mut self.dependencies, &mut self.dependents]
            .into_iter()
            .flatten()
        {
            permissions.add_text(text.clone());
            permissions.value.add_text(text.clone());
        }
        self.import_checks.add_text(text);
    }

    fn add_path(&mut self, path: Arc<str>) {
        self.tags.add_path(path.clone());
        if let Some(tags) = &mut self.tags {
            for rule in tags.as_inner_mut().values_mut() {
                rule.add_path(path.clone());
                rule.value.add_path(path.clone());
            }
        }
        self.implicit_dependencies.add_path(path.clone());
        if let Some(implicit_dependencies) = &mut self.implicit_dependencies {
            for dep in implicit_dependencies.as_inner_mut() {
                dep.add_path(path.clone());
            }
        }
        self.ignore.add_path(path.clone());
        if let Some(ignore) = &mut self.ignore {
            for glob in ignore.as_inner_mut() {
                glob.add_path(path.clone());
            }
        }
        for permissions in [&mut self.dependencies, &mut self.dependents]
            .into_iter()
            .flatten()
        {
            permissions.add_path(path.clone());
            permissions.value.add_path(path.clone());
        }
        self.import_checks.add_path(path);
    }
}

impl WithMetadata for Rule {
    fn add_text(&mut self, text: Arc<str>) {
        self.dependencies.add_text(text.clone());
        if let Some(dependencies) = &mut self.dependencies {
            dependencies.value.add_text(text.clone());
        }

        self.dependents.add_text(text.clone());
        if let Some(dependents) = &mut self.dependents {
            dependents.value.add_text(text.clone());
        }
    }

    fn add_path(&mut self, path: Arc<str>) {
        self.dependencies.add_path(path.clone());
        if let Some(dependencies) = &mut self.dependencies {
            dependencies.value.add_path(path.clone());
        }

        self.dependents.add_path(path.clone());
        if let Some(dependents) = &mut self.dependents {
            dependents.value.add_path(path);
        }
    }
}

impl WithMetadata for Permissions {
    fn add_text(&mut self, text: Arc<str>) {
        self.allow.add_text(text.clone());
        if let Some(allow) = &mut self.allow {
            allow.value.add_text(text.clone());
        }

        self.deny.add_text(text.clone());
        if let Some(deny) = &mut self.deny {
            deny.value.add_text(text.clone());
        }

        self.deny_packages.add_text(text.clone());
        if let Some(deny_packages) = &mut self.deny_packages {
            deny_packages.value.add_text(text);
        }
    }

    fn add_path(&mut self, path: Arc<str>) {
        self.allow.add_path(path.clone());
        if let Some(allow) = &mut self.allow {
            allow.value.add_path(path.clone());
        }

        self.deny.add_path(path.clone());
        if let Some(deny) = &mut self.deny {
            deny.value.add_path(path.clone());
        }

        self.deny_packages.add_path(path.clone());
        if let Some(deny_packages) = &mut self.deny_packages {
            deny_packages.value.add_path(path);
        }
    }
}
