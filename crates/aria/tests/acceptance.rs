use std::path::Path;

#[test]
fn catalog() -> anyhow::Result<()> {
    Ok(aria_harness::run_catalog(
        Path::new(env!("CARGO_BIN_EXE_aria")),
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/cases"),
    )?)
}
