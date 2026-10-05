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

    // `apps/my-app/index.ts` has an import marked with `@boundaries-ignore`,
    // which `turbo boundaries` reports as a warning.
    assert!(
        stderr.contains("ignoring import on line 10 in") && stderr.contains("index.ts"),
        "stdout:\n{stdout}\nstderr:\n{stderr}"
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
