use std::ffi::OsString;
use std::io;
use std::path::PathBuf;
use std::process::ExitCode;
use std::str::FromStr;

use anchor_core::{
    ChangeKind, ManifestChange, ManifestDiff, ManifestId, ManifestNode, NativeRelativePath,
    NativeString, ObjectId, ObjectStore, PathEncoding,
};
use anchor_git::{GitContext, PolicyDrift};
use anchor_session::{
    CapturePolicy, ConfigLoader, IndexRestoreResult, MaintenanceService, PolicyOverrides,
    RecoveryService, RestoreApplyResult, RestoreService, RunRequest, Session, SessionId,
    SessionInspection, SessionRunner, SessionStore, TextMergeMode, TransactionRecoveryService,
    WholeRestoreMode, WholeRestoreResult,
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
        /// Permit a degraded snapshot when unsupported or unreadable paths are encountered.
        #[arg(long)]
        allow_degraded: bool,
        /// Traverse mount boundaries under the worktree.
        #[arg(long)]
        cross_mounts: bool,
        /// Retain all native command arguments in session metadata.
        #[arg(long)]
        record_arguments: bool,
        /// Override the maximum number of included manifest entries.
        #[arg(long = "max-entries", visible_alias = "max-files")]
        max_entries: Option<u64>,
        /// Override the maximum total raw bytes captured per endpoint.
        #[arg(long)]
        max_total_bytes: Option<u64>,
        /// Override the maximum raw bytes captured for one file.
        #[arg(long)]
        max_file_bytes: Option<u64>,
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
    /// List recoverably deleted sessions for this worktree.
    DeletedSessions {
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
        /// Compare the session's before-state with the current worktree.
        #[arg(long, conflicts_with = "drift")]
        current: bool,
        /// Compare the session-end state with the current worktree.
        #[arg(long, conflicts_with = "current")]
        drift: bool,
        #[arg(long, value_enum, default_value_t)]
        format: OutputFormat,
    },
    /// Open a read-only terminal reviewer for a completed session.
    Review { session: String },
    /// Remove one or all session-window changes when they are provably safe.
    Restore {
        session: String,
        /// Worktree-root-relative path to restore.
        #[arg(long, required_unless_present = "all", conflicts_with = "all")]
        file: Option<PathBuf>,
        /// Restore every included path as one recoverable batch.
        #[arg(long, conflicts_with_all = ["file", "merge", "expect_merged"])]
        all: bool,
        /// Attempt a bounded inverse three-way text merge when current content drifted.
        #[arg(long, requires = "file")]
        merge: bool,
        /// Confirm filesystem mutation after reviewing the relevant diff.
        #[arg(long)]
        yes: bool,
        /// Require the recalculated merge to match this previewed BLAKE3 object ID.
        #[arg(long, requires_all = ["merge", "yes"])]
        expect_merged: Option<String>,
        /// Require the current worktree to match this whole-restore preview manifest.
        #[arg(long, requires_all = ["all", "yes"])]
        expect_current: Option<String>,
        #[arg(long, value_enum, default_value_t)]
        format: OutputFormat,
    },
    /// Preview or restore all unambiguous included worktree paths for a session.
    Rollback {
        session: String,
        /// Confirm the batch using the manifest ID returned by a fresh preview.
        #[arg(long)]
        yes: bool,
        /// Require the current worktree to match this preview manifest.
        #[arg(long, requires = "yes")]
        expect_current: Option<String>,
        #[arg(long, value_enum, default_value_t)]
        format: OutputFormat,
    },
    /// Restore exact raw index bytes if no post-session index drift exists.
    RestoreIndex {
        session: String,
        #[arg(long, required = true)]
        yes: bool,
        #[arg(long, value_enum, default_value_t)]
        format: OutputFormat,
    },
    /// Verify retained sessions, manifests, objects, and repository drift.
    Doctor {
        #[arg(long, value_enum, default_value_t)]
        format: OutputFormat,
    },
    /// Remove immutable data unreachable from retained sessions.
    Gc {
        #[arg(long)]
        dry_run: bool,
        #[arg(long, value_enum, default_value_t)]
        format: OutputFormat,
    },
    /// Mark stale nonterminal sessions abandoned after proving their child lock is free.
    Recover {
        #[arg(long, value_enum, default_value_t)]
        format: OutputFormat,
    },
    /// Roll back interrupted schema-v3 restore transactions after byte verification.
    RecoverTransactions {
        /// Confirm filesystem or index recovery mutation.
        #[arg(long, required = true)]
        yes: bool,
        #[arg(long, value_enum, default_value_t)]
        format: OutputFormat,
    },
    /// Move a terminal session into recoverable local tombstone storage.
    Delete {
        session: String,
        #[arg(long, required = true)]
        yes: bool,
    },
    /// Restore a recoverably deleted session.
    Undelete { session: String },
    /// Permanently delete a tombstoned session record.
    Purge {
        session: String,
        #[arg(long, required = true)]
        yes: bool,
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

#[allow(clippy::too_many_lines)]
fn execute(cli: Cli) -> Result<i32> {
    match cli.command {
        Commands::Run {
            allow_degraded,
            cross_mounts,
            record_arguments,
            max_entries,
            max_total_bytes,
            max_file_bytes,
            command,
        } => {
            let invocation_directory = std::env::current_dir()
                .into_diagnostic()
                .wrap_err("cannot read current directory")?;
            let context = GitContext::discover(&invocation_directory)
                .into_diagnostic()
                .wrap_err("cannot discover a Git worktree")?;
            let resolution = ConfigLoader::load(context.worktree_root())
                .into_diagnostic()
                .wrap_err("cannot resolve Anchor configuration")?;
            let capture_policy = PolicyOverrides {
                max_entries,
                max_total_bytes,
                max_file_bytes,
                allow_degraded,
                cross_mounts,
                record_arguments,
            }
            .apply(resolution.policy)
            .into_diagnostic()
            .wrap_err("invalid capture policy")?;
            let result = SessionRunner::run(&RunRequest {
                invocation_directory,
                command,
                capture_policy,
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
            let _lease = store
                .acquire_store_read_lease()
                .into_diagnostic()
                .wrap_err("Anchor storage is busy")?;
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
            let _lease = store
                .acquire_store_read_lease()
                .into_diagnostic()
                .wrap_err("Anchor storage is busy")?;
            let sessions = store
                .list_sessions()
                .into_diagnostic()
                .wrap_err("cannot list sessions")?;
            print_sessions(&sessions, format)?;
            Ok(0)
        }
        Commands::DeletedSessions { format } => {
            let store = current_store()?;
            let _lease = store
                .acquire_store_read_lease()
                .into_diagnostic()
                .wrap_err("Anchor storage is busy")?;
            let sessions = store
                .list_deleted_sessions()
                .into_diagnostic()
                .wrap_err("cannot list deleted sessions")?;
            print_sessions(&sessions, format)?;
            Ok(0)
        }
        Commands::Show { session, format } => {
            let store = current_store()?;
            let _lease = store
                .acquire_store_read_lease()
                .into_diagnostic()
                .wrap_err("Anchor storage is busy")?;
            let session = load_session(&store, &session)?;
            print_sessions(std::slice::from_ref(&session), format)?;
            Ok(0)
        }
        Commands::Diff {
            session,
            current,
            drift,
            format,
        } => {
            let store = current_store()?;
            let _lease = store
                .acquire_store_read_lease()
                .into_diagnostic()
                .wrap_err("Anchor storage is busy")?;
            let session = load_session(&store, &session)?;
            let before = store
                .load_manifest(session.before.manifest)
                .into_diagnostic()
                .wrap_err("cannot load before manifest")?;
            let mut repository_drift = None;
            let mut index_drift = None;
            let current_snapshot = if current || drift {
                let snapshot = SessionInspection::capture_current(&store, session.id)
                    .into_diagnostic()
                    .wrap_err("cannot capture current worktree state")?;
                let reference = if drift {
                    session.after.as_ref().ok_or_else(|| {
                        miette::miette!(
                            "session {} has no complete after-snapshot ({:?})",
                            session.id,
                            session.state
                        )
                    })?
                } else {
                    &session.before
                };
                repository_drift = Some(snapshot.repository != reference.repository);
                index_drift = Some(snapshot.index != reference.index);
                Some(snapshot)
            } else {
                None
            };
            let (left, right, view) = if drift {
                let after = session.after.as_ref().ok_or_else(|| {
                    miette::miette!(
                        "session {} has no complete after-snapshot ({:?})",
                        session.id,
                        session.state
                    )
                })?;
                let after = store
                    .load_manifest(after.manifest)
                    .into_diagnostic()
                    .wrap_err("cannot load after manifest")?;
                (
                    after,
                    current_snapshot
                        .as_ref()
                        .ok_or_else(|| miette::miette!("current snapshot was not constructed"))?
                        .manifest
                        .clone(),
                    "drift",
                )
            } else if current {
                (
                    before.clone(),
                    current_snapshot
                        .as_ref()
                        .ok_or_else(|| miette::miette!("current snapshot was not constructed"))?
                        .manifest
                        .clone(),
                    "current",
                )
            } else {
                let after = session.after.as_ref().ok_or_else(|| {
                    miette::miette!(
                        "session {} has no complete after-snapshot ({:?})",
                        session.id,
                        session.state
                    )
                })?;
                (
                    before.clone(),
                    store
                        .load_manifest(after.manifest)
                        .into_diagnostic()
                        .wrap_err("cannot load after manifest")?,
                    "session-end",
                )
            };
            let diff = ManifestDiff::between(&left, &right);
            print_diff(
                &diff,
                format,
                store.objects(),
                view,
                repository_drift,
                index_drift,
            )?;
            Ok(i32::from(!diff.is_empty()))
        }
        Commands::Review { session } => {
            let store = current_store()?;
            let review_lease = store
                .acquire_store_read_lease()
                .into_diagnostic()
                .wrap_err("Anchor storage is busy")?;
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
            let model = review_model(session.id, &diff, store.objects())?;
            let action = anchor_tui::review(&model)
                .into_diagnostic()
                .wrap_err("terminal review failed")?;
            let anchor_tui::ReviewAction::RestoreSelected { index } = action else {
                return Ok(0);
            };
            let change = diff
                .changes
                .get(index)
                .ok_or_else(|| miette::miette!("review selection is out of range"))?;
            if change.kind == ChangeKind::Renamed {
                miette::bail!(
                    "a rename spans two paths and cannot be restored as one TUI file action"
                );
            }
            eprintln!(
                "Restore the exact safe inverse for {}? [y/N]",
                display_path(&change.path)
            );
            let mut answer = String::new();
            io::stdin()
                .read_line(&mut answer)
                .into_diagnostic()
                .wrap_err("cannot read confirmation")?;
            if !matches!(answer.trim(), "y" | "Y" | "yes" | "YES") {
                println!("no change made");
                return Ok(0);
            }
            drop(review_lease);
            let result = RestoreService::restore_file(&store, session.id, change.path.clone())
                .into_diagnostic()
                .wrap_err("restore was refused")?;
            report_restore_result(result, OutputFormat::Human)
        }
        Commands::Restore {
            session,
            file,
            all,
            merge,
            yes,
            expect_merged,
            expect_current,
            format,
        } => {
            if all {
                return execute_whole_restore(&session, yes, expect_current.as_deref(), format);
            }
            let store = current_store()?;
            let id = SessionId::from_str(&session)
                .into_diagnostic()
                .wrap_err("session ID is not a UUID")?;
            if !merge && !yes {
                if format == OutputFormat::Json {
                    print_json(&serde_json::json!({
                        "schema": 1,
                        "operation": "restore-file",
                        "status": "confirmation-required",
                        "session": session,
                    }))?;
                } else {
                    eprintln!(
                        "no change made; review `anchor diff {session}` and rerun this command with --yes"
                    );
                }
                return Ok(3);
            }
            if merge && yes && expect_merged.is_none() {
                miette::bail!("merged restoration requires --expect-merged from a fresh preview");
            }
            let path = NativeRelativePath::from_host_path(
                file.as_deref()
                    .ok_or_else(|| miette::miette!("one of --file or --all is required"))?,
            )
            .into_diagnostic()
            .wrap_err("--file must be a safe worktree-root-relative path")?;
            let merge_mode = if !merge {
                TextMergeMode::Disabled
            } else if yes {
                let expected = ObjectId::from_hex(
                    expect_merged
                        .as_deref()
                        .ok_or_else(|| miette::miette!("--yes requires --expect-merged"))?,
                )
                .into_diagnostic()
                .wrap_err("--expect-merged is not a BLAKE3 object ID")?;
                TextMergeMode::Apply {
                    expected_object: expected,
                }
            } else {
                TextMergeMode::Preview
            };
            let result = RestoreService::restore_file_with_merge(&store, id, path, merge_mode)
                .into_diagnostic()
                .wrap_err("restore was refused")?;
            if let RestoreApplyResult::TextMergeAvailable {
                path,
                current_object,
                current_raw_size,
                merged_object,
                merged_raw_size,
                ..
            } = result
            {
                if format == OutputFormat::Json {
                    print_json(&serde_json::json!({
                        "schema": 1,
                        "operation": "restore-file",
                        "status": "merge-preview",
                        "path": PathJson::from(&path),
                        "current_object": current_object.to_string(),
                        "current_raw_size": current_raw_size,
                        "merged_object": merged_object.to_string(),
                        "merged_raw_size": merged_raw_size,
                    }))?;
                    return Ok(3);
                }
                let current = store
                    .objects()
                    .get(current_object, current_raw_size)
                    .into_diagnostic()
                    .wrap_err("cannot load current merge input")?;
                let merged = store
                    .objects()
                    .get(merged_object, merged_raw_size)
                    .into_diagnostic()
                    .wrap_err("cannot load merged preview")?;
                print_text_pair(
                    &current,
                    &merged,
                    &format!("current/{}", display_path(&path)),
                    &format!("merged/{}", display_path(&path)),
                );
                eprintln!(
                    "clean merge preview only; rerun with --merge --yes --expect-merged {merged_object} to apply this exact result"
                );
                Ok(3)
            } else {
                report_restore_result(result, format)
            }
        }
        Commands::Rollback {
            session,
            yes,
            expect_current,
            format,
        } => execute_whole_restore(&session, yes, expect_current.as_deref(), format),
        Commands::RestoreIndex {
            session,
            yes,
            format,
        } => {
            if !yes {
                miette::bail!("index restoration requires --yes");
            }
            let store = current_store()?;
            let id = SessionId::from_str(&session)
                .into_diagnostic()
                .wrap_err("session ID is not a UUID")?;
            match RestoreService::restore_index(&store, id)
                .into_diagnostic()
                .wrap_err("index restore was refused")?
            {
                IndexRestoreResult::Applied => {
                    report_index_restore("applied", format)?;
                    Ok(0)
                }
                IndexRestoreResult::NoChange => {
                    report_index_restore("no-change", format)?;
                    Ok(0)
                }
                IndexRestoreResult::Conflict => {
                    if format == OutputFormat::Json {
                        report_index_restore("conflict", format)?;
                    } else {
                        eprintln!("index conflict: post-session index drift was preserved");
                    }
                    Ok(4)
                }
            }
        }
        Commands::Doctor { format } => {
            let (context, store) = current_context_and_store()?;
            let report = MaintenanceService::doctor(&store, &context)
                .into_diagnostic()
                .wrap_err("integrity verification failed")?;
            if format == OutputFormat::Json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "sessions": report.sessions,
                        "deleted_sessions": report.deleted_sessions,
                        "incomplete_sessions": report.incomplete_sessions,
                        "manifests_verified": report.manifests_verified,
                        "objects_verified": report.objects_verified,
                        "transactions": report.transactions,
                        "transactions_needing_recovery": report.transactions_needing_recovery,
                        "unfinished_transactions": report.unfinished_transactions,
                        "repository_drift_from_latest": report.repository_drift_from_latest,
                        "store_private": report.store_private,
                    }))
                    .into_diagnostic()?
                );
            } else {
                println!("sessions: {}", report.sessions);
                println!("deleted sessions: {}", report.deleted_sessions);
                println!("incomplete sessions: {}", report.incomplete_sessions);
                println!("manifests verified: {}", report.manifests_verified);
                println!("objects verified: {}", report.objects_verified);
                println!("restore transactions: {}", report.transactions);
                println!(
                    "transactions needing recovery: {}",
                    report.transactions_needing_recovery
                );
                println!(
                    "unfinished transactions: {}",
                    report.unfinished_transactions
                );
                println!(
                    "repository drift from latest: {}",
                    report.repository_drift_from_latest
                );
                println!("store private: {}", report.store_private);
            }
            Ok(i32::from(
                report.incomplete_sessions > 0
                    || report.transactions_needing_recovery > 0
                    || report.unfinished_transactions > 0
                    || report.repository_drift_from_latest
                    || !report.store_private,
            ))
        }
        Commands::Gc { dry_run, format } => {
            let store = current_store()?;
            let report = MaintenanceService::gc(&store, dry_run)
                .into_diagnostic()
                .wrap_err("garbage collection failed")?;
            if format == OutputFormat::Json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "dry_run": report.dry_run,
                        "manifests_removed": report.manifests_removed,
                        "objects_removed": report.objects_removed,
                        "bytes_reclaimed": report.bytes_reclaimed,
                    }))
                    .into_diagnostic()?
                );
            } else {
                let verb = if report.dry_run {
                    "would reclaim"
                } else {
                    "reclaimed"
                };
                println!(
                    "{verb} {} bytes ({} manifests, {} objects)",
                    report.bytes_reclaimed, report.manifests_removed, report.objects_removed
                );
            }
            Ok(0)
        }
        Commands::Recover { format } => {
            let store = current_store()?;
            let abandoned = RecoveryService::mark_abandoned(&store)
                .into_diagnostic()
                .wrap_err("cannot recover stale session metadata")?;
            if format == OutputFormat::Json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "abandoned_sessions": abandoned
                            .iter()
                            .map(ToString::to_string)
                            .collect::<Vec<_>>(),
                    }))
                    .into_diagnostic()?
                );
            } else if abandoned.is_empty() {
                println!("No stale nonterminal sessions.");
            } else {
                for session in &abandoned {
                    println!("marked {session} abandoned");
                }
            }
            Ok(0)
        }
        Commands::RecoverTransactions { yes, format } => {
            if !yes {
                miette::bail!("transaction recovery requires --yes");
            }
            let store = current_store()?;
            let report = TransactionRecoveryService::recover(&store)
                .into_diagnostic()
                .wrap_err("restore transaction recovery was refused")?;
            if format == OutputFormat::Json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "rolled_back": report.rolled_back,
                        "completed": report.completed,
                        "skipped_other_worktrees": report.skipped_other_worktrees,
                    }))
                    .into_diagnostic()?
                );
            } else {
                for transaction in &report.rolled_back {
                    println!("rolled back transaction {transaction}");
                }
                for transaction in &report.completed {
                    println!("completed committed transaction {transaction}");
                }
                if report.rolled_back.is_empty() && report.completed.is_empty() {
                    println!("No recoverable transactions for this worktree.");
                }
                if report.skipped_other_worktrees > 0 {
                    println!(
                        "skipped {} transaction(s) owned by linked worktrees",
                        report.skipped_other_worktrees
                    );
                }
            }
            Ok(0)
        }
        Commands::Delete { session, yes } => {
            if !yes {
                miette::bail!("session deletion requires --yes");
            }
            let store = current_store()?;
            let id = SessionId::from_str(&session)
                .into_diagnostic()
                .wrap_err("session ID is not a UUID")?;
            store
                .delete_session(id)
                .into_diagnostic()
                .wrap_err("cannot delete session")?;
            println!("deleted {id} recoverably; use `anchor undelete {id}` to restore it");
            Ok(0)
        }
        Commands::Undelete { session } => {
            let store = current_store()?;
            let id = SessionId::from_str(&session)
                .into_diagnostic()
                .wrap_err("session ID is not a UUID")?;
            store
                .undelete_session(id)
                .into_diagnostic()
                .wrap_err("cannot undelete session")?;
            println!("restored session {id}");
            Ok(0)
        }
        Commands::Purge { session, yes } => {
            if !yes {
                miette::bail!("permanent session purge requires --yes");
            }
            let store = current_store()?;
            let id = SessionId::from_str(&session)
                .into_diagnostic()
                .wrap_err("session ID is not a UUID")?;
            store
                .purge_deleted_session(id)
                .into_diagnostic()
                .wrap_err("cannot purge deleted session")?;
            println!(
                "permanently removed session record {id}; unreachable data is reclaimed by `anchor gc`"
            );
            Ok(0)
        }
    }
}

fn current_store() -> Result<SessionStore> {
    current_context_and_store().map(|(_, store)| store)
}

fn current_context_and_store() -> Result<(GitContext, SessionStore)> {
    let current = std::env::current_dir()
        .into_diagnostic()
        .wrap_err("cannot read current directory")?;
    let context = GitContext::discover(&current)
        .into_diagnostic()
        .wrap_err("cannot discover a Git worktree")?;
    let location = context.store_location();
    let store = SessionStore::open(location.root, location.worktree_key)
        .into_diagnostic()
        .wrap_err("cannot open Anchor storage")?;
    Ok((context, store))
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
        let redacted = if session.redacted_argument_count == 0 {
            String::new()
        } else {
            format!(
                " [+{} argument(s) not recorded]",
                session.redacted_argument_count
            )
        };
        println!(
            "{}  {:?}  {}  {}{}",
            session.id, session.state, session.started_at.seconds, command, redacted
        );
        if let Some(failure) = &session.failure {
            println!("  failure: {failure}");
        }
        if let Some(drift) = session
            .after
            .as_ref()
            .and_then(|endpoint| endpoint.policy_observation)
            .map(|observation| observation.drift)
            .filter(|drift| drift.any())
        {
            println!("  policy drift: {drift:?} (frozen scope remained in effect)");
        }
    }
    Ok(())
}

fn execute_whole_restore(
    session: &str,
    yes: bool,
    expect_current: Option<&str>,
    format: OutputFormat,
) -> Result<i32> {
    let store = current_store()?;
    let id = SessionId::from_str(session)
        .into_diagnostic()
        .wrap_err("session ID is not a UUID")?;
    let mode = if yes {
        let expected = ManifestId::from_hex(expect_current.ok_or_else(|| {
            miette::miette!("whole restoration requires --expect-current from a fresh preview")
        })?)
        .into_diagnostic()
        .wrap_err("--expect-current is not a BLAKE3 manifest ID")?;
        WholeRestoreMode::Apply {
            expected_current: expected,
        }
    } else {
        WholeRestoreMode::Preview
    };
    match RestoreService::restore_all(&store, id, mode)
        .into_diagnostic()
        .wrap_err("whole restore was refused")?
    {
        WholeRestoreResult::Preview {
            current_manifest,
            writes,
            no_changes,
        } => {
            if format == OutputFormat::Json {
                print_json(&serde_json::json!({
                    "schema": 1,
                    "operation": "rollback",
                    "status": "preview",
                    "session": session,
                    "current_manifest": current_manifest.to_string(),
                    "writes": writes,
                    "no_changes": no_changes,
                }))?;
            } else {
                println!(
                    "whole restore preview: {writes} path(s) would change; \
                     {no_changes} already safe/no-op"
                );
                eprintln!(
                    "no change made; apply this exact preview with \
                     `anchor rollback {session} --yes --expect-current {current_manifest}`"
                );
            }
            Ok(3)
        }
        WholeRestoreResult::Applied { paths } => {
            if format == OutputFormat::Json {
                print_json(&serde_json::json!({
                    "schema": 1,
                    "operation": "rollback",
                    "status": "applied",
                    "session": session,
                    "paths": paths,
                }))?;
            } else {
                println!("restored {paths} included path(s) as a verified batch");
            }
            Ok(0)
        }
        WholeRestoreResult::Conflicts { conflicts } => {
            if format == OutputFormat::Json {
                let conflicts = conflicts
                    .iter()
                    .map(|conflict| {
                        serde_json::json!({
                            "path": PathJson::from(&conflict.path),
                            "reason": format!("{:?}", conflict.reason),
                        })
                    })
                    .collect::<Vec<_>>();
                print_json(&serde_json::json!({
                    "schema": 1,
                    "operation": "rollback",
                    "status": "conflict",
                    "session": session,
                    "conflicts": conflicts,
                }))?;
            } else {
                for conflict in conflicts {
                    eprintln!(
                        "conflict {}: {:?}",
                        display_path(&conflict.path),
                        conflict.reason
                    );
                }
                eprintln!("no paths changed because the batch contains conflicts");
            }
            Ok(4)
        }
    }
}

fn report_restore_result(result: RestoreApplyResult, format: OutputFormat) -> Result<i32> {
    match result {
        RestoreApplyResult::Applied {
            session_id,
            path,
            merged,
        } => {
            if format == OutputFormat::Json {
                print_json(&serde_json::json!({
                    "schema": 1,
                    "operation": "restore-file",
                    "status": "applied",
                    "session": session_id.to_string(),
                    "path": PathJson::from(&path),
                    "merged": merged,
                }))?;
            } else {
                println!("restored {}", display_path(&path));
            }
            Ok(0)
        }
        RestoreApplyResult::NoChange { reason } => {
            if format == OutputFormat::Json {
                print_json(&serde_json::json!({
                    "schema": 1,
                    "operation": "restore-file",
                    "status": "no-change",
                    "reason": format!("{reason:?}"),
                }))?;
            } else {
                println!("no change: {reason:?}");
            }
            Ok(0)
        }
        RestoreApplyResult::Conflict { reason } => {
            if format == OutputFormat::Json {
                print_json(&serde_json::json!({
                    "schema": 1,
                    "operation": "restore-file",
                    "status": "conflict",
                    "reason": format!("{reason:?}"),
                }))?;
            } else {
                eprintln!("conflict: {reason:?}; no filesystem change was made");
            }
            Ok(4)
        }
        RestoreApplyResult::TextMergeAvailable { .. } => {
            miette::bail!("internal error: merge preview was not rendered")
        }
    }
}

fn report_index_restore(status: &'static str, format: OutputFormat) -> Result<()> {
    if format == OutputFormat::Json {
        return print_json(&serde_json::json!({
            "schema": 1,
            "operation": "restore-index",
            "status": status,
        }));
    }
    match status {
        "applied" => println!("restored exact pre-session index bytes"),
        "no-change" => println!("index already matches the safe target"),
        _ => {}
    }
    Ok(())
}

fn print_json(value: &serde_json::Value) -> Result<()> {
    println!(
        "{}",
        serde_json::to_string_pretty(value)
            .into_diagnostic()
            .wrap_err("cannot encode JSON output")?
    );
    Ok(())
}

fn print_diff(
    diff: &ManifestDiff,
    format: OutputFormat,
    objects: &ObjectStore,
    view: &'static str,
    repository_drift: Option<bool>,
    index_drift: Option<bool>,
) -> Result<()> {
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
            serde_json::to_string_pretty(&DiffJson {
                view,
                repository_drift,
                index_drift,
                changes,
            })
            .into_diagnostic()
            .wrap_err("cannot encode JSON output")?
        );
        return Ok(());
    }
    if repository_drift == Some(true) {
        eprintln!("warning: repository state differs from the selected comparison endpoint");
    }
    if index_drift == Some(true) {
        eprintln!("warning: Git index bytes differ from the selected comparison endpoint");
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
        print_content_diff(change, objects)?;
    }
    Ok(())
}

#[derive(Serialize)]
struct DiffJson {
    view: &'static str,
    repository_drift: Option<bool>,
    index_drift: Option<bool>,
    changes: Vec<ChangeJson>,
}

fn display_native(value: &NativeString) -> String {
    value.to_host().map_or_else(
        |_| "<foreign-native-string>".to_owned(),
        |value| escape_display(&value.to_string_lossy()),
    )
}

fn display_path(path: &NativeRelativePath) -> String {
    match path.encoding() {
        PathEncoding::UnixBytes => path
            .components()
            .iter()
            .map(|component| escape_bytes(component))
            .collect::<Vec<_>>()
            .join("/"),
        PathEncoding::WindowsWtf16Le => path.to_host_path().map_or_else(
            |_| "<foreign-path>".to_owned(),
            |value| escape_display(&value.to_string_lossy()),
        ),
    }
}

fn print_content_diff(change: &ManifestChange, objects: &ObjectStore) -> Result<()> {
    const MAX_TEXT_DIFF_BYTES: u64 = 8 * 1024 * 1024;
    if change.kind == ChangeKind::Renamed || change.kind == ChangeKind::ModeChanged {
        return Ok(());
    }
    let before = load_regular(change.before.as_ref(), objects, MAX_TEXT_DIFF_BYTES)?;
    let after = load_regular(change.after.as_ref(), objects, MAX_TEXT_DIFF_BYTES)?;
    let (Some(before), Some(after)) = (before, after) else {
        return Ok(());
    };
    if before.iter().chain(&after).any(|byte| *byte == 0)
        || std::str::from_utf8(&before).is_err()
        || std::str::from_utf8(&after).is_err()
    {
        println!("  Binary or opaque content differs");
        return Ok(());
    }
    let before = std::str::from_utf8(&before).expect("validated above");
    let after = std::str::from_utf8(&after).expect("validated above");
    let input = imara_diff::InternedInput::new(before, after);
    let mut content_diff = imara_diff::Diff::compute(imara_diff::Algorithm::Histogram, &input);
    content_diff.postprocess_lines(&input);
    let rendered = content_diff
        .unified_diff(
            &imara_diff::BasicLineDiffPrinter(&input.interner),
            imara_diff::UnifiedDiffConfig::default(),
            &input,
        )
        .to_string();
    if !rendered.is_empty() {
        println!("--- before/{}", display_path(&change.path));
        println!("+++ session/{}", display_path(&change.path));
        print!("{}", sanitize_diff_text(&rendered));
        if !rendered.ends_with('\n') {
            println!();
        }
    }
    Ok(())
}

fn print_text_pair(before: &[u8], after: &[u8], before_label: &str, after_label: &str) {
    let (Ok(before), Ok(after)) = (std::str::from_utf8(before), std::str::from_utf8(after)) else {
        println!("Binary or opaque content differs");
        return;
    };
    let input = imara_diff::InternedInput::new(before, after);
    let mut content_diff = imara_diff::Diff::compute(imara_diff::Algorithm::Histogram, &input);
    content_diff.postprocess_lines(&input);
    let rendered = content_diff
        .unified_diff(
            &imara_diff::BasicLineDiffPrinter(&input.interner),
            imara_diff::UnifiedDiffConfig::default(),
            &input,
        )
        .to_string();
    println!("--- {before_label}");
    println!("+++ {after_label}");
    print!("{}", sanitize_diff_text(&rendered));
    if !rendered.ends_with('\n') {
        println!();
    }
}

fn review_model(
    session_id: SessionId,
    diff: &ManifestDiff,
    objects: &ObjectStore,
) -> Result<anchor_tui::ReviewModel> {
    let files = diff
        .changes
        .iter()
        .map(|change| {
            Ok(anchor_tui::ReviewFile {
                path: display_path(&change.path),
                status: change_symbol(change.kind).to_owned(),
                before: review_content(change.before.as_ref(), objects)?,
                after: review_content(change.after.as_ref(), objects)?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(anchor_tui::ReviewModel {
        title: format!("Session {session_id}"),
        files,
    })
}

fn review_content(
    entry: Option<&anchor_core::ManifestEntry>,
    objects: &ObjectStore,
) -> Result<anchor_tui::ReviewContent> {
    const MAX_TUI_FILE_BYTES: u64 = 2 * 1024 * 1024;
    let Some(entry) = entry else {
        return Ok(anchor_tui::ReviewContent::Absent);
    };
    match &entry.node {
        ManifestNode::Regular {
            object, raw_size, ..
        } => {
            if *raw_size > MAX_TUI_FILE_BYTES {
                return Ok(anchor_tui::ReviewContent::Binary { size: *raw_size });
            }
            let bytes = objects
                .get(*object, *raw_size)
                .into_diagnostic()
                .wrap_err("cannot load review object")?;
            let Ok(text) = std::str::from_utf8(&bytes) else {
                return Ok(anchor_tui::ReviewContent::Binary { size: *raw_size });
            };
            if bytes.contains(&0) {
                return Ok(anchor_tui::ReviewContent::Binary { size: *raw_size });
            }
            Ok(anchor_tui::ReviewContent::Text(
                text.lines().map(sanitize_diff_text).collect(),
            ))
        }
        ManifestNode::Symlink { target, .. } => Ok(anchor_tui::ReviewContent::Description(
            format!("symlink → {}", display_native(target)),
        )),
        ManifestNode::EmptyDirectory => Ok(anchor_tui::ReviewContent::Description(
            "empty directory".to_owned(),
        )),
    }
}

fn load_regular(
    entry: Option<&anchor_core::ManifestEntry>,
    objects: &ObjectStore,
    maximum: u64,
) -> Result<Option<Vec<u8>>> {
    let Some(entry) = entry else {
        return Ok(Some(Vec::new()));
    };
    let ManifestNode::Regular {
        object, raw_size, ..
    } = entry.node
    else {
        return Ok(None);
    };
    if raw_size > maximum {
        return Ok(None);
    }
    objects
        .get(object, raw_size)
        .map(Some)
        .into_diagnostic()
        .wrap_err("cannot read diff object")
}

fn sanitize_diff_text(value: &str) -> String {
    value
        .chars()
        .flat_map(|character| match character {
            '\n' | '\t' => character.to_string().chars().collect::<Vec<_>>(),
            character if character.is_control() => character
                .escape_default()
                .collect::<String>()
                .chars()
                .collect(),
            character => vec![character],
        })
        .collect()
}

fn escape_display(value: &str) -> String {
    value
        .chars()
        .flat_map(char::escape_default)
        .collect::<String>()
}

fn escape_bytes(value: &[u8]) -> String {
    value.iter().fold(String::new(), |mut output, byte| {
        use std::fmt::Write as _;
        match byte {
            b' '..=b'~' if *byte != b'\\' => output.push(char::from(*byte)),
            b'\\' => output.push_str("\\\\"),
            _ => write!(output, "\\x{byte:02x}").expect("writing to a string cannot fail"),
        }
        output
    })
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
    redacted_argument_count: u64,
    capture_policy: CapturePolicy,
    worktree: NativeStringJson,
    frozen_policy: Option<String>,
    after_policy_drift: Option<PolicyDrift>,
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
            redacted_argument_count: session.redacted_argument_count,
            capture_policy: session.capture_policy,
            worktree: NativeStringJson::from(&session.worktree_root),
            frozen_policy: session.frozen_policy.map(|id| id.to_string()),
            after_policy_drift: session
                .after
                .as_ref()
                .and_then(|endpoint| endpoint.policy_observation)
                .map(|observation| observation.drift),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rollback_and_restore_all_accept_machine_readable_preview() {
        let rollback =
            Cli::try_parse_from(["anchor", "rollback", "session", "--format", "json"]).unwrap();
        assert!(matches!(
            rollback.command,
            Commands::Rollback {
                format: OutputFormat::Json,
                yes: false,
                ..
            }
        ));

        let restore =
            Cli::try_parse_from(["anchor", "restore", "session", "--all", "--format", "json"])
                .unwrap();
        assert!(matches!(
            restore.command,
            Commands::Restore {
                all: true,
                file: None,
                format: OutputFormat::Json,
                ..
            }
        ));
    }

    #[test]
    fn restore_requires_exactly_one_scope() {
        assert!(Cli::try_parse_from(["anchor", "restore", "session"]).is_err());
        assert!(
            Cli::try_parse_from(["anchor", "restore", "session", "--all", "--file", "path"])
                .is_err()
        );
    }
}
