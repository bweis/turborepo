#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used))]

mod common;

#[test]
fn test_boundaries() -> Result<(), anyhow::Error> {
    check_json_output!(
        "boundaries",
        "npm@10.5.0",
        "query",
        "get boundaries lints" => ["query { boundaries { items { message import } } }"],
    );

    Ok(())
}

#[test]
fn test_boundaries_cli_shows_warnings() -> Result<(), anyhow::Error> {
    let tempdir = tempfile::tempdir()?;
    common::setup_fixture("boundaries", "npm@10.5.0", tempdir.path(), false)?;

    let output = common::turbo_command(tempdir.path())
        .arg("boundaries")
        .env("TURBO_CONFIG_DIR_PATH", tempdir.path())
        .env("NO_COLOR", "1")
        .output()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // `apps/my-app/index.ts` has imports marked with `@boundaries-ignore` on
    // lines 11, 16 and 20, which `turbo boundaries` reports as warnings naming
    // the (1-based) line of the import.
    let ignored_lines: Vec<&str> = stderr
        .lines()
        .filter(|line| line.contains("ignoring import on line"))
        .collect();
    assert_eq!(
        ignored_lines.len(),
        3,
        "stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    for (line, expected) in ignored_lines.iter().zip([11, 16, 20]) {
        assert!(
            line.contains(&format!("ignoring import on line {expected} in"))
                && line.contains("index.ts"),
            "expected line {expected} in {line:?}\nstderr:\n{stderr}"
        );
    }

    Ok(())
}

#[test]
fn test_boundaries_tags() -> Result<(), anyhow::Error> {
    check_json_output!(
        "boundaries_tags",
        "npm@10.5.0",
        "query",
        "get boundaries lints" => ["query { boundaries { items { message import } } }"],
    );

    Ok(())
}

#[test]
fn test_boundaries_ignore() -> Result<(), anyhow::Error> {
    check_json_output!(
        "boundaries_ignore",
        "npm@10.5.0",
        "query",
        "get boundaries lints" => ["query { boundaries { items { message import } } }"],
    );

    Ok(())
}

#[test]
fn test_boundaries_deny_packages() -> Result<(), anyhow::Error> {
    check_json_output!(
        "boundaries_deny_packages",
        "npm@10.5.0",
        "query",
        "get boundaries lints" => ["query { boundaries { items { message import reason } } }"],
    );

    Ok(())
}

/// A `browser`-tagged app that depends on `pg` through workspace packages
/// (`@repo/web` -> `@repo/ui` -> `@repo/db` -> `pg`) must fail `turbo
/// boundaries` when the `browser` tag rule denies `pg`. Without
/// `denyPackages` support, rules can't refer to npm packages and this check
/// passes.
#[test]
fn test_boundaries_deny_packages_fails_on_transitive_npm_dependency() -> Result<(), anyhow::Error> {
    let tempdir = tempfile::tempdir()?;
    common::setup_fixture(
        "boundaries_deny_packages_transitive",
        "npm@10.5.0",
        tempdir.path(),
        false,
    )?;

    let output = common::turbo_command(tempdir.path())
        .arg("boundaries")
        .env("TURBO_CONFIG_DIR_PATH", tempdir.path())
        .output()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(
        output.status.code(),
        Some(1),
        "expected `turbo boundaries` to fail\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.contains("1 issue found"),
        "expected exactly one issue\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    // Diagnostics are wrapped to the terminal width with a `|` gutter, so
    // compare with whitespace and gutters removed.
    let normalize = |text: &str| -> String {
        text.chars()
            .filter(|c| !c.is_whitespace() && *c != '|')
            .collect()
    };
    assert!(
        normalize(&stderr).contains(&normalize(
            "Package `@repo/web` depends on denied package `pg` (declared in `dependencies` of \
             `@repo/db`)"
        )),
        "expected a denied `pg` diagnostic\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    Ok(())
}

#[test]
fn test_boundaries_on_basic_monorepo() -> Result<(), anyhow::Error> {
    check_json_output!(
        "basic_monorepo",
        "npm@10.5.0",
        "query",
        "get boundaries lints" => ["query { boundaries { items { message import } } }"],
    );

    Ok(())
}

#[test]
fn test_boundaries_circular() -> Result<(), anyhow::Error> {
    check_json_output!(
        "boundaries_circular",
        "npm@10.5.0",
        "query",
        "get boundaries lints" => ["query { boundaries { items { message import } } }"],
    );

    Ok(())
}

#[test]
fn test_boundaries_import_checks_disabled() -> Result<(), anyhow::Error> {
    check_json_output!(
        "boundaries_import_checks_disabled",
        "npm@10.5.0",
        "query",
        "get boundaries lints" => ["query { boundaries { items { message import } } }"],
    );

    Ok(())
}

#[test]
fn test_boundaries_import_checks_disabled_summary() -> Result<(), anyhow::Error> {
    let tempdir = tempfile::tempdir()?;
    common::setup_fixture(
        "boundaries_import_checks_disabled",
        "npm@10.5.0",
        tempdir.path(),
        false,
    )?;

    let output = common::run_turbo(tempdir.path(), &["boundaries"]);
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert_eq!(
        output.status.code(),
        Some(1),
        "{}",
        common::combined_output(&output)
    );
    assert!(
        stdout.contains("Checked 3 packages (import checks disabled), 2 issues found"),
        "unexpected summary:\n{stdout}"
    );

    Ok(())
}

#[test]
fn test_boundaries_central_tags() -> Result<(), anyhow::Error> {
    check_json_output!(
        "boundaries_central_tags",
        "npm@10.5.0",
        "query",
        "get boundaries lints" => ["query { boundaries { items { message import path reason } } }"],
    );

    Ok(())
}

#[test]
fn test_boundaries_central_tags_cli_output() -> Result<(), anyhow::Error> {
    let tempdir = tempfile::tempdir()?;
    common::setup_fixture(
        "boundaries_central_tags",
        "npm@10.5.0",
        tempdir.path(),
        false,
    )?;

    let output = common::turbo_command(tempdir.path())
        .arg("boundaries")
        .env("TURBO_CONFIG_DIR_PATH", tempdir.path())
        .env("NO_COLOR", "1")
        .output()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(
        output.status.code(),
        Some(1),
        "stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.contains("Checked 0 files in 5 packages, 4 issues found"),
        "stdout:\n{stdout}"
    );
    // Centrally assigned tags are reported where they are assigned.
    assert!(
        stderr.contains(r#""packages/server/*": ["#) && stderr.contains("tag found here"),
        "stderr:\n{stderr}"
    );
    assert!(
        stderr.contains(
            "`boundaries.packageTags` glob `services/*` does not match any package directory"
        ),
        "stderr:\n{stderr}"
    );

    Ok(())
}

mod baseline {
    use std::{fs, path::Path};

    use super::common::{run_turbo, setup_fixture};

    const BASELINE: &str = "boundaries-baseline.json";
    const APP_INDEX: &str = "apps/my-app/index.ts";

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
        fs::write(&path, edit(contents)).unwrap();
    }

    fn squash(text: &str) -> String {
        text.chars()
            .filter(|c| !c.is_whitespace() && *c != '|')
            .collect()
    }

    fn read_baseline(dir: &Path, path: &str) -> String {
        fs::read_to_string(dir.join(path)).unwrap()
    }

    #[test]
    fn test_baseline_ratchet() -> Result<(), anyhow::Error> {
        let tempdir = tempfile::tempdir()?;
        let dir = tempdir.path();
        setup_fixture("boundaries", "npm@10.5.0", dir, false)?;

        // Without a baseline, the existing violations fail the check.
        assert_status(dir, &[], 1);
        assert!(!dir.join(BASELINE).exists());

        // Record them.
        let (stdout, _) = assert_status(dir, &["--update-baseline"], 0);
        assert!(
            stdout.contains("Updated boundaries-baseline.json with 14 violations"),
            "{stdout}"
        );
        let baseline = read_baseline(dir, BASELINE);
        insta::assert_snapshot!("boundaries_baseline_file", baseline);

        // Updating again is a no-op.
        assert_status(dir, &["--update-baseline"], 0);
        assert_eq!(read_baseline(dir, BASELINE), baseline);

        // Baselined violations are suppressed.
        let (stdout, _) = assert_status(dir, &[], 0);
        assert!(
            stdout.contains("no issues found (14 suppressed by baseline)"),
            "{stdout}"
        );

        // Unrelated edits that shift line numbers don't affect the baseline.
        edit(dir, APP_INDEX, |contents| {
            format!("// a new comment\n\n{contents}")
        });
        assert_status(dir, &[], 0);

        // A new violation fails, even if it is identical to a baselined one in
        // the same file.
        edit(dir, APP_INDEX, |contents| {
            format!("{contents}\nimport {{ data2 }} from \"utils/data\";\n")
        });
        let (stdout, stderr) = assert_status(dir, &[], 1);
        assert!(
            stdout.contains("1 issue found (14 suppressed by baseline)"),
            "{stdout}"
        );
        assert!(
            squash(&stderr).contains(&squash("cannot import package `utils`")),
            "{stderr}"
        );
        edit(dir, APP_INDEX, |contents| {
            contents.replace("\nimport { data2 } from \"utils/data\";\n", "")
        });
        assert_status(dir, &[], 0);

        // Fixing a violation without updating the baseline is an error, so
        // the violation can't silently be reintroduced.
        edit(dir, APP_INDEX, |contents| {
            contents.replace("import { data } from \"utils/data\";", "")
        });
        let (stdout, stderr) = assert_status(dir, &[], 1);
        assert!(
            stdout.contains("1 issue found (13 suppressed by baseline)"),
            "{stdout}"
        );
        // Diagnostics are wrapped to the terminal width, so compare without
        // whitespace.
        let squashed = squash(&stderr);
        assert!(
            squashed.contains(&squash(
                "Stale entry in boundaries baseline `boundaries-baseline.json` for package \
                 `my-app`: `package-not-found` in `apps/my-app/index.ts` for import `utils` \
                 (baselined: 1, found: 0)"
            )),
            "{stderr}"
        );
        assert!(
            squashed.contains(&squash("turbo boundaries --update-baseline")),
            "{stderr}"
        );

        // Updating the baseline removes the fixed violation.
        let (stdout, _) = assert_status(dir, &["--update-baseline"], 0);
        assert!(stdout.contains("with 13 violations"), "{stdout}");
        let baseline: serde_json::Value = serde_json::from_str(&read_baseline(dir, BASELINE))?;
        assert!(
            !baseline["violations"]["my-app"]
                .as_array()
                .unwrap()
                .iter()
                .any(|entry| entry["file"] == APP_INDEX && entry["import"] == "utils")
        );
        assert_status(dir, &[], 0);

        Ok(())
    }

    #[test]
    fn test_baseline_respects_filter() -> Result<(), anyhow::Error> {
        let tempdir = tempfile::tempdir()?;
        let dir = tempdir.path();
        setup_fixture("boundaries", "npm@10.5.0", dir, false)?;

        assert_status(dir, &["--update-baseline"], 0);
        let baseline = read_baseline(dir, BASELINE);

        // Fix a violation in `my-app`.
        edit(dir, APP_INDEX, |contents| {
            contents.replace("import { data } from \"utils/data\";", "")
        });

        // `my-app` isn't checked, so its entries are neither matched nor stale.
        let (stdout, _) = assert_status(dir, &["--filter=another"], 0);
        assert!(stdout.contains("no issues found"), "{stdout}");
        assert!(!stdout.contains("suppressed"), "{stdout}");

        // Updating the baseline for other packages preserves `my-app`'s
        // entries.
        assert_status(dir, &["--filter=another", "--update-baseline"], 0);
        assert_eq!(read_baseline(dir, BASELINE), baseline);

        // Once `my-app` is in scope, the fixed violation is stale.
        let (_, stderr) = assert_status(dir, &["--filter=my-app"], 1);
        assert!(stderr.contains("Stale entry"), "{stderr}");
        assert_status(dir, &["--filter=my-app", "--update-baseline"], 0);
        assert_status(dir, &[], 0);

        Ok(())
    }

    #[test]
    fn test_baseline_path_is_configurable() -> Result<(), anyhow::Error> {
        let tempdir = tempfile::tempdir()?;
        let dir = tempdir.path();
        setup_fixture("boundaries", "npm@10.5.0", dir, false)?;

        edit(dir, "turbo.json", |contents| {
            contents.replacen(
                "\"boundaries\": {",
                "\"boundaries\": {\n    \"baseline\": \"config/boundaries.json\",",
                1,
            )
        });

        let (stdout, _) = assert_status(dir, &["--update-baseline"], 0);
        assert!(
            stdout.contains("Updated config/boundaries.json"),
            "{stdout}"
        );
        assert!(dir.join("config/boundaries.json").exists());
        assert!(!dir.join(BASELINE).exists());
        assert_status(dir, &[], 0);

        // A malformed baseline is an error rather than being ignored.
        fs::write(dir.join("config/boundaries.json"), "{}")?;
        let (_, stderr) = assert_status(dir, &[], 1);
        assert!(
            squash(&stderr).contains(&squash("invalid boundaries baseline")),
            "{stderr}"
        );

        Ok(())
    }

    #[test]
    fn test_baseline_circular_dependencies() -> Result<(), anyhow::Error> {
        let tempdir = tempfile::tempdir()?;
        let dir = tempdir.path();
        setup_fixture("boundaries_circular", "npm@10.5.0", dir, false)?;

        assert_status(dir, &[], 1);
        assert_status(dir, &["--update-baseline"], 0);
        let baseline: serde_json::Value = serde_json::from_str(&read_baseline(dir, BASELINE))?;
        // Cycles are recorded under the root package, keyed by every package
        // in the cycle.
        assert_eq!(
            baseline["violations"]["//"],
            serde_json::json!([{
                "rule": "circular-dependency",
                "cycle": ["@repo/pkg-a", "@repo/pkg-b", "@repo/pkg-c"],
                "count": 1
            }]),
            "{baseline}"
        );

        // Cycles are checked regardless of filters, so their entries are
        // matched even when filtering.
        assert_status(dir, &[], 0);
        assert_status(dir, &["--filter=@repo/pkg-d"], 0);

        // A new package joining the existing cycle (pkg-a -> pkg-e -> pkg-a)
        // is a new violation, even if the reported cycle path is unchanged.
        fs::create_dir_all(dir.join("packages/pkg-e"))?;
        fs::write(
            dir.join("packages/pkg-e/package.json"),
            r#"{ "name": "@repo/pkg-e", "dependencies": { "@repo/pkg-a": "*" } }"#,
        )?;
        edit(dir, "packages/pkg-a/package.json", |contents| {
            contents.replace(
                r#""@repo/pkg-b": "*""#,
                r#""@repo/pkg-b": "*", "@repo/pkg-e": "*""#,
            )
        });
        let (_, stderr) = assert_status(dir, &[], 1);
        let squashed = squash(&stderr);
        assert!(
            squashed.contains(&squash("Circular package dependency detected")),
            "{stderr}"
        );
        assert!(squashed.contains(&squash("Stale entry")), "{stderr}");

        assert_status(dir, &["--update-baseline"], 0);
        let baseline: serde_json::Value = serde_json::from_str(&read_baseline(dir, BASELINE))?;
        assert_eq!(
            baseline["violations"]["//"][0]["cycle"],
            serde_json::json!(["@repo/pkg-a", "@repo/pkg-b", "@repo/pkg-c", "@repo/pkg-e"]),
            "{baseline}"
        );
        assert_status(dir, &[], 0);

        Ok(())
    }

    #[test]
    fn test_baseline_keeps_entries_for_unparseable_files() -> Result<(), anyhow::Error> {
        let tempdir = tempfile::tempdir()?;
        let dir = tempdir.path();
        setup_fixture("boundaries", "npm@10.5.0", dir, false)?;

        assert_status(dir, &["--update-baseline"], 0);
        let baseline = read_baseline(dir, BASELINE);

        // A syntax error means none of the file's imports are checked.
        edit(dir, APP_INDEX, |contents| {
            format!("{contents}\nconst = ;\n")
        });

        // Its baselined violations aren't reported as stale; only the parse
        // error fails the check.
        let (stdout, stderr) = assert_status(dir, &[], 1);
        assert!(stdout.contains("1 issue found"), "{stdout}");
        let squashed = squash(&stderr);
        assert!(
            squashed.contains(&squash("failed to parse file")),
            "{stderr}"
        );
        assert!(!squashed.contains(&squash("Stale entry")), "{stderr}");

        // Updating the baseline still fails on the parse error, and leaves the
        // file's entries untouched.
        let (stdout, _) = assert_status(dir, &["--update-baseline"], 1);
        assert!(
            stdout.contains("Kept existing entries for 1 file that could not be checked"),
            "{stdout}"
        );
        assert_eq!(read_baseline(dir, BASELINE), baseline);

        // Once the syntax error is fixed, the violations are still baselined.
        edit(dir, APP_INDEX, |contents| {
            contents.replace("\nconst = ;\n", "")
        });
        assert_status(dir, &[], 0);

        Ok(())
    }

    #[test]
    fn test_baseline_path_must_stay_in_repository() -> Result<(), anyhow::Error> {
        let tempdir = tempfile::tempdir()?;
        let dir = tempdir.path().join("repo");
        setup_fixture("boundaries", "npm@10.5.0", &dir, false)?;

        edit(&dir, "turbo.json", |contents| {
            contents.replacen(
                "\"boundaries\": {",
                "\"boundaries\": {\n    \"baseline\": \"../outside.json\",",
                1,
            )
        });

        let (_, stderr) = assert_status(&dir, &["--update-baseline"], 1);
        assert!(
            squash(&stderr).contains(&squash("invalid `boundaries.baseline` path")),
            "{stderr}"
        );
        assert!(!tempdir.path().join("outside.json").exists());

        Ok(())
    }

    #[test]
    fn test_update_baseline_conflicts_with_ignore() -> Result<(), anyhow::Error> {
        let tempdir = tempfile::tempdir()?;
        let dir = tempdir.path();
        setup_fixture("boundaries", "npm@10.5.0", dir, false)?;

        let (status, _, stderr) = boundaries(dir, &["--update-baseline", "--ignore=all"]);
        assert_ne!(status, 0);
        assert!(stderr.contains("cannot be used with"), "{stderr}");
        assert!(!dir.join(BASELINE).exists());

        Ok(())
    }
}
