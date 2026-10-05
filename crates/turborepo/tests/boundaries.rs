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
