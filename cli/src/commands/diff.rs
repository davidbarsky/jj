// Copyright 2020 The Jujutsu Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use clap_complete::ArgValueCandidates;
use clap_complete::ArgValueCompleter;
use futures::TryStreamExt as _;
use futures::future::try_join_all;
use indexmap::IndexSet;
use itertools::Itertools as _;
use jj_lib::copies::CopyRecords;
use jj_lib::diff_selection::DiffSelection;
use jj_lib::diff_selection::DiffSelectionSource;
use jj_lib::diff_selection::SelectionAction;
use jj_lib::matchers::EverythingMatcher;
use jj_lib::matchers::Matcher;
use jj_lib::merge::Diff;
use jj_lib::repo::Repo as _;
use jj_lib::rewrite::merge_commit_trees;
use tracing::instrument;

use crate::cli_util::CommandHelper;
use crate::cli_util::HunkIdArgs;
use crate::cli_util::RevisionArg;
use crate::cli_util::print_unmatched_explicit_paths;
use crate::cli_util::short_commit_hash;
use crate::command_error::CommandError;
use crate::command_error::user_error;
use crate::complete;
use crate::diff_util::ColorWordsDiffOptions;
use crate::diff_util::DiffFormatArgs;
use crate::diff_util::HunkSourceDisplay;
use crate::diff_util::show_color_words_diff_for_selection;
use crate::diff_util::show_templated;
use crate::formatter::FormatterExt as _;
use crate::ui::Ui;

/// Compare file contents between two revisions
///
/// With the `-r` option, shows the changes compared to the parent revision.
/// If there are several parent revisions (i.e., the given revision is a
/// merge), then they will be merged and the changes from the result to the
/// given revision will be shown.
///
/// With the `--from` and/or `--to` options, shows the difference from/to the
/// given revisions. If either is left out, it defaults to the working-copy
/// commit. For example, `jj diff --from main` shows the changes from "main"
/// (perhaps a bookmark name) to the working-copy commit.
///
/// If no option is specified, it defaults to `-r @`.
///
/// With `--hunk` or `--hunk-file`, only the selected hunks of the diff are
/// shown, using the IDs listed by `--hunks` for the same revision or range.
/// Combine this with `--git` to get a patch of just those changes.
/// For a partial text hunk, unselected removed lines are kept, followed by
/// the selected added lines.
#[derive(clap::Args, Clone, Debug)]
#[command(mut_arg("ignore_all_space", |a| a.short('w')))]
#[command(mut_arg("ignore_space_change", |a| a.short('b')))]
pub(crate) struct DiffArgs {
    /// Show changes in these revisions
    ///
    /// If there are multiple revisions, then the total diff for all of them
    /// will be shown. For example, if you have a linear chain of revisions
    /// A..D, then `jj diff -r B::D` equals `jj diff --from A --to D`. Multiple
    /// heads and/or roots are supported, but gaps in the revset are not
    /// supported (e.g. `jj diff -r 'A|C'` in a linear chain A..C).
    ///
    /// If a revision is a merge commit, this shows changes *from* the
    /// automatic merge of the contents of all of its parents *to* the contents
    /// of the revision itself.
    ///
    /// If none of `-r`, `-f`, or `-t` is provided, then the default is `-r @`.
    #[arg(long, short, value_name = "REVSETS", alias = "revision")]
    #[arg(add = ArgValueCompleter::new(complete::revset_expression_all))]
    revisions: Option<Vec<RevisionArg>>,

    /// Show changes from this revision
    ///
    /// If none of `-r`, `-f`, or `-t` is provided, then the default is `-r @`.
    #[arg(long, short, conflicts_with = "revisions", value_name = "REVSET")]
    #[arg(add = ArgValueCompleter::new(complete::revset_expression_all))]
    from: Option<RevisionArg>,

    /// Show changes to this revision
    ///
    /// If none of `-r`, `-f`, or `-t` is provided, then the default is `-r @`.
    #[arg(long, short, conflicts_with = "revisions", value_name = "REVSET")]
    #[arg(add = ArgValueCompleter::new(complete::revset_expression_all))]
    to: Option<RevisionArg>,

    /// Restrict the diff to these paths
    #[arg(value_name = "FILESETS", value_hint = clap::ValueHint::AnyPath)]
    #[arg(add = ArgValueCompleter::new(complete::modified_revision_or_range_files))]
    paths: Vec<String>,

    /// Show hunks with IDs for use with `--hunk`
    #[arg(
        long,
        conflicts_with_all = [
            "template",
            "short-format",
            "long-format",
            "tool",
            "ignore_all_space",
            "ignore_space_change",
            "hunk_ids",
            "hunk_files",
        ],
        help_heading = "Diff Formatting Options",
    )]
    hunks: bool,

    #[command(flatten)]
    hunk_selection: HunkIdArgs,

    /// Render each file diff entry using the given template
    ///
    /// All 0-argument methods of the [`TreeDiffEntry` type][TreeDiffEntry] are
    /// available as keywords in the template expression. See
    /// [`jj help -k templates`][templates] for more information.
    ///
    /// [TreeDiffEntry]:
    ///     https://docs.jj-vcs.dev/latest/templates/#treediffentry-type
    ///
    /// [templates]: https://docs.jj-vcs.dev/latest/templates/
    #[arg(
        long,
        short = 'T',
        conflicts_with_all = ["short-format", "long-format", "tool"],
        help_heading = "Diff Formatting Options",
    )]
    #[arg(add = ArgValueCandidates::new(complete::template_aliases))]
    template: Option<String>,

    #[command(flatten)]
    format: DiffFormatArgs,
}

#[instrument(skip_all)]
pub(crate) async fn cmd_diff(
    ui: &mut Ui,
    command: &CommandHelper,
    args: &DiffArgs,
) -> Result<(), CommandError> {
    let selections = args.hunk_selection.resolve(command.cwd())?;
    let uses_hunk_ids = args.hunks || !selections.is_empty();
    let workspace_command = command.workspace_helper(ui).await?;
    let repo = workspace_command.repo();
    let fileset_expression = workspace_command.parse_file_patterns(ui, &args.paths)?;
    let matcher = fileset_expression.to_matcher();
    let copy_record_matcher = match uses_hunk_ids {
        true => &EverythingMatcher as &dyn Matcher,
        false => matcher.as_ref(),
    };

    let from_tree;
    let to_tree;
    let mut hunk_sources = None;
    let from_commits;
    let to_commits;
    if args.from.is_some() || args.to.is_some() {
        let resolve_revision = async |r: &Option<RevisionArg>| {
            workspace_command
                .resolve_single_rev(ui, r.as_ref().unwrap_or(&RevisionArg::AT))
                .await
        };
        let from = resolve_revision(&args.from).await?;
        let to = resolve_revision(&args.to).await?;
        from_tree = from.tree();
        to_tree = to.tree();

        from_commits = vec![from];
        to_commits = vec![to];
    } else {
        let revision_args = args
            .revisions
            .as_deref()
            .unwrap_or(std::slice::from_ref(&RevisionArg::AT));
        let revisions_evaluator = workspace_command.parse_union_revsets(ui, revision_args)?;
        let target_expression = revisions_evaluator.expression();
        if uses_hunk_ids {
            hunk_sources = Some(
                workspace_command
                    .attach_revset_evaluator(target_expression.clone())
                    .evaluate_to_commits()?
                    .try_collect::<Vec<_>>()
                    .await?,
            );
        }
        let mut gaps_revset = workspace_command
            .attach_revset_evaluator(
                target_expression
                    .roots()
                    .range(&target_expression.heads())
                    .minus(target_expression),
            )
            .evaluate_to_commit_ids()?;
        if let Some(commit_id) = gaps_revset.try_next().await? {
            return Err(
                user_error("Cannot diff revsets with gaps in.").hinted(format!(
                    "Revision {} would need to be in the set.",
                    short_commit_hash(&commit_id)
                )),
            );
        }
        let heads: Vec<_> = workspace_command
            .attach_revset_evaluator(target_expression.heads())
            .evaluate_to_commits()?
            .try_collect()
            .await?;
        let roots: Vec<_> = workspace_command
            .attach_revset_evaluator(target_expression.roots())
            .evaluate_to_commits()?
            .try_collect()
            .await?;

        // Collect parents outside of revset to preserve parent order
        let parents: IndexSet<_> = try_join_all(roots.iter().map(|c| c.parents()))
            .await?
            .into_iter()
            .flatten()
            .collect();
        let parents = parents.into_iter().collect_vec();
        from_tree = merge_commit_trees(repo.as_ref(), &parents).await?;
        to_tree = merge_commit_trees(repo.as_ref(), &heads).await?;

        from_commits = parents;
        to_commits = heads;
    }

    let mut shown_tree = to_tree.clone();
    if uses_hunk_ids {
        let sources = match hunk_sources.as_deref() {
            Some([source, rest @ ..]) => std::iter::once(source)
                .chain(rest)
                .map(DiffSelectionSource::Commit)
                .collect::<Vec<_>>(),
            None | Some([]) => vec![DiffSelectionSource::Range {
                from: &from_commits,
                to: &to_commits,
            }],
        };
        let source_display = match sources.as_slice() {
            [_, _, ..] if !args.hunks => {
                return Err(user_error(
                    "`--hunk` and `--hunk-file` require a single revision or `--from`/`--to`",
                ));
            }
            [_, _, ..] => HunkSourceDisplay::Show,
            [] | [_] => HunkSourceDisplay::Hide,
        };
        let mut options = ColorWordsDiffOptions::from_settings(workspace_command.settings())?;
        options.merge_args(&args.format);
        for source in sources {
            let plan = DiffSelection::build(source.capture(repo.as_ref()).await?, &matcher).await?;
            if args.hunks {
                ui.request_pager();
                let mut formatter = ui.stdout_formatter();
                let mut formatter = formatter.labeled("diff");
                show_color_words_diff_for_selection(
                    *formatter.labeled("color_words"),
                    repo.store(),
                    &plan,
                    workspace_command.env().path_converter(),
                    &options,
                    workspace_command.env().conflict_marker_style(),
                    source_display,
                )
                .await?;
            } else {
                shown_tree = plan.select(&selections, SelectionAction::Select).await?;
            }
        }
        if args.hunks {
            print_unmatched_explicit_paths(
                ui,
                &workspace_command,
                &fileset_expression,
                [&from_tree, &to_tree],
            )
            .await?;
            return Ok(());
        }
    }
    let roots = from_commits
        .iter()
        .map(|commit| commit.id().clone())
        .collect::<Vec<_>>();
    let heads = to_commits
        .iter()
        .map(|commit| commit.id().clone())
        .collect::<Vec<_>>();
    let copy_records =
        CopyRecords::collect(repo.store(), &roots, &heads, copy_record_matcher).await?;

    // -T disables both short/long rendering formats, but it might be okay to
    // enable long format if explicitly specified (assuming -T is for short or
    // summary output.)
    let maybe_template;
    let diff_renderer;
    if let Some(text) = &args.template {
        let language = workspace_command.commit_template_language();
        let template = workspace_command
            .parse_template(ui, &language, text)?
            .labeled(["diff"]);
        maybe_template = Some(template);
        diff_renderer = workspace_command.diff_renderer(vec![]);
    } else {
        maybe_template = None;
        diff_renderer = workspace_command.diff_renderer_for(&args.format)?;
    }

    ui.request_pager();
    if let Some(template) = &maybe_template {
        let transition = copy_records
            .bind(Diff::new(from_tree.clone(), shown_tree.clone()))
            .await?;
        let tree_diff = transition.diff(&matcher);
        show_templated(ui.stdout_formatter().as_mut(), tree_diff, template).await?;
    }
    diff_renderer
        .show_diff(
            ui,
            ui.stdout_formatter().as_mut(),
            Diff::new(&from_tree, &shown_tree),
            &matcher,
            &copy_records,
            ui.term_width(),
        )
        .await?;
    print_unmatched_explicit_paths(
        ui,
        &workspace_command,
        &fileset_expression,
        [&from_tree, &to_tree],
    )
    .await?;
    Ok(())
}
