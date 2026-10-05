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
