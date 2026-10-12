//! Enrollment with a person watching a terminal.
//!
//! The daemon records; this prints what to do between samples. Both `aria
//! speaker enroll` and the offer at the end of `aria setup` walk the same
//! path, so the prompts read the same either way.

use std::io::Write;
use std::thread;

use crate::enrollment::{EnrollmentRequest, EnrollmentResult, EnrollmentStage};
use crate::ipc::{ClientError, DaemonEvent, EventStream};

use super::DaemonSession;

pub const DEFAULT_SAMPLES: u32 = 5;

/// Records the samples, prompting for each one, and answers with the stored
/// voice. Progress goes to stdout — this is the flow for someone sitting at
/// a terminal, and nothing else calls it.
pub fn enroll(
    session: &DaemonSession,
    request: EnrollmentRequest,
) -> Result<EnrollmentResult, ClientError> {
    // Subscribing before the call, so the prompt for the first sample is not
    // missed while the model loads. The thread is never joined: a request the
    // daemon refuses before it starts emits no terminal stage, and waiting for
    // one that is not coming would hang the command. Process exit ends it.
    let events = session.events()?;
    thread::spawn(move || prompt_through(events));

    session.enroll(request)
}

/// Prints what to do next as the daemon reports it, and returns once the
/// enrollment ends however it ends.
fn prompt_through(events: EventStream) {
    for event in events {
        let Ok(DaemonEvent::Enrollment(stage)) = event else {
            continue;
        };
        match stage {
            EnrollmentStage::Recording { index, of } => {
                print!("  say something ({index}/{of})... ");
                let _flushed = std::io::stdout().flush();
            }
            EnrollmentStage::SampleTaken { .. } => println!("heard"),
            EnrollmentStage::Enrolled | EnrollmentStage::Cancelled | EnrollmentStage::Failed(_) => {
                return;
            }
        }
    }
}
