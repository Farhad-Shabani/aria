use crate::doctor::Status;

pub(super) fn run() -> anyhow::Result<()> {
    // The runtime directory must stay uncreated until the check for it runs,
    // so no daemon logging starts here.
    let report = crate::doctor::diagnose();
    let width = report
        .checks
        .iter()
        .map(|check| check.name.len())
        .max()
        .unwrap_or_default();

    for check in &report.checks {
        let status = match check.status {
            Status::Ok => "OK",
            Status::Warning => "WARN",
            Status::Failed => "FAIL",
        };
        println!(
            "{name:width$}  {status:4}  {detail}",
            name = check.name,
            detail = check.detail
        );
    }

    if report.has_failure() {
        std::process::exit(1);
    }
    Ok(())
}
