use std::collections::HashMap;

use dialoguer::{Confirm, Input};
use miette::{Report, SourceSpan};
use turbopath::AbsoluteSystemPath;
use turborepo_boundaries::{
    Baseline, BaselineScope, BoundariesChecker, BoundariesContext, ViolationKey,
};
use turborepo_run::{boundaries::RunTurboJsonProvider, builder::RunBuilder};
use turborepo_signals::{SignalHandler, listeners::get_signal};
use turborepo_telemetry::events::command::CommandEventBuilder;
use turborepo_ui::{BOLD_GREEN, LogSinks, color};

use crate::{cli, cli::BoundariesIgnore, commands::CommandBase};

pub async fn run(
    base: CommandBase,
    telemetry: CommandEventBuilder,
    ignore: Option<BoundariesIgnore>,
    reason: Option<String>,
    update_baseline: bool,
) -> Result<i32, cli::Error> {
    let signal = get_signal()?;
    let handler = SignalHandler::new(signal);

    // Boundaries warnings are emitted through the global logger, which has no
    // sinks until it is initialized.
    LogSinks::new(base.color_config).init_logger();

    let (run, _analytics) = RunBuilder::new(base.run_builder_input()?, None)?
        .do_not_validate_engine()
        .build(&handler, telemetry)
        .await?;

    let turbo_json_provider = RunTurboJsonProvider::new(run.turbo_json_loader());
    let root_boundaries_config = run
        .root_turbo_json()
        .boundaries
        .as_ref()
        .map(|spanned| spanned.as_inner());
    let ctx = BoundariesContext {
        repo_root: run.repo_root(),
        pkg_dep_graph: run.pkg_dep_graph(),
        turbo_json_provider: &turbo_json_provider,
        root_boundaries_config,
        filtered_pkgs: run.filtered_pkgs(),
    };
    let (baseline_path, baseline_display_path) =
        Baseline::path(run.repo_root(), root_boundaries_config)?;
    // Loading the baseline before checking means a malformed baseline is
    // reported without waiting for the whole check to finish.
    let baseline = Baseline::load(&baseline_path, &baseline_display_path)?;
    let mut scope = BaselineScope::from_context(&ctx);

    let mut result = BoundariesChecker::check_boundaries(&ctx, true)?;
    // Files that failed to parse may be missing violations, so their entries
    // are neither matched nor rewritten.
    let unchecked_files = scope.exclude_unchecked_files(run.repo_root(), &result.diagnostics);

    if update_baseline {
        let current = Baseline::from_diagnostics(run.repo_root(), &result.diagnostics);
        let baselined_count = current.len();
        // With `--filter`, entries for packages that weren't checked are kept.
        let updated = match &baseline {
            Some(baseline) => baseline.update(current, &scope),
            None => current,
        };
        // Don't create an empty baseline file in a repository without
        // violations, but do keep an existing one in sync.
        let write = baseline.is_some() || !updated.is_empty();
        if write {
            updated.write(&baseline_path)?;
        }

        // Diagnostics that can't be baselined still need to be fixed.
        result.diagnostics.retain(|diagnostic| {
            ViolationKey::from_diagnostic(run.repo_root(), diagnostic).is_none()
        });
        result.suppressed_by_baseline += baselined_count;
        result.emit(run.color_config());
        if write {
            println!(
                "{} {} with {} {}",
                color!(run.color_config(), BOLD_GREEN, "Updated"),
                baseline_display_path,
                updated.len(),
                if updated.len() == 1 {
                    "violation"
                } else {
                    "violations"
                }
            );
        }
        if unchecked_files > 0 && baseline.is_some() {
            // The parse errors above already fail the command; make it clear
            // that the baseline wasn't pruned for those files either.
            println!(
                "Kept existing entries for {unchecked_files} {} that could not be checked",
                if unchecked_files == 1 {
                    "file"
                } else {
                    "files"
                },
            );
        }
        return Ok(if result.is_ok() { 0 } else { 1 });
    }

    if let Some(baseline) = &baseline {
        baseline.apply(run.repo_root(), &baseline_display_path, &scope, &mut result);
    }

    if let Some(ignore) = ignore {
        let mut patches: HashMap<&AbsoluteSystemPath, Vec<(SourceSpan, String)>> = HashMap::new();
        for diagnostic in &result.diagnostics {
            let Some((path, span)) = diagnostic.path_and_span() else {
                continue;
            };

            let reason = match ignore {
                BoundariesIgnore::All => Some(reason.clone().unwrap_or_else(|| {
                    "automatically added by `turbo boundaries --ignore=all`".to_string()
                })),
                BoundariesIgnore::Prompt => {
                    print!("{esc}c", esc = 27 as char);
                    println!();
                    println!();
                    println!("{:?}", Report::new(diagnostic.clone()));
                    let prompt = format!(
                        "Ignore this error by adding a {} comment?",
                        color!(run.color_config(), BOLD_GREEN, "@boundaries-ignore"),
                    );
                    if Confirm::new()
                        .with_prompt(prompt)
                        .default(false)
                        .interact()?
                    {
                        if let Some(reason) = reason.clone() {
                            Some(reason)
                        } else {
                            Some(
                                Input::new()
                                    .with_prompt("Reason for ignoring this error")
                                    .interact_text()?,
                            )
                        }
                    } else {
                        None
                    }
                }
            };

            if let Some(reason) = reason {
                patches.entry(path).or_default().push((span, reason));
            }
        }

        for (path, file_patches) in patches {
            let short_path = match run.repo_root().anchor(path) {
                Ok(path) => path.to_string(),
                Err(_) => path.to_string(),
            };
            println!(
                "{} {}",
                color!(run.color_config(), BOLD_GREEN, "patching"),
                short_path
            );
            BoundariesChecker::patch_file(path, file_patches)?;
        }
    } else {
        result.emit(run.color_config());
    }

    if result.is_ok() { Ok(0) } else { Ok(1) }
}
