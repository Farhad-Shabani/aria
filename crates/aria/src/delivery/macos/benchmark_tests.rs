use super::{DeliveryError, MacOsTextDelivery, OutputMode, TextDelivery};
use anyhow::Context;
use std::time::Duration;

#[path = "../../../benches/dictation_delivery/target.rs"]
mod target;

#[test]
#[ignore = "requires an idle desktop and event-posting permission on the test executable"]
fn native_streaming_target_receives_unicode_selection_and_rejects_focus_loss() -> anyhow::Result<()>
{
    anyhow::ensure!(
        objc2_core_graphics::CGPreflightPostEventAccess(),
        "grant Accessibility permission to the test executable before this native check"
    );
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let delivery = MacOsTextDelivery::new(Duration::from_secs(2))?;
    let mut target = target::Target::launch()?;
    target.reset("", 0, 0)?;
    let prepared = delivery.prepare(OutputMode::Insert, || false)?;
    let started_ns = target::uptime_ns()?;
    let first = "hé🙂 فارسی — a long Unicode segment";
    let second = " followed by another segment";
    delivery.deliver(&prepared, first, || false)?;
    delivery.deliver(&prepared, second, || false)?;
    runtime
        .block_on(target.finish(&format!("{first}{second}"), started_ns))
        .context("Unicode insertion trial")?;

    target.reset("🙂 OLD suffix", 3, 3)?;
    let prepared = delivery.prepare(OutputMode::Insert, || false)?;
    let started_ns = target::uptime_ns()?;
    delivery.deliver(&prepared, "hé🙂", || false)?;
    runtime
        .block_on(target.finish("🙂 hé🙂 suffix", started_ns))
        .context("selection replacement trial")?;

    let mut other = target::Target::launch()?;
    other.reset("", 0, 0)?;
    assert!(matches!(
        delivery.deliver(&prepared, "must not arrive", || false),
        Err(DeliveryError::TargetChanged)
    ));
    let snapshot = target.snapshot()?;
    assert!(snapshot.focus_lost);
    assert_eq!(snapshot.text, "🙂 hé🙂 suffix");
    assert!(other.snapshot()?.text.is_empty());
    other.stop()?;
    target.reset("", 0, 0)?;
    assert!(target.snapshot()?.text.is_empty());
    target.stop()
}
