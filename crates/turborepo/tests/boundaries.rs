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
