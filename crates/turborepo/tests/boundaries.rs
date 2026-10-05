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
