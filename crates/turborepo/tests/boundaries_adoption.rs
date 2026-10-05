#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used))]
//! End-to-end scenario: adopting `turbo boundaries` in an existing repository.
//!
//! The `boundaries_adoption` fixture models a typical `apps/*` + `packages/*`
//! workspace where every boundaries feature is used at once:
//!
//! - Tags come from the directory layout via root `boundaries.packageTags`.
//!   Only `packages/storage` has its own `turbo.json`, adding `node` on top of
//!   the `library` tag it gets from `packages/*`.
//! - `browser` packages can't depend on `node` packages, nor on `pg` or
//!   `@aws-sdk/*`. `library` packages may only depend on other libraries.
//! - `apps/web` has a TanStack Router `routeTree.gen.ts` that imports across
//!   packages, skipped by the root `boundaries.ignore`.
//! - `apps/web/tsconfig.json` has path aliases into `packages/ui` (declared as
//!   a dependency) and `packages/utils` (not declared).
//! - `apps/api` has an undeclared `zod` import and a `@boundaries-ignore`
//!   import, which is reported as a warning.
//!
//! The existing violations are recorded in a baseline, after which only new
//! violations fail, and fixed violations must be removed from the baseline.

mod common;

use std::{fs, path::Path};

use common::{run_turbo, setup_fixture};

const FIXTURE: &str = "boundaries_adoption";
const BASELINE: &str = "boundaries-baseline.json";
const WEB_PACKAGE_JSON: &str = "apps/web/package.json";

fn boundaries(dir: &Path, args: &[&str]) -> (i32, String, String) {
    let args: Vec<&str> = std::iter::once("boundaries")
        .chain(args.iter().copied())
        .collect();
    let output = run_turbo(dir, &args);
    (
        output
            .status
            .code()
            .expect("turbo exited with a status code"),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn assert_status(dir: &Path, args: &[&str], expected: i32) -> (String, String) {
    let (status, stdout, stderr) = boundaries(dir, args);
    assert_eq!(
        status, expected,
        "turbo boundaries {args:?} exited with {status}, expected \
         {expected}\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    (stdout, stderr)
}

fn edit(dir: &Path, path: &str, edit: impl FnOnce(String) -> String) {
    let path = dir.join(path);
    let contents = fs::read_to_string(&path).unwrap();
    let edited = edit(contents.clone());
    assert_ne!(edited, contents, "edit to {} had no effect", path.display());
    fs::write(&path, edited).unwrap();
}

/// Diagnostics are wrapped to the terminal width, so compare them without
/// whitespace and the `|` gutter.
fn squash(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_whitespace() && *c != '|')
        .collect()
}

#[track_caller]
fn assert_reports(stderr: &str, message: &str) {
    assert!(
        squash(stderr).contains(&squash(message)),
        "expected `{message}` in:\n{stderr}"
    );
}

/// Adds a workspace dependency to `apps/web`'s `package.json`.
fn add_web_dependency(dir: &Path, package: &str) {
    edit(dir, WEB_PACKAGE_JSON, |contents| {
        contents.replacen(
            r#""dependencies": {"#,
            &format!("\"dependencies\": {{\n    \"{package}\": \"*\","),
            1,
        )
    });
}

#[test]
fn test_boundaries_adoption_scenario() -> Result<(), anyhow::Error> {
    let tempdir = tempfile::tempdir()?;
    let dir = tempdir.path();
    setup_fixture(FIXTURE, "npm@10.5.0", dir, false)?;

    // Step 1: the first run on the existing repository fails with six
    // violations. `routeTree.gen.ts` is ignored, so it is neither reported nor
    // counted: there are 11 source files, but only 10 are checked.
    let (stdout, stderr) = assert_status(dir, &[], 1);
    assert!(
        stdout.contains("Checked 10 files in 6 packages, 6 issues found"),
        "{stdout}"
    );
    assert!(!stderr.contains("routeTree.gen.ts"), "{stderr}");

    // `@repo/storage` gets `node` from its own turbo.json, which is where the
    // denied tag is reported. It keeps the `library` tag from the
    // `packages/*` glob, otherwise `@repo/ui` (a library) would also break the
    // `library` allowlist by depending on it.
    assert_reports(
        &stderr,
        "Package `@repo/storage` found with tag listed in denylist for `@repo/web`: `node`",
    );
    assert_reports(&stderr, r#""tags": ["node"]"#);
    assert!(
        !stderr.contains("found without any tag listed in allowlist"),
        "{stderr}"
    );

    // `denyPackages` follows workspace dependencies: `@repo/web` depends on
    // `@repo/ui`, which depends on `@repo/storage`, which declares
    // `@aws-sdk/client-s3`.
    assert_reports(
        &stderr,
        "Package `@repo/web` depends on denied package `@aws-sdk/client-s3` (declared in \
         `dependencies` of `@repo/storage`)",
    );
    assert_reports(&stderr, "denied by `@aws-sdk/*` in `denyPackages`");

    // The `@utils/*` alias resolves into `packages/utils`, which `apps/web`
    // doesn't depend on. The `@ui/*` alias into `packages/ui` is declared, so
    // it isn't reported (see the baseline snapshot below).
    assert_reports(
        &stderr,
        "cannot import package `@repo/utils` because it is not a dependency",
    );
    assert_reports(
        &stderr,
        "`@utils/date` is a tsconfig path alias that resolves into package `@repo/utils`",
    );

    // Plain undeclared imports are still reported, and `@boundaries-ignore`
    // comments are surfaced as warnings.
    assert_reports(
        &stderr,
        "cannot import package `zod` because it is not a dependency",
    );
    assert!(
        stderr.contains("ignoring import on line") && stderr.contains("apps/api/src/index.ts"),
        "{stderr}"
    );

    // Step 2: record the existing violations so the check can be enabled in
    // CI right away.
    let (stdout, _) = assert_status(dir, &["--update-baseline"], 0);
    assert!(
        stdout.contains("Updated boundaries-baseline.json with 6 violations"),
        "{stdout}"
    );
    insta::assert_snapshot!(
        "boundaries_adoption_baseline",
        fs::read_to_string(dir.join(BASELINE))?
    );

    // Step 3: with the baseline committed, the check passes.
    let (stdout, _) = assert_status(dir, &[], 0);
    assert!(
        stdout
            .contains("Checked 10 files in 6 packages, no issues found (6 suppressed by baseline)"),
        "{stdout}"
    );

    // Step 4: someone makes the web app depend on the database package. That
    // is a new violation of both browser rules, so the check fails. The `node`
    // tag comes from `packageTags`, so it is reported in the root turbo.json.
    add_web_dependency(dir, "@repo/db");
    let (stdout, stderr) = assert_status(dir, &[], 1);
    assert!(
        stdout.contains("2 issues found (6 suppressed by baseline)"),
        "{stdout}"
    );
    assert_reports(
        &stderr,
        "Package `@repo/db` found with tag listed in denylist for `@repo/web`: `node`",
    );
    assert_reports(&stderr, r#""packages/db": ["node"]"#);
    assert_reports(
        &stderr,
        "Package `@repo/web` depends on denied package `pg` (declared in `dependencies` of \
         `@repo/db`)",
    );
    assert_reports(
        &stderr,
        "`@repo/web` depends on `@repo/db`, which declares `pg`",
    );
    edit(dir, WEB_PACKAGE_JSON, |contents| {
        contents.replacen("\n    \"@repo/db\": \"*\",", "", 1)
    });
    assert_status(dir, &[], 0);

    // Step 5: the web app declares `@repo/utils`, fixing the baselined alias
    // violation. The baseline entry is now stale, which fails the check until
    // the baseline is ratcheted down.
    add_web_dependency(dir, "@repo/utils");
    let (stdout, stderr) = assert_status(dir, &[], 1);
    assert!(
        stdout.contains("1 issue found (5 suppressed by baseline)"),
        "{stdout}"
    );
    assert_reports(
        &stderr,
        "Stale entry in boundaries baseline `boundaries-baseline.json` for package `@repo/web`: \
         `package-not-found` in `apps/web/src/main.tsx` for import `@repo/utils` (baselined: 1, \
         found: 0)",
    );

    let (stdout, _) = assert_status(dir, &["--update-baseline"], 0);
    assert!(stdout.contains("with 5 violations"), "{stdout}");
    let baseline: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(dir.join(BASELINE))?)?;
    assert!(
        !baseline["violations"]["@repo/web"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["import"] == "@repo/utils"),
        "{baseline}"
    );
    let (stdout, _) = assert_status(dir, &[], 0);
    assert!(
        stdout.contains("no issues found (5 suppressed by baseline)"),
        "{stdout}"
    );

    // Step 6: the ignore glob is load-bearing. Without it, the generated route
    // tree's cross-package imports are new violations.
    edit(dir, "turbo.json", |contents| {
        contents.replacen(r#""ignore": ["**/routeTree.gen.ts"],"#, "", 1)
    });
    let (stdout, stderr) = assert_status(dir, &[], 1);
    assert!(
        stdout
            .contains("Checked 11 files in 6 packages, 2 issues found (5 suppressed by baseline)"),
        "{stdout}"
    );
    assert_reports(
        &stderr,
        "import `../../../packages/ui/src/routes/admin` leaves the package",
    );
    assert_reports(
        &stderr,
        "cannot import package `@repo/db` because it is not a dependency",
    );

    Ok(())
}

// Import rules aren't evaluated when import checks are disabled, so their
// baseline entries are treated like entries for packages excluded by
// `--filter`: neither matched nor stale, and preserved by `--update-baseline`.
// Previously they were reported as stale and dropped by `--update-baseline`,
// so re-enabling import checks reported them as new violations.
#[test]
fn test_boundaries_adoption_import_checks_disabled_keeps_import_baseline()
-> Result<(), anyhow::Error> {
    let tempdir = tempfile::tempdir()?;
    let dir = tempdir.path();
    setup_fixture(FIXTURE, "npm@10.5.0", dir, false)?;
    assert_status(dir, &["--update-baseline"], 0);
    let baseline = fs::read_to_string(dir.join(BASELINE))?;

    // Temporarily disable import checks, e.g. while migrating tsconfig paths.
    edit(dir, "turbo.json", |contents| {
        contents.replacen(
            r#""boundaries": {"#,
            "\"boundaries\": {\n    \"importChecks\": false,",
            1,
        )
    });
    let (stdout, stderr) = assert_status(dir, &[], 0);
    assert!(!stderr.contains("Stale entry"), "{stderr}");
    assert!(
        stdout.contains("no issues found (4 suppressed by baseline)"),
        "{stdout}"
    );
    assert_status(dir, &["--update-baseline"], 0);
    assert_eq!(fs::read_to_string(dir.join(BASELINE))?, baseline);

    // Re-enabling import checks finds the same, still baselined, violations.
    edit(dir, "turbo.json", |contents| {
        contents.replacen("\n    \"importChecks\": false,", "", 1)
    });
    assert_status(dir, &[], 0);

    Ok(())
}

// BUG (or not yet landed): `denyPackages` reports `devDependencies` of
// transitive workspace dependencies. Those are never installed for consumers,
// so only the package's own `devDependencies` should count.
//
// Repro (fixture `boundaries_adoption`): add `"devDependencies": { "pg": "^8"
// }` to `packages/utils/package.json` and `"@repo/utils": "*"` to the
// dependencies of `packages/ui/package.json`, then run `turbo boundaries`:
//   x Package `@repo/web` depends on denied package `pg` (declared in
//     `devDependencies` of `@repo/utils`)
// (and the same for `@repo/ui`). `deny_packages_reports_dev_dependencies_of_
// workspace_dependencies` in `turborepo-boundaries/src/tags.rs` documents the
// current behavior, and the docs say every dependency field of every workspace
// dependency is checked.
#[test]
fn test_boundaries_adoption_deny_packages_skips_transitive_dev_dependencies()
-> Result<(), anyhow::Error> {
    let tempdir = tempfile::tempdir()?;
    let dir = tempdir.path();
    setup_fixture(FIXTURE, "npm@10.5.0", dir, false)?;
    assert_status(dir, &["--update-baseline"], 0);

    // `@repo/utils` uses `pg` in its own tests only.
    edit(dir, "packages/utils/package.json", |contents| {
        contents.replacen(
            r#""name": "@repo/utils""#,
            "\"name\": \"@repo/utils\",\n  \"devDependencies\": {\n    \"pg\": \"^8.0.0\"\n  }",
            1,
        )
    });
    edit(dir, "packages/ui/package.json", |contents| {
        contents.replacen(
            r#""dependencies": {"#,
            "\"dependencies\": {\n    \"@repo/utils\": \"*\",",
            1,
        )
    });
    let (_, stderr) = assert_status(dir, &[], 0);
    assert!(!stderr.contains("denied package `pg`"), "{stderr}");

    // A browser package's own devDependencies are still denied.
    edit(dir, WEB_PACKAGE_JSON, |contents| {
        contents.replacen(
            r#""name": "@repo/web","#,
            "\"name\": \"@repo/web\",\n  \"devDependencies\": {\n    \"pg\": \"^8.0.0\"\n  },",
            1,
        )
    });
    let (_, stderr) = assert_status(dir, &[], 1);
    assert_reports(
        &stderr,
        "Package `@repo/web` depends on denied package `pg` (declared in `devDependencies` of \
         `@repo/web`)",
    );

    Ok(())
}

// BUG: the `@boundaries-ignore` warning, which `turbo boundaries` now shows,
// reports a 0-based line number. In `apps/api/src/index.ts` the ignored import
// is on line 5 (the comment is on line 4), but the warning says
// `ignoring import on line 4`. The existing
// `test_boundaries_cli_shows_warnings` asserts the same off-by-one (`line 10`
// for an import on line 11).
#[test]
fn test_boundaries_adoption_ignore_warning_line_number() -> Result<(), anyhow::Error> {
    let tempdir = tempfile::tempdir()?;
    let dir = tempdir.path();
    setup_fixture(FIXTURE, "npm@10.5.0", dir, false)?;

    let (_, stderr) = assert_status(dir, &[], 1);
    assert!(stderr.contains("ignoring import on line 5 in"), "{stderr}");

    Ok(())
}
