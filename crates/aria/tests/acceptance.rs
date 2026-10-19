use std::path::Path;

fn main() {
    let tests = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    aria_harness::run_catalog(
        Path::new(env!("CARGO_BIN_EXE_aria")),
        &tests.join("cases"),
        &tests.join("fixtures"),
    )
    .expect("the catalog should load")
    .exit();
}
