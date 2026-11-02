use crate::supervisor::{DaemonState, Job, Status as ServiceStatus, Supervisor};
use anyhow::Context;
use clap::Subcommand;

#[derive(Subcommand, Clone, Copy)]
pub(super) enum ServiceCommand {
    /// Register the daemon with launchd or systemd and start it
    Install,
    /// Stop the daemon and remove the registration
    Uninstall,
    /// Start the registered daemon
    Start,
    /// Stop the registered daemon; it comes back at next login
    Stop,
    /// Report the registration and whether the daemon answers
    Status,
}

pub(super) fn run(command: ServiceCommand) -> anyhow::Result<()> {
    let supervisor = Supervisor::open().context("failed to prepare the service")?;
    let status = match command {
        ServiceCommand::Install => supervisor.install()?,
        ServiceCommand::Uninstall => supervisor.uninstall()?,
        ServiceCommand::Start => supervisor.start()?,
        ServiceCommand::Stop => supervisor.stop()?,
        ServiceCommand::Status => supervisor.status()?,
    };
    println!("{}", service_status_message(&status));
    let inspect_only = matches!(command, ServiceCommand::Status);
    if inspect_only && !matches!(status.daemon, DaemonState::Ready { .. }) {
        std::process::exit(1);
    }
    Ok(())
}

pub(super) fn service_status_message(status: &ServiceStatus) -> String {
    let job = match status.job {
        Job::Absent => "not registered".to_owned(),
        Job::Idle => "registered, idle".to_owned(),
        Job::Running(pid) => format!("registered, pid {pid}"),
    };
    let daemon = match status.daemon {
        DaemonState::Ready { pid } => {
            format!("ready{}, {}", pid_suffix(pid), status.socket.display())
        }
        DaemonState::Starting { pid } => format!("starting{}", pid_suffix(pid)),
        DaemonState::NotRunning => match status.job {
            Job::Absent => "stopped".to_owned(),
            Job::Idle | Job::Running(_) => {
                "stopped; run `aria service start` or open Aria".to_owned()
            }
        },
    };
    let unit = if status.app_agent {
        "Aria.app's login agent".to_owned()
    } else {
        status.unit.display().to_string()
    };
    format!("unit     {unit} ({job})\ndaemon   {daemon}")
}

fn pid_suffix(pid: Option<u32>) -> String {
    pid.map(|pid| format!(", pid {pid}")).unwrap_or_default()
}
