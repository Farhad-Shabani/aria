use anyhow::Context;
use aria_daemon::guided::DEFAULT_SAMPLES;
use aria_daemon::{DaemonSession, EnrollmentRequest, Role, SpeakerName, guided};
use clap::{Subcommand, ValueEnum};

use super::runtime_paths;

#[derive(Subcommand)]
pub(super) enum SpeakerCommand {
    /// Record a few utterances and store the voice they agree on
    Enroll {
        /// The person's name, as transcripts should show it
        name: String,
        /// Owner or guest of this machine. Defaults to the role a replaced voice
        /// had, or to owner while nobody owns the machine
        #[arg(long, value_enum)]
        role: Option<CliRole>,
        /// How many utterances to record, 2 to 10
        #[arg(long, default_value_t = DEFAULT_SAMPLES)]
        samples: u32,
        /// Replace an existing profile of this name
        #[arg(long)]
        replace: bool,
    },
    /// Show who is enrolled
    List,
    /// Forget one voice
    Remove { name: String },
    /// Make one voice the owner; any other owner becomes a guest
    Owner { name: String },
}

#[derive(Clone, Copy, ValueEnum)]
pub(super) enum CliRole {
    Owner,
    Guest,
}

impl From<CliRole> for Role {
    fn from(role: CliRole) -> Self {
        match role {
            CliRole::Owner => Self::Owner,
            CliRole::Guest => Self::Guest,
        }
    }
}

pub(super) fn run(command: SpeakerCommand) -> anyhow::Result<()> {
    let paths = runtime_paths()?;
    // Enrollment records through the daemon's microphone, so it needs the
    // daemon that owns one; a temporary on-demand daemon has none.
    let session = DaemonSession::connect_at(&paths)?;
    match command {
        SpeakerCommand::Enroll {
            name,
            role,
            samples,
            replace,
        } => enroll(&session, name, role, samples, replace),
        SpeakerCommand::List => list_speakers(&session),
        SpeakerCommand::Remove { name } => remove_speaker(&session, name),
        SpeakerCommand::Owner { name } => set_owner(&session, name),
    }
}

fn enroll(
    session: &DaemonSession,
    name: String,
    role: Option<CliRole>,
    samples: u32,
    replace: bool,
) -> anyhow::Result<()> {
    let enrolled = guided::enroll(
        session,
        EnrollmentRequest {
            name: SpeakerName::new(name)?,
            role: role.map(Role::from),
            samples,
            replace,
        },
    )?;
    println!(
        "enrolled {} as {} (consistency {:.2})",
        enrolled.name, enrolled.role, enrolled.consistency
    );
    Ok(())
}

fn list_speakers(session: &DaemonSession) -> anyhow::Result<()> {
    let speakers = session.list_speakers()?;
    if speakers.is_empty() {
        println!("nobody is enrolled");
        println!("enroll with: aria speaker enroll <name>");
        return Ok(());
    }
    for speaker in speakers {
        println!(
            "  {} ({}) — enrolled {}",
            speaker.name,
            speaker.role,
            enrolled_on(speaker.enrolled_at)
        );
    }
    Ok(())
}

/// The day a profile was recorded, in UTC. A profile whose moment does not
/// convert reads as unknown rather than failing the listing: the name and
/// role are what the reader came for.
fn enrolled_on(seconds: u64) -> String {
    let format = time::macros::format_description!("[year]-[month]-[day]");
    i64::try_from(seconds)
        .ok()
        .and_then(|seconds| time::OffsetDateTime::from_unix_timestamp(seconds).ok())
        .and_then(|moment| moment.format(&format).ok())
        .unwrap_or_else(|| "unknown".to_owned())
}

fn remove_speaker(session: &DaemonSession, name: String) -> anyhow::Result<()> {
    let name = SpeakerName::new(name)?;
    session.remove_speaker(&name)?;
    println!("removed {name}");
    Ok(())
}

fn set_owner(session: &DaemonSession, name: String) -> anyhow::Result<()> {
    let name = SpeakerName::new(name)?;
    session.set_owner(&name)?;
    println!("{name} is now the owner");
    Ok(())
}
