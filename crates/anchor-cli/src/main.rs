use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;
use std::str::FromStr;

use anchor_core::{ChangeKind, ManifestDiff, NativeRelativePath, NativeString, PathEncoding};
use anchor_git::GitContext;
use anchor_session::{
    RestoreApplyResult, RestoreService, RunRequest, Session, SessionId, SessionRunner, SessionStore,
};
use clap::{Parser, Subcommand, ValueEnum};
use miette::{IntoDiagnostic as _, Result, WrapErr as _};
use serde::Serialize;

#[derive(Debug, Parser)]
#[command(
    name = "anchor",
    version,
    about = "Review filesystem changes observed during interactive command sessions"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Capture the worktree before and after an arbitrary interactive command.
    Run {
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<OsString>,
    },
    /// Show the most recent session for this worktree.
    Status {
        #[arg(long, value_enum, default_value_t)]
        format: OutputFormat,
    },
    /// List sessions retained for this worktree.
    Sessions {
        #[arg(long, value_enum, default_value_t)]
        format: OutputFormat,
    },
    /// Show one session's metadata.
    Show {
        session: String,
        #[arg(long, value_enum, default_value_t)]
        format: OutputFormat,
    },
    /// Compare the before and session-end manifests.
    Diff {
        session: String,
        #[arg(long, value_enum, default_value_t)]
        format: OutputFormat,
    },
    /// Remove one path's session-window change when it is provably safe.
    Restore {
        session: String,
        /// Worktree-root-relative path to restore.
        #[arg(long)]
        file: PathBuf,
    },
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
enum OutputFormat {
    #[default]
    Human,
    Json,
}

fn main() -> ExitCode {
    match execute(Cli::parse()) {
        Ok(code) => ExitCode::from(u8::try_from(code.clamp(0, 255)).unwrap_or(1)),
        Err(error) => {
            eprintln!("{error:?}");
            ExitCode::from(1)
        }
    }
}

fn execute(cli: Cli) -> Result<i32> {
    match cli.command {
        Commands::Run { command } => {
            let invocation_directory = std::env::current_dir()
                .into_diagnostic()
                .wrap_err("cannot read current directory")?;
            let result = SessionRunner::run(&RunRequest {
                invocation_directory,
                command,
                capture_options: anchor_core::CaptureOptions::default(),
            })
            .into_diagnostic()
            .wrap_err("session failed")?;
            eprintln!("Anchor session {}: {:?}", result.session_id, result.state);
            if let Some(failure) = &result.after_failure {
                eprintln!("after-snapshot failed: {failure}");
            }
            Ok(result.process_exit_code())
        }
        Commands::Status { format } => {
            let store = current_store()?;
            let sessions = store
                .list_sessions()
                .into_diagnostic()
                .wrap_err("cannot list sessions")?;
            if let Some(session) = sessions.first() {
                print_sessions(std::slice::from_ref(session), format)?;
                Ok(if session.state.is_terminal() { 0 } else { 3 })
            } else {
                if format == OutputFormat::Json {
                    println!("null");
                } else {
                    println!("No sessions in this worktree.");
                }
                Ok(0)
            }
        }
        Commands::Sessions { format } => {
            let store = current_store()?;
            let sessions = store
                .list_sessions()
                .into_diagnostic()
                .wrap_err("cannot list sessions")?;
            print_sessions(&sessions, format)?;
            Ok(0)
        }
        Commands::Show { session, format } => {
            let store = current_store()?;
            let session = load_session(&store, &session)?;
            print_sessions(std::slice::from_ref(&session), format)?;
            Ok(0)
        }
        Commands::Diff { session, format } => {
            let store = current_store()?;
            let session = load_session(&store, &session)?;
            let Some(after) = &session.after else {
                miette::bail!(
                    "session {} has no complete after-snapshot ({:?})",
                    session.id,
                    session.state
                );
            };
            let before = store
                .load_manifest(session.before.manifest)
                .into_diagnostic()
                .wrap_err("cannot load before manifest")?;
            let after = store
                .load_manifest(after.manifest)
                .into_diagnostic()
                .wrap_err("cannot load after manifest")?;
            let diff = ManifestDiff::between(&before, &after);
            print_diff(&diff, format)?;
            Ok(i32::from(!diff.is_empty()))
        }
        Commands::Restore { session, file } => {
            let store = current_store()?;
            let id = SessionId::from_str(&session)
                .into_diagnostic()
                .wrap_err("session ID is not a UUID")?;
            let path = NativeRelativePath::from_host_path(&file)
                .into_diagnostic()
                .wrap_err("--file must be a safe worktree-root-relative path")?;
            let result = RestoreService::restore_file(&store, id, path)
                .into_diagnostic()
                .wrap_err("restore was refused")?;
            match result {
                RestoreApplyResult::Applied { path, .. } => {
                    println!("restored {}", display_path(&path));
                    Ok(0)
                }
                RestoreApplyResult::NoChange { reason } => {
                    println!("no change: {reason:?}");
                    Ok(0)
                }
                RestoreApplyResult::Conflict { reason } => {
                    eprintln!("conflict: {reason:?}; no filesystem change was made");
                    Ok(4)
                }
            }
        }
    }
}

fn current_store() -> Result<SessionStore> {
    let current = std::env::current_dir()
        .into_diagnostic()
        .wrap_err("cannot read current directory")?;
    let context = GitContext::discover(&current)
        .into_diagnostic()
        .wrap_err("cannot discover a Git worktree")?;
    let location = context.store_location();
    SessionStore::open(location.root, location.worktree_key)
        .into_diagnostic()
        .wrap_err("cannot open Anchor storage")
}

fn load_session(store: &SessionStore, value: &str) -> Result<Session> {
    let id = SessionId::from_str(value)
        .into_diagnostic()
        .wrap_err("session ID is not a UUID")?;
    store
        .load_session(id)
        .into_diagnostic()
        .wrap_err("cannot load session")
}

fn print_sessions(sessions: &[Session], format: OutputFormat) -> Result<()> {
    if format == OutputFormat::Json {
        let records = sessions.iter().map(SessionJson::from).collect::<Vec<_>>();
        println!(
            "{}",
            serde_json::to_string_pretty(&records)
                .into_diagnostic()
                .wrap_err("cannot encode JSON output")?
        );
        return Ok(());
    }
    for session in sessions {
        let command = session
            .command
            .iter()
            .map(display_native)
            .collect::<Vec<_>>()
            .join(" ");
        println!(
            "{}  {:?}  {}  {}",
            session.id, session.state, session.started_at.seconds, command
        );
        if let Some(failure) = &session.failure {
            println!("  failure: {failure}");
        }
    }
    Ok(())
}

fn print_diff(diff: &ManifestDiff, format: OutputFormat) -> Result<()> {
    if format == OutputFormat::Json {
        let changes = diff
            .changes
            .iter()
            .map(|change| ChangeJson {
                status: change_name(change.kind),
                path: PathJson::from(&change.path),
                previous_path: change.previous_path.as_ref().map(PathJson::from),
            })
            .collect::<Vec<_>>();
        println!(
            "{}",
            serde_json::to_string_pretty(&changes)
                .into_diagnostic()
                .wrap_err("cannot encode JSON output")?
        );
        return Ok(());
    }
    for change in &diff.changes {
        if let Some(previous) = &change.previous_path {
            println!(
                "{}\t{} -> {}",
                change_symbol(change.kind),
                display_path(previous),
                display_path(&change.path)
            );
        } else {
            println!(
                "{}\t{}",
                change_symbol(change.kind),
                display_path(&change.path)
            );
        }
    }
    Ok(())
}

fn display_native(value: &NativeString) -> String {
    value.to_host().map_or_else(
        |_| "<foreign-native-string>".to_owned(),
        |value| value.to_string_lossy().into(),
    )
}

fn display_path(path: &NativeRelativePath) -> String {
    path.to_host_path().map_or_else(
        |_| "<foreign-path>".to_owned(),
        |value| value.to_string_lossy().into(),
    )
}

fn change_symbol(kind: ChangeKind) -> &'static str {
    match kind {
        ChangeKind::Added => "A",
        ChangeKind::Removed => "D",
        ChangeKind::Modified => "M",
        ChangeKind::TypeChanged => "T",
        ChangeKind::ModeChanged => "P",
        ChangeKind::SymlinkTargetChanged => "L",
        ChangeKind::Renamed => "R",
    }
}

fn change_name(kind: ChangeKind) -> &'static str {
    match kind {
        ChangeKind::Added => "added",
        ChangeKind::Removed => "removed",
        ChangeKind::Modified => "modified",
        ChangeKind::TypeChanged => "type-changed",
        ChangeKind::ModeChanged => "mode-changed",
        ChangeKind::SymlinkTargetChanged => "symlink-target-changed",
        ChangeKind::Renamed => "renamed",
    }
}

#[derive(Serialize)]
struct SessionJson {
    id: String,
    state: String,
    command: Vec<NativeStringJson>,
    worktree: NativeStringJson,
    before_manifest: String,
    after_manifest: Option<String>,
    exit_code: Option<i32>,
    signal: Option<i32>,
    failure: Option<String>,
}

impl From<&Session> for SessionJson {
    fn from(session: &Session) -> Self {
        Self {
            id: session.id.to_string(),
            state: format!("{:?}", session.state),
            command: session.command.iter().map(NativeStringJson::from).collect(),
            worktree: NativeStringJson::from(&session.worktree_root),
            before_manifest: session.before.manifest.to_string(),
            after_manifest: session
                .after
                .as_ref()
                .map(|after| after.manifest.to_string()),
            exit_code: session.exit.and_then(|exit| exit.code),
            signal: session.exit.and_then(|exit| exit.signal),
            failure: session.failure.clone(),
        }
    }
}

#[derive(Serialize)]
struct ChangeJson {
    status: &'static str,
    path: PathJson,
    previous_path: Option<PathJson>,
}

#[derive(Serialize)]
struct PathJson {
    encoding: &'static str,
    components_hex: Vec<String>,
}

impl From<&NativeRelativePath> for PathJson {
    fn from(path: &NativeRelativePath) -> Self {
        Self {
            encoding: encoding_name(path.encoding()),
            components_hex: path
                .components()
                .iter()
                .map(|component| hex(component))
                .collect(),
        }
    }
}

#[derive(Serialize)]
struct NativeStringJson {
    encoding: &'static str,
    bytes_hex: String,
}

impl From<&NativeString> for NativeStringJson {
    fn from(value: &NativeString) -> Self {
        Self {
            encoding: encoding_name(value.encoding()),
            bytes_hex: hex(value.bytes()),
        }
    }
}

fn encoding_name(encoding: PathEncoding) -> &'static str {
    match encoding {
        PathEncoding::UnixBytes => "unix-bytes",
        PathEncoding::WindowsWtf16Le => "windows-wtf16le",
    }
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(
        String::with_capacity(bytes.len() * 2),
        |mut output, byte| {
            write!(output, "{byte:02x}").expect("writing to a string cannot fail");
            output
        },
    )
}
