//! Running the service manager's commands and waiting on their effects.

use std::{path::Path, time::Duration};

use super::{Commands, probe, shell};
use crate::cli::say;

/// How often `install` asks launchd to load the service while the old gateway
/// finishes its requests.
const RETRY_EVERY: Duration = Duration::from_millis(500);

/// Runs each command, printing it first. A `bootout` of a service that is not
/// loaded is not an error. launchd reports `bootout` done while the old gateway
/// still finishes its requests, and refuses `bootstrap` until it has exited, so
/// that one is retried for as long as the old gateway may take: `settle`.
pub(super) fn run_all(
    commands: &Commands,
    show: &dyn Fn(&Path) -> String,
    settle: Duration,
) -> Result<(), String> {
    for command in commands {
        say!("  $ {}", shell(command, show));
        let (program, args) = command.split_first().ok_or("an empty command")?;
        let verb = args.first().map(String::as_str);
        let run = || {
            let output = std::process::Command::new(program)
                .args(args)
                .output()
                .map_err(|e| format!("cannot run {program}: {e}"))?;
            if output.status.success() {
                Ok(())
            } else {
                Err(String::from_utf8_lossy(&output.stderr).trim().to_owned())
            }
        };
        let result = if verb == Some("bootstrap") {
            retry(run, settle, RETRY_EVERY)
        } else {
            run()
        };
        if let Err(why) = result
            && verb != Some("bootout")
        {
            return Err(format!("{} failed: {why}", shell(command, show)));
        }
    }
    Ok(())
}

/// `attempt` until it succeeds or `settle` has passed; the last error then.
pub(super) fn retry(
    mut attempt: impl FnMut() -> Result<(), String>,
    settle: Duration,
    every: Duration,
) -> Result<(), String> {
    let deadline = std::time::Instant::now() + settle;
    loop {
        match attempt() {
            Ok(()) => return Ok(()),
            Err(_) if std::time::Instant::now() < deadline => std::thread::sleep(every),
            Err(why) => return Err(why),
        }
    }
}

/// Waits up to `wait` for the gateway to answer, and says so; when it does
/// not, the end of the message log `messages` may say why.
pub(super) fn wait_for(
    url: &str,
    messages: &Path,
    show: &dyn Fn(&Path) -> String,
    wait: Duration,
) -> Result<(), String> {
    let deadline = std::time::Instant::now() + wait;
    while std::time::Instant::now() < deadline {
        if probe(url).is_ok() {
            say!("The gateway answers at {url}.");
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    Err(format!(
        "nothing answers at {url} yet; the end of {} may say why",
        show(messages)
    ))
}

/// The user id, for launchd's `gui/<uid>` domain.
pub(super) fn uid() -> Result<String, String> {
    let output = std::process::Command::new("id")
        .arg("-u")
        .output()
        .map_err(|e| format!("cannot run id -u: {e}"))?;
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// Writes `text` to `path` through a temporary file, so a half-written
/// definition is never loaded.
pub(super) fn write_atomically(path: &Path, text: &str) -> Result<(), String> {
    let temporary = path.with_extension("tmp");
    std::fs::write(&temporary, text)
        .and_then(|()| std::fs::rename(&temporary, path))
        .map_err(|e| format!("cannot write {}: {e}", path.display()))
}
