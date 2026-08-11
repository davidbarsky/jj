// Copyright 2026 The Jujutsu Authors
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

use bstr::BStr;
use bstr::BString;
use jj_lib::backend::CommitId;
use jj_lib::backend::CopyId;
use jj_lib::backend::CopyRecord;
use jj_lib::backend::FileId;
use jj_lib::backend::MergedTreeValueExt as _;
use jj_lib::backend::TreeValue;
use jj_lib::conflict_labels::ConflictLabels;
use jj_lib::copies::CopyOperation;
use jj_lib::copies::CopyRecords;
use jj_lib::diff_selection::DiffHunkContext;
use jj_lib::diff_selection::DiffHunkId;
use jj_lib::diff_selection::DiffHunkIdParseError;
use jj_lib::diff_selection::DiffSelection;
use jj_lib::diff_selection::DiffSelectionError;
use jj_lib::diff_selection::DiffSelectionSource;
use jj_lib::diff_selection::HunkLineSide;
use jj_lib::diff_selection::HunkPresentation;
use jj_lib::diff_selection::HunkSelection;
use jj_lib::diff_selection::PathHunkReason;
use jj_lib::diff_selection::SelectionAction;
use jj_lib::matchers::EverythingMatcher;
use jj_lib::matchers::FilesMatcher;
use jj_lib::merge::Diff;
use jj_lib::merge::Merge;
use jj_lib::merged_tree::MergedTree;
use jj_lib::object_id::ObjectId as _;
use jj_lib::repo::Repo as _;
use jj_lib::repo_path::RepoPath;
use pollster::FutureExt as _;
use pretty_assertions::assert_eq;
use test_case::test_case;
use testutils::TestRepo;
use testutils::TestResult;
use testutils::TestThreeWayMergeTreeBuilder;
use testutils::create_single_tree;
use testutils::create_tree;
use testutils::create_tree_with;
use testutils::repo_path;
use testutils::repo_path_component;

fn file_id_at(tree: &MergedTree, path: &RepoPath) -> FileId {
    let value = tree
        .path_value(path)
        .block_on()
        .unwrap()
        .into_resolved()
        .unwrap();
    let Some(TreeValue::File {
        id,
        executable: _,
        copy_id: _,
    }) = value
    else {
        panic!("expected a regular file fixture");
    };
    id
}

#[test]
fn test_select_resolved_text_hunk_and_endpoints() -> TestResult {
    let test_repo = TestRepo::init();
    let repo = &test_repo.repo;
    let path = repo_path("file");
    let before = create_tree_with(repo, |builder| {
        builder
            .file(path, "first before\n<<<<<<< not a conflict\nlast before\n")
            .executable(true);
    });
    let after = create_tree_with(repo, |builder| {
        builder
            .file(path, "first after\n<<<<<<< not a conflict\nlast after\n")
            .executable(true);
    });
    let plan = DiffSelection::build(
        DiffSelectionSource::Trees {
            transition: CopyRecords::default()
                .bind(Diff::new(before.clone(), after.clone()))
                .block_on()?
                .into(),
            tree_labels: Diff::new("before", "after"),
            context: DiffHunkContext::TreePair,
        }
        .capture(repo.as_ref())
        .block_on()?,
        &EverythingMatcher,
    )
    .block_on()?;

    assert_eq!(plan.hunks().len(), 2);
    assert_eq!(
        plan.select(&[], SelectionAction::Select)
            .block_on()?
            .tree_ids(),
        before.tree_ids()
    );
    let all_ids = plan
        .hunks()
        .iter()
        .map(|hunk| HunkSelection::from(*hunk.id()))
        .collect::<Vec<_>>();
    assert_eq!(
        plan.select(&all_ids, SelectionAction::Select)
            .block_on()?
            .tree_ids(),
        after.tree_ids()
    );

    let first_id = *plan.hunks().first().unwrap().id();
    let selected = plan
        .select(&[first_id.into()], SelectionAction::Select)
        .block_on()?;
    let value = selected
        .path_value(path)
        .block_on()?
        .into_resolved()
        .unwrap();
    let Some(jj_lib::backend::TreeValue::File {
        id,
        executable,
        copy_id,
    }) = value
    else {
        panic!("selected path should be a resolved file");
    };
    assert!(executable);
    assert_eq!(copy_id, CopyId::placeholder());
    assert_eq!(
        testutils::read_file(repo.store(), path, &id),
        b"first after\n<<<<<<< not a conflict\nlast before\n"
    );
    Ok(())
}

#[test]
fn test_select_all_hunks_in_fileset_preserves_unmatched_paths() -> TestResult {
    let test_repo = TestRepo::init();
    let repo = &test_repo.repo;
    let selected_path = repo_path("selected");
    let unmatched_path = repo_path("unmatched");
    let before = create_tree(
        repo,
        &[(selected_path, "before\n"), (unmatched_path, "before\n")],
    );
    let after = create_tree(
        repo,
        &[(selected_path, "after\n"), (unmatched_path, "after\n")],
    );
    let plan = DiffSelection::build(
        DiffSelectionSource::Trees {
            transition: CopyRecords::default()
                .bind(Diff::new(before.clone(), after.clone()))
                .block_on()?
                .into(),
            tree_labels: Diff::new("before", "after"),
            context: DiffHunkContext::TreePair,
        }
        .capture(repo.as_ref())
        .block_on()?,
        &FilesMatcher::new([selected_path]),
    )
    .block_on()?;
    let ids = plan
        .hunks()
        .iter()
        .map(|hunk| HunkSelection::from(*hunk.id()))
        .collect::<Vec<_>>();

    let selected = plan.select(&ids, SelectionAction::Select).block_on()?;

    assert_eq!(
        selected.path_value(selected_path).block_on()?,
        after.path_value(selected_path).block_on()?
    );
    assert_eq!(
        selected.path_value(unmatched_path).block_on()?,
        before.path_value(unmatched_path).block_on()?
    );
    Ok(())
}

#[test_case(&[], b"first before\nmiddle\nlast before\n"; "select before state")]
#[test_case(&[0], b"first after\nmiddle\nlast before\n"; "select first hunk")]
#[test_case(&[1], b"first before\nmiddle\nlast after\n"; "select second hunk")]
#[test_case(&[0, 1], b"first after\nmiddle\nlast after\n"; "select after state")]
fn test_text_hunks_partition_before_and_after(
    selected_hunks: &[usize],
    expected: &[u8],
) -> TestResult {
    let test_repo = TestRepo::init();
    let repo = &test_repo.repo;
    let path = repo_path("file");
    let before = create_tree(repo, &[(path, "first before\nmiddle\nlast before\n")]);
    let after = create_tree(repo, &[(path, "first after\nmiddle\nlast after\n")]);
    let plan = DiffSelection::build(
        DiffSelectionSource::Trees {
            transition: CopyRecords::default()
                .bind(Diff::new(before, after))
                .block_on()?
                .into(),
            tree_labels: Diff::new("before", "after"),
            context: DiffHunkContext::TreePair,
        }
        .capture(repo.as_ref())
        .block_on()?,
        &EverythingMatcher,
    )
    .block_on()?;
    let ids = plan
        .hunks()
        .iter()
        .map(|hunk| HunkSelection::from(*hunk.id()))
        .collect::<Vec<_>>();
    let selected_ids = selected_hunks
        .iter()
        .map(|&index| ids[index].clone())
        .collect::<Vec<_>>();
    let selected = plan
        .select(&selected_ids, SelectionAction::Select)
        .block_on()?;
    let value = selected
        .path_value(path)
        .block_on()?
        .into_resolved()
        .unwrap();
    let Some(jj_lib::backend::TreeValue::File {
        id,
        executable: _,
        copy_id: _,
    }) = value
    else {
        panic!("selected path should be a resolved file");
    };
    assert_eq!(testutils::read_file(repo.store(), path, &id), expected);
    Ok(())
}

#[test]
fn test_diff_hunk_id_requires_exact_hex() {
    match "abcd".parse::<DiffHunkId>() {
        Err(DiffHunkIdParseError::Length(4)) => {}
        Err(DiffHunkIdParseError::Length(length)) => {
            panic!("expected length 4, got {length}")
        }
        Err(DiffHunkIdParseError::NotHex) => panic!("valid hex should report its length"),
        Ok(id) => panic!("short hunk ID should not parse: {id}"),
    }
    match "0000000000000000000000000000000".parse::<DiffHunkId>() {
        Err(DiffHunkIdParseError::Length(31)) => {}
        Err(DiffHunkIdParseError::Length(length)) => {
            panic!("expected length 31, got {length}")
        }
        Err(DiffHunkIdParseError::NotHex) => panic!("valid hex should report its length"),
        Ok(id) => panic!("short hunk ID should not parse: {id}"),
    }
    match "0000000000000000000000000000000g".parse::<DiffHunkId>() {
        Err(DiffHunkIdParseError::NotHex) => {}
        Err(DiffHunkIdParseError::Length(length)) => {
            panic!("32 characters should be decoded, got length {length}")
        }
        Ok(id) => panic!("non-hex hunk ID should not parse: {id}"),
    }
    let id = "00000000000000000000000000000000"
        .parse::<DiffHunkId>()
        .unwrap();
    assert_eq!(id.to_string(), "00000000000000000000000000000000");
}

#[test]
fn test_hunk_selection_parses_line_ranges() -> TestResult {
    let id = "00000000000000000000000000000000";
    assert_eq!(
        id.parse::<HunkSelection>()?,
        HunkSelection {
            id: id.parse()?,
            lines: None,
        }
    );
    let selection = format!("{id}:-2-4,+3").parse::<HunkSelection>()?;
    assert_eq!(selection.id, id.parse()?);
    let Some(_) = selection.lines.as_ref() else {
        panic!("line selection should contain ranges");
    };
    assert_eq!(
        selection,
        format!("{id}:+3,-4,-2-3,").parse::<HunkSelection>()?
    );
    assert_ne!(selection, format!("{id}:-2-4,-3").parse::<HunkSelection>()?);

    Ok(())
}

#[test_case(
    "00000000000000000000000000000000:",
    "expected line numbers after `:`";
    "missing line numbers"
)]
#[test_case(
    "00000000000000000000000000000000:2",
    "line selection `2` must start with `-` (removed) or `+` (added)";
    "line number without side"
)]
#[test_case(
    "00000000000000000000000000000000:-0",
    "line selection `-0` must be a line number or an ascending range such as `2-4`";
    "zero line number"
)]
#[test_case(
    "00000000000000000000000000000000:+4-2",
    "line selection `+4-2` must be a line number or an ascending range such as `2-4`";
    "descending added range"
)]
#[test_case(
    "00000000000000000000000000000000:-x",
    "line selection `-x` must be a line number or an ascending range such as `2-4`";
    "non-numeric line number"
)]
#[test_case(
    "abcd:-1",
    "hunk ID must be 32 hexadecimal characters, got 4";
    "short hunk ID"
)]
fn test_hunk_selection_rejects_invalid_line_ranges(input: &str, expected: &str) -> TestResult {
    let error = input.parse::<HunkSelection>().unwrap_err();
    assert_eq!(error.to_string(), expected, "parsing {input}");
    Ok(())
}

#[test]
fn test_select_lines_of_text_hunks() -> TestResult {
    let test_repo = TestRepo::init();
    let repo = &test_repo.repo;
    let path = repo_path("file");
    let before = create_tree(repo, &[(path, "a\nb\nc\nd\ne\n")]);
    let after = create_tree(repo, &[(path, "a\nB1\nB2\nd\nE\n")]);
    let plan = DiffSelection::build(
        DiffSelectionSource::Trees {
            transition: CopyRecords::default()
                .bind(Diff::new(before, after))
                .block_on()?
                .into(),
            tree_labels: Diff::new("before", "after"),
            context: DiffHunkContext::TreePair,
        }
        .capture(repo.as_ref())
        .block_on()?,
        &EverythingMatcher,
    )
    .block_on()?;
    let [first, last] = plan
        .hunks()
        .iter()
        .map(|hunk| *hunk.id())
        .collect::<Vec<_>>()
        .try_into()
        .unwrap();
    let select_file = |selections: &[String]| -> TestResult<Vec<u8>> {
        let selections = selections
            .iter()
            .map(|selection| selection.parse())
            .collect::<Result<Vec<HunkSelection>, _>>()?;
        let tree = plan
            .select(&selections, SelectionAction::Select)
            .block_on()?;
        Ok(testutils::read_file(
            repo.store(),
            path,
            &file_id_at(&tree, path),
        ))
    };

    // Numbers start at each hunk's first changed line, so `-2` is `c` and `+2`
    // is `B2`. Unselected removed lines stay before selected additions.
    assert_eq!(
        select_file(&[format!("{first}:-2,+2")])?,
        b"a\nb\nB2\nd\ne\n"
    );
    assert_eq!(
        select_file(&[format!("{first}:+1-2")])?,
        b"a\nb\nc\nB1\nB2\nd\ne\n"
    );
    // The last hunk is shown after the context line `d`, which is not
    // numbered.
    assert_eq!(select_file(&[format!("{last}:+1")])?, b"a\nb\nc\nd\ne\nE\n");
    assert_eq!(select_file(&[format!("{last}:-1")])?, b"a\nb\nc\nd\n");
    // Line selections of one hunk combine, and a whole selection wins.
    assert_eq!(
        select_file(&[format!("{last}:-1"), format!("{last}:+1")])?,
        b"a\nb\nc\nd\nE\n"
    );
    assert_eq!(
        select_file(&[format!("{first}:-1"), first.to_string()])?,
        b"a\nB1\nB2\nd\ne\n"
    );

    Ok(())
}

#[derive(Clone, Copy, Debug)]
enum TextHunk {
    First,
    Last,
}

#[test_case(TextHunk::Last, "-2", HunkLineSide::Removed, 2; "removed line after last hunk")]
#[test_case(TextHunk::First, "+3", HunkLineSide::Added, 3; "added line after first hunk")]
#[test_case(TextHunk::First, "-3", HunkLineSide::Removed, 3; "removed line after first hunk")]
fn test_invalid_line_selection_of_text_hunk(
    error_hunk: TextHunk,
    error_line_selection: &str,
    expected_side: HunkLineSide,
    expected_line: usize,
) -> TestResult {
    let test_repo = TestRepo::init();
    let repo = &test_repo.repo;
    let path = repo_path("file");
    let before = create_tree(repo, &[(path, "a\nb\nc\nd\ne\n")]);
    let after = create_tree(repo, &[(path, "a\nB1\nB2\nd\nE\n")]);
    let plan = DiffSelection::build(
        DiffSelectionSource::Trees {
            transition: CopyRecords::default()
                .bind(Diff::new(before, after))
                .block_on()?
                .into(),
            tree_labels: Diff::new("before", "after"),
            context: DiffHunkContext::TreePair,
        }
        .capture(repo.as_ref())
        .block_on()?,
        &EverythingMatcher,
    )
    .block_on()?;
    let [first, last] = plan
        .hunks()
        .iter()
        .map(|hunk| *hunk.id())
        .collect::<Vec<_>>()
        .try_into()
        .unwrap();
    let hunk_id = match error_hunk {
        TextHunk::First => first,
        TextHunk::Last => last,
    };
    let selection = format!("{hunk_id}:{error_line_selection}");
    let error = plan
        .select(&[selection.parse()?], SelectionAction::Select)
        .block_on()
        .unwrap_err();
    match error {
        DiffSelectionError::LineNotInHunk { id: _, side, line } => {
            assert_eq!((side, line), (expected_side, expected_line), "{selection}");
        }
        DiffSelectionError::HunkNotFound(_)
        | DiffSelectionError::LinesUnsupported(_)
        | DiffSelectionError::Backend(_) => panic!("unexpected error for {selection}: {error}"),
    }
    Ok(())
}

#[test]
fn test_line_selection_of_path_atom_is_rejected() -> TestResult {
    let test_repo = TestRepo::init();
    let repo = &test_repo.repo;
    let path = repo_path("added");
    let before = create_tree(repo, &[]);
    let after = create_tree(repo, &[(path, "a\nb\n")]);
    let plan = DiffSelection::build(
        DiffSelectionSource::Trees {
            transition: CopyRecords::default()
                .bind(Diff::new(before, after))
                .block_on()?
                .into(),
            tree_labels: Diff::new("before", "after"),
            context: DiffHunkContext::TreePair,
        }
        .capture(repo.as_ref())
        .block_on()?,
        &EverythingMatcher,
    )
    .block_on()?;
    let id = *plan.hunks()[0].id();

    let error = plan
        .select(&[format!("{id}:+1").parse()?], SelectionAction::Select)
        .block_on()
        .unwrap_err();

    match error {
        DiffSelectionError::LinesUnsupported(error_id) => assert_eq!(error_id, id),
        DiffSelectionError::HunkNotFound(_)
        | DiffSelectionError::LineNotInHunk {
            id: _,
            side: _,
            line: _,
        }
        | DiffSelectionError::Backend(_) => panic!("unexpected error: {error}"),
    }
    Ok(())
}

#[test]
fn test_stale_hunk_id_is_rejected() -> TestResult {
    let test_repo = TestRepo::init();
    let repo = &test_repo.repo;
    let path = repo_path("file");
    let before = create_tree(repo, &[(path, "before\n")]);
    let after = create_tree(repo, &[(path, "after\n")]);
    let stale_plan = DiffSelection::build(
        DiffSelectionSource::Trees {
            transition: CopyRecords::default()
                .bind(Diff::new(before.clone(), after))
                .block_on()?
                .into(),
            tree_labels: Diff::new("before", "after"),
            context: DiffHunkContext::TreePair,
        }
        .capture(repo.as_ref())
        .block_on()?,
        &EverythingMatcher,
    )
    .block_on()?;
    let stale_id = *stale_plan.hunks().first().unwrap().id();
    let new_after = create_tree(repo, &[(path, "new after\n")]);
    let new_plan = DiffSelection::build(
        DiffSelectionSource::Trees {
            transition: CopyRecords::default()
                .bind(Diff::new(before, new_after))
                .block_on()?
                .into(),
            tree_labels: Diff::new("before", "after"),
            context: DiffHunkContext::TreePair,
        }
        .capture(repo.as_ref())
        .block_on()?,
        &EverythingMatcher,
    )
    .block_on()?;

    let error = new_plan
        .select(&[stale_id.into()], SelectionAction::Select)
        .block_on()
        .unwrap_err();

    match error {
        DiffSelectionError::HunkNotFound(id) => assert_eq!(id, stale_id),
        DiffSelectionError::LinesUnsupported(_)
        | DiffSelectionError::LineNotInHunk {
            id: _,
            side: _,
            line: _,
        } => panic!("whole selection should not report a line error: {error}"),
        DiffSelectionError::Backend(error) => panic!("unexpected backend error: {error}"),
    }
    Ok(())
}

#[test]
fn test_repeated_identical_hunks_have_distinct_ids() -> TestResult {
    let test_repo = TestRepo::init();
    let repo = &test_repo.repo;
    let path = repo_path("file");
    let before = create_tree(repo, &[(path, "old\nseparator\nold\n")]);
    let after = create_tree(repo, &[(path, "new\nseparator\nnew\n")]);
    let plan = DiffSelection::build(
        DiffSelectionSource::Trees {
            transition: CopyRecords::default()
                .bind(Diff::new(before, after))
                .block_on()?
                .into(),
            tree_labels: Diff::new("before", "after"),
            context: DiffHunkContext::TreePair,
        }
        .capture(repo.as_ref())
        .block_on()?,
        &EverythingMatcher,
    )
    .block_on()?;

    let ids = plan
        .hunks()
        .iter()
        .map(|hunk| *hunk.id())
        .collect::<Vec<_>>();
    assert_eq!(ids.len(), 2);
    assert_ne!(ids[0], ids[1]);
    Ok(())
}

#[test]
fn test_hunk_id_survives_changes_around_the_atom() -> TestResult {
    let test_repo = TestRepo::init();
    let repo = &test_repo.repo;
    let path = repo_path("file");
    let other_path = repo_path("other");
    let build = |before: MergedTree, after: MergedTree, commit: u8| {
        DiffSelection::build(
            DiffSelectionSource::Trees {
                transition: CopyRecords::default()
                    .bind(Diff::new(before, after))
                    .block_on()?
                    .into(),
                tree_labels: Diff::new("before", "after"),
                context: DiffHunkContext::Commit(CommitId::new(vec![commit])),
            }
            .capture(repo.as_ref())
            .block_on()?,
            &EverythingMatcher,
        )
        .block_on()
    };
    let first = build(
        create_tree(repo, &[(path, "kept\nbefore\n")]),
        create_tree(repo, &[(path, "kept\nafter\n")]),
        1,
    )?;
    // A different commit whose trees differ everywhere except in the atom and
    // the line next to it: a new line further above, and an unrelated change
    // to another file.
    let second_before = create_tree(
        repo,
        &[
            (path, "added above\nkept\nbefore\n"),
            (other_path, "other\n"),
        ],
    );
    let second = build(
        second_before.clone(),
        create_tree(
            repo,
            &[
                (path, "added above\nkept\nafter\n"),
                (other_path, "changed\n"),
            ],
        ),
        2,
    )?;
    // Changing the line next to the atom is the one nearby edit that retires
    // its ID.
    let neighbor_changed = build(
        create_tree(repo, &[(path, "edited\nbefore\n")]),
        create_tree(repo, &[(path, "edited\nafter\n")]),
        3,
    )?;

    let id = *first.hunks()[0].id();
    let second_hunks = second.hunks();
    assert_eq!(*second_hunks[0].id(), id);
    assert_ne!(*neighbor_changed.hunks()[0].id(), id);
    let HunkPresentation::Text(text) = &second_hunks[0] else {
        panic!("the file change should be a text atom");
    };
    // The atom has no unchanged lines after it, so the lines before it are
    // shown as leading context, outside the numbered contents.
    assert_eq!(
        text.context(),
        Diff::new(Some(BStr::new("added above\nkept\n")), None)
    );
    let selected = second
        .select(&[format!("{id}:+1").parse()?], SelectionAction::Select)
        .block_on()?;
    assert_eq!(
        testutils::read_file(repo.store(), path, &file_id_at(&selected, path)),
        b"added above\nkept\nbefore\nafter\n"
    );
    assert_eq!(
        selected.path_value(other_path).block_on()?,
        second_before.path_value(other_path).block_on()?
    );
    Ok(())
}

#[test]
fn test_rename_is_one_indivisible_atom() -> TestResult {
    let test_repo = TestRepo::init();
    let repo = &test_repo.repo;
    let before_path = repo_path("before");
    let after_path = repo_path("after");
    let before = create_tree(repo, &[(before_path, "contents\n")]);
    let after = create_tree(repo, &[(after_path, "contents\n")]);
    let source_file = file_id_at(&before, before_path);
    let mut copy_records = CopyRecords::default();
    copy_records.add_records([CopyRecord {
        target: after_path.to_owned(),
        source: before_path.to_owned(),
        target_commit: CommitId::new(vec![2]),
        source_commit: CommitId::new(vec![1]),
        source_file,
    }]);
    let plan = DiffSelection::build(
        DiffSelectionSource::Trees {
            transition: copy_records
                .bind(Diff::new(before, after.clone()))
                .block_on()?
                .into(),
            tree_labels: Diff::new("before", "after"),
            context: DiffHunkContext::TreePair,
        }
        .capture(repo.as_ref())
        .block_on()?,
        &EverythingMatcher,
    )
    .block_on()?;

    let hunks = plan.hunks();
    let [hunk] = hunks.as_slice() else {
        panic!("rename should be represented by one copy-aware atom");
    };
    let id = *hunk.id();
    let HunkPresentation::Copy(view) = hunk else {
        panic!("rename should be represented by one copy-aware atom");
    };
    let (paths, _values, operation) = view.details();
    assert_eq!(paths.before, before_path);
    assert_eq!(paths.after, after_path);
    assert_eq!(operation, CopyOperation::Rename);
    assert_eq!(
        plan.select(&[id.into()], SelectionAction::Select)
            .block_on()?
            .tree_ids(),
        after.tree_ids()
    );
    Ok(())
}

#[test]
fn test_copy_is_one_indivisible_atom() -> TestResult {
    let test_repo = TestRepo::init();
    let repo = &test_repo.repo;
    let before_path = repo_path("before");
    let after_path = repo_path("after");
    let before = create_tree(repo, &[(before_path, "contents\n")]);
    let after = create_tree(
        repo,
        &[(before_path, "contents\n"), (after_path, "contents\n")],
    );
    let source_file = file_id_at(&before, before_path);
    let mut copy_records = CopyRecords::default();
    copy_records.add_records([CopyRecord {
        target: after_path.to_owned(),
        source: before_path.to_owned(),
        target_commit: CommitId::new(vec![2]),
        source_commit: CommitId::new(vec![1]),
        source_file,
    }]);
    let plan = DiffSelection::build(
        DiffSelectionSource::Trees {
            transition: copy_records
                .bind(Diff::new(before, after.clone()))
                .block_on()?
                .into(),
            tree_labels: Diff::new("before", "after"),
            context: DiffHunkContext::TreePair,
        }
        .capture(repo.as_ref())
        .block_on()?,
        &EverythingMatcher,
    )
    .block_on()?;

    let hunks = plan.hunks();
    let [hunk] = hunks.as_slice() else {
        panic!("copy should be represented by one copy-aware atom");
    };
    let id = *hunk.id();
    let HunkPresentation::Copy(view) = hunk else {
        panic!("copy should be represented by one copy-aware atom");
    };
    assert_eq!(view.details().2, CopyOperation::Copy);
    assert_eq!(
        plan.select(&[id.into()], SelectionAction::Select)
            .block_on()?
            .tree_ids(),
        after.tree_ids()
    );
    Ok(())
}

#[test]
fn test_multiple_copy_targets_do_not_each_remove_source() -> TestResult {
    let test_repo = TestRepo::init();
    let repo = &test_repo.repo;
    let source = repo_path("source");
    let first_target = repo_path("first-target");
    let second_target = repo_path("second-target");
    let before = create_tree(repo, &[(source, "contents\n")]);
    let after = create_tree(
        repo,
        &[(first_target, "contents\n"), (second_target, "contents\n")],
    );
    let source_file = file_id_at(&before, source);
    let mut copy_records = CopyRecords::default();
    copy_records.add_records([
        CopyRecord {
            target: first_target.to_owned(),
            source: source.to_owned(),
            target_commit: CommitId::new(vec![2]),
            source_commit: CommitId::new(vec![1]),
            source_file: source_file.clone(),
        },
        CopyRecord {
            target: second_target.to_owned(),
            source: source.to_owned(),
            target_commit: CommitId::new(vec![2]),
            source_commit: CommitId::new(vec![1]),
            source_file,
        },
    ]);
    let plan = DiffSelection::build(
        DiffSelectionSource::Trees {
            transition: copy_records
                .bind(Diff::new(before, after.clone()))
                .block_on()?
                .into(),
            tree_labels: Diff::new("before", "after"),
            context: DiffHunkContext::TreePair,
        }
        .capture(repo.as_ref())
        .block_on()?,
        &EverythingMatcher,
    )
    .block_on()?;
    let hunks = plan.hunks();
    let first_copy_id = hunks
        .iter()
        .find_map(|hunk| {
            let HunkPresentation::Copy(view) = hunk else {
                return None;
            };
            if view.details().2 == CopyOperation::Copy && view.details().0.after == first_target {
                Some(*hunk.id())
            } else {
                None
            }
        })
        .expect("first copy target should be selectable without deleting its source");
    let expected = create_tree(
        repo,
        &[(source, "contents\n"), (first_target, "contents\n")],
    );
    let all_ids = hunks
        .iter()
        .map(|hunk| HunkSelection::from(*hunk.id()))
        .collect::<Vec<_>>();

    assert_eq!(
        plan.select(&[first_copy_id.into()], SelectionAction::Select)
            .block_on()?
            .tree_ids(),
        expected.tree_ids()
    );
    assert_eq!(
        plan.select(&all_ids, SelectionAction::Select)
            .block_on()?
            .tree_ids(),
        after.tree_ids()
    );
    Ok(())
}

#[test]
fn test_rename_source_match_includes_both_endpoints() -> TestResult {
    let test_repo = TestRepo::init();
    let repo = &test_repo.repo;
    let source = repo_path("source");
    let target = repo_path("target");
    let before = create_tree(repo, &[(source, "contents\n")]);
    let after = create_tree(repo, &[(target, "contents\n")]);
    let source_file = file_id_at(&before, source);
    let mut copy_records = CopyRecords::default();
    copy_records.add_records([CopyRecord {
        target: target.to_owned(),
        source: source.to_owned(),
        target_commit: CommitId::new(vec![2]),
        source_commit: CommitId::new(vec![1]),
        source_file,
    }]);
    let plan = DiffSelection::build(
        DiffSelectionSource::Trees {
            transition: copy_records
                .bind(Diff::new(before, after.clone()))
                .block_on()?
                .into(),
            tree_labels: Diff::new("before", "after"),
            context: DiffHunkContext::TreePair,
        }
        .capture(repo.as_ref())
        .block_on()?,
        &FilesMatcher::new([source]),
    )
    .block_on()?;

    let hunks = plan.hunks();
    let [hunk] = hunks.as_slice() else {
        panic!("matching either endpoint should expose one copy-aware atom");
    };
    let id = *hunk.id();
    let HunkPresentation::Copy(view) = hunk else {
        panic!("matching either endpoint should expose one copy-aware atom");
    };
    let (paths, _values, operation) = view.details();
    assert_eq!(paths.before, source);
    assert_eq!(paths.after, target);
    assert_eq!(operation, CopyOperation::Rename);
    assert_eq!(
        plan.select(&[id.into()], SelectionAction::Select)
            .block_on()?
            .tree_ids(),
        after.tree_ids()
    );
    Ok(())
}

#[test]
fn test_structural_atom_removes_external_rename_source() -> TestResult {
    let test_repo = TestRepo::init();
    let repo = &test_repo.repo;
    let source = repo_path("source");
    let node = repo_path("node");
    let target = node.join(repo_path_component("child"));
    let before = create_tree(repo, &[(source, "contents\n"), (node, "blocking\n")]);
    let after = create_tree(repo, &[(&target, "contents\n")]);
    let source_file = file_id_at(&before, source);
    let mut copy_records = CopyRecords::default();
    copy_records.add_records([CopyRecord {
        target: target.clone(),
        source: source.to_owned(),
        target_commit: CommitId::new(vec![2]),
        source_commit: CommitId::new(vec![1]),
        source_file,
    }]);
    let plan = DiffSelection::build(
        DiffSelectionSource::Trees {
            transition: copy_records
                .bind(Diff::new(before, after.clone()))
                .block_on()?
                .into(),
            tree_labels: Diff::new("before", "after"),
            context: DiffHunkContext::TreePair,
        }
        .capture(repo.as_ref())
        .block_on()?,
        &EverythingMatcher,
    )
    .block_on()?;

    let hunks = plan.hunks();
    let [hunk] = hunks.as_slice() else {
        panic!("the structural transition should remain one atom");
    };
    let id = *hunk.id();
    let HunkPresentation::Path(view) = hunk else {
        panic!("the structural transition should remain one atom");
    };
    assert_eq!(view.details().0, node);
    assert_eq!(view.removed_sources(), &[source.to_owned()]);
    assert_eq!(
        plan.select(&[id.into()], SelectionAction::Select)
            .block_on()?
            .tree_ids(),
        after.tree_ids()
    );
    Ok(())
}

#[test]
fn test_conflicting_copy_targets_preserve_source_deletions() -> TestResult {
    let test_repo = TestRepo::init();
    let repo = &test_repo.repo;
    let first = repo_path("first");
    let second = repo_path("second");
    let target = repo_path("target");
    let before = create_tree(repo, &[(first, "first\n"), (second, "second\n")]);
    let after = create_tree(repo, &[(target, "target\n")]);
    let first_source_file = file_id_at(&before, first);
    let second_source_file = file_id_at(&before, second);
    let mut copy_records = CopyRecords::default();
    copy_records.add_records([
        CopyRecord {
            target: target.to_owned(),
            source: first.to_owned(),
            target_commit: CommitId::new(vec![3]),
            source_commit: CommitId::new(vec![1]),
            source_file: first_source_file,
        },
        CopyRecord {
            target: target.to_owned(),
            source: second.to_owned(),
            target_commit: CommitId::new(vec![3]),
            source_commit: CommitId::new(vec![2]),
            source_file: second_source_file,
        },
    ]);
    let plan = DiffSelection::build(
        DiffSelectionSource::Trees {
            transition: copy_records
                .bind(Diff::new(before, after.clone()))
                .block_on()?
                .into(),
            tree_labels: Diff::new("before", "after"),
            context: DiffHunkContext::TreePair,
        }
        .capture(repo.as_ref())
        .block_on()?,
        &EverythingMatcher,
    )
    .block_on()?;
    let hunks = plan.hunks();
    let ids = hunks
        .iter()
        .map(|hunk| HunkSelection::from(*hunk.id()))
        .collect::<Vec<_>>();

    assert_eq!(hunks.len(), 3);
    assert_eq!(
        plan.select(&ids, SelectionAction::Select)
            .block_on()?
            .tree_ids(),
        after.tree_ids()
    );
    Ok(())
}

#[test]
fn test_stale_copy_record_does_not_remove_replacement_tree() -> TestResult {
    let test_repo = TestRepo::init();
    let repo = &test_repo.repo;
    let source = repo_path("source");
    let child = source.join(repo_path_component("child"));
    let target = repo_path("z-target");
    let before = create_tree(repo, &[(source, "before\n")]);
    let after = create_tree(repo, &[(&child, "child\n"), (target, "target\n")]);
    let expected = create_tree(repo, &[(&child, "child\n")]);
    let mut copy_records = CopyRecords::default();
    copy_records.add_records([CopyRecord {
        target: target.to_owned(),
        source: source.to_owned(),
        target_commit: CommitId::new(vec![2]),
        source_commit: CommitId::new(vec![1]),
        source_file: FileId::new(vec![3]),
    }]);
    let plan = DiffSelection::build(
        DiffSelectionSource::Trees {
            transition: copy_records
                .bind(Diff::new(before, after))
                .block_on()?
                .into(),
            tree_labels: Diff::new("before", "after"),
            context: DiffHunkContext::TreePair,
        }
        .capture(repo.as_ref())
        .block_on()?,
        &FilesMatcher::new([source]),
    )
    .block_on()?;
    for hunk in plan.hunks() {
        match hunk {
            HunkPresentation::Text(_) | HunkPresentation::Path(_) => {}
            HunkPresentation::Copy(_) => {
                panic!("a record for a different source file must not combine paths")
            }
        }
    }
    let ids = plan
        .hunks()
        .iter()
        .map(|hunk| HunkSelection::from(*hunk.id()))
        .collect::<Vec<_>>();

    assert_eq!(
        plan.select(&ids, SelectionAction::Select)
            .block_on()?
            .tree_ids(),
        expected.tree_ids()
    );
    Ok(())
}

#[test_case("binary", PathHunkReason::Binary; "binary content change")]
#[test_case("copy", PathHunkReason::FileMetadata; "copy metadata change")]
#[test_case("mode", PathHunkReason::FileMetadata; "executable metadata change")]
fn test_binary_and_metadata_changes_are_path_atoms(
    expected_path_name: &str,
    expected_reason: PathHunkReason,
) -> TestResult {
    let test_repo = TestRepo::init();
    let repo = &test_repo.repo;
    let binary_path = repo_path("binary");
    let copy_path = repo_path("copy");
    let mode_path = repo_path("mode");
    let before = create_tree_with(repo, |builder| {
        builder.file(binary_path, b"before\0");
        builder
            .file(copy_path, b"content")
            .copy_id(CopyId::new(vec![1]));
        builder.file(mode_path, b"content");
    });
    let after = create_tree_with(repo, |builder| {
        builder.file(binary_path, b"after\0");
        builder
            .file(copy_path, b"content")
            .copy_id(CopyId::new(vec![2]));
        builder.file(mode_path, b"content").executable(true);
    });
    let plan = DiffSelection::build(
        DiffSelectionSource::Trees {
            transition: CopyRecords::default()
                .bind(Diff::new(before.clone(), after.clone()))
                .block_on()?
                .into(),
            tree_labels: Diff::new("before", "after"),
            context: DiffHunkContext::TreePair,
        }
        .capture(repo.as_ref())
        .block_on()?,
        &EverythingMatcher,
    )
    .block_on()?;

    let hunks = plan.hunks();
    assert_eq!(hunks.len(), 3);
    let expected_path = repo_path(expected_path_name);
    let hunk = hunks
        .iter()
        .find(|hunk| hunk.path() == expected_path)
        .unwrap();
    let HunkPresentation::Path(view) = hunk else {
        panic!("change at {expected_path:?} should be a path atom");
    };
    assert_eq!(view.details().2, expected_reason);
    let binary_id = hunks
        .iter()
        .find(|hunk| hunk.path() == binary_path)
        .map(|hunk| *hunk.id())
        .unwrap();
    let selected = plan
        .select(&[binary_id.into()], SelectionAction::Select)
        .block_on()?;
    let binary_value = selected
        .path_value(binary_path)
        .block_on()?
        .into_resolved()
        .unwrap();
    let Some(jj_lib::backend::TreeValue::File {
        id,
        executable: _,
        copy_id: _,
    }) = binary_value
    else {
        panic!("selected binary path should be a file");
    };
    assert_eq!(
        testutils::read_file(repo.store(), binary_path, &id),
        b"after\0"
    );
    assert_eq!(
        selected.path_value(copy_path).block_on()?,
        before.path_value(copy_path).block_on()?
    );
    assert_eq!(
        selected.path_value(mode_path).block_on()?,
        before.path_value(mode_path).block_on()?
    );
    Ok(())
}

#[test_case("added", PathHunkReason::FilePresence; "added file")]
#[test_case("deleted", PathHunkReason::FilePresence; "deleted file")]
#[test_case("symlink", PathHunkReason::NonFile; "symlink")]
#[test_case("submodule", PathHunkReason::NonFile; "submodule")]
fn test_structural_changes_are_path_atoms(
    expected_path_name: &str,
    expected_reason: PathHunkReason,
) -> TestResult {
    let test_repo = TestRepo::init();
    let repo = &test_repo.repo;
    let added_path = repo_path("added");
    let deleted_path = repo_path("deleted");
    let symlink_path = repo_path("symlink");
    let submodule_path = repo_path("submodule");
    let before_submodule = repo.store().root_commit_id().clone();
    let mut after_submodule = before_submodule.as_bytes().to_vec();
    after_submodule[0] ^= 1;
    let after_submodule = CommitId::new(after_submodule);
    let before = create_tree_with(repo, |builder| {
        builder.file(deleted_path, "deleted\n");
        builder.symlink(symlink_path, "before-target");
        builder.submodule(submodule_path, before_submodule);
    });
    let after = create_tree_with(repo, |builder| {
        builder.file(added_path, "added\n");
        builder.symlink(symlink_path, "after-target");
        builder.submodule(submodule_path, after_submodule);
    });
    let plan = DiffSelection::build(
        DiffSelectionSource::Trees {
            transition: CopyRecords::default()
                .bind(Diff::new(before.clone(), after.clone()))
                .block_on()?
                .into(),
            tree_labels: Diff::new("before", "after"),
            context: DiffHunkContext::TreePair,
        }
        .capture(repo.as_ref())
        .block_on()?,
        &EverythingMatcher,
    )
    .block_on()?;

    let hunks = plan.hunks();
    let expected_path = repo_path(expected_path_name);
    let hunk = hunks
        .iter()
        .find(|hunk| hunk.path() == expected_path)
        .unwrap();
    let HunkPresentation::Path(view) = hunk else {
        panic!("structural change at {expected_path:?} should be a path atom");
    };
    let (hunk_path, values, reason) = view.details();
    assert_eq!(hunk_path, expected_path);
    assert_eq!(values.before, before.path_value(expected_path).block_on()?);
    assert_eq!(values.after, after.path_value(expected_path).block_on()?);
    assert_eq!(reason, expected_reason);
    Ok(())
}

#[test]
fn test_file_to_tree_transition_is_one_indivisible_atom() -> TestResult {
    let test_repo = TestRepo::init();
    let repo = &test_repo.repo;
    let path = repo_path("node");
    let selected_child = path.join(repo_path_component("selected"));
    let other_child = path.join(repo_path_component("other"));
    let before = create_tree(repo, &[(path, "before\n")]);
    let after = create_tree(
        repo,
        &[(&selected_child, "selected\n"), (&other_child, "other\n")],
    );
    let plan = DiffSelection::build(
        DiffSelectionSource::Trees {
            transition: CopyRecords::default()
                .bind(Diff::new(before.clone(), after.clone()))
                .block_on()?
                .into(),
            tree_labels: Diff::new("before", "after"),
            context: DiffHunkContext::TreePair,
        }
        .capture(repo.as_ref())
        .block_on()?,
        &FilesMatcher::new([&selected_child]),
    )
    .block_on()?;

    let hunks = plan.hunks();
    let [hunk] = hunks.as_slice() else {
        panic!("the structural transition should be one atom");
    };
    assert_eq!(hunk.path(), path);
    let id = *hunk.id();
    let selected = plan
        .select(&[id.into()], SelectionAction::Select)
        .block_on()?;
    assert_eq!(selected.tree_ids_and_labels(), after.tree_ids_and_labels());
    assert_eq!(
        plan.select(&[], SelectionAction::Select)
            .block_on()?
            .tree_ids_and_labels(),
        before.tree_ids_and_labels()
    );
    Ok(())
}

#[test]
fn test_tree_file_conflict_is_a_path_atom() -> TestResult {
    let test_repo = TestRepo::init();
    let repo = &test_repo.repo;
    let path = repo_path("node");
    let child_path = path.join(repo_path_component("child"));
    let before = create_tree(repo, &[(path, "base\n")]);
    let mut after_builder = TestThreeWayMergeTreeBuilder::new(repo.store().clone());
    after_builder.base().file(path, "base\n");
    after_builder.parent1().file(path, "left\n");
    after_builder.parent2().file(&child_path, "right\n");
    let after = after_builder.write_merged_tree();
    let plan = DiffSelection::build(
        DiffSelectionSource::Trees {
            transition: CopyRecords::default()
                .bind(Diff::new(before, after))
                .block_on()?
                .into(),
            tree_labels: Diff::new("before", "after"),
            context: DiffHunkContext::TreePair,
        }
        .capture(repo.as_ref())
        .block_on()?,
        &EverythingMatcher,
    )
    .block_on()?;

    let hunks = plan.hunks();
    let hunk = hunks.iter().find(|hunk| hunk.path() == path).unwrap();
    let HunkPresentation::Path(view) = hunk else {
        panic!("tree/file conflict should be a path atom");
    };
    assert_eq!(view.details().2, PathHunkReason::NonFile);
    Ok(())
}

#[test]
fn test_path_hunk_reason_display() {
    assert_eq!(PathHunkReason::FilePresence.to_string(), "file presence");
    assert_eq!(PathHunkReason::NonFile.to_string(), "non-file value");
    assert_eq!(PathHunkReason::Binary.to_string(), "binary content");
    assert_eq!(PathHunkReason::FileMetadata.to_string(), "file metadata");
    assert_eq!(PathHunkReason::FileIdentity.to_string(), "file identity");
}

#[test]
fn test_select_conflict_content_without_materialized_markers() -> TestResult {
    let test_repo = TestRepo::init();
    let repo = &test_repo.repo;
    let path = repo_path("file");
    let before = create_tree(repo, &[(path, "first base\nmiddle\nlast base\n")]);
    let mut after_builder = TestThreeWayMergeTreeBuilder::new(repo.store().clone());
    after_builder
        .base()
        .file(path, "first base\nmiddle\nlast base\n");
    after_builder
        .parent1()
        .file(path, "first left\nmiddle\nlast left\n");
    after_builder
        .parent2()
        .file(path, "first right\nmiddle\nlast right\n");
    let after = after_builder.write_merged_tree();
    let plan = DiffSelection::build(
        DiffSelectionSource::Trees {
            transition: CopyRecords::default()
                .bind(Diff::new(before.clone(), after.clone()))
                .block_on()?
                .into(),
            tree_labels: Diff::new("parent", "working copy"),
            context: DiffHunkContext::TreePair,
        }
        .capture(repo.as_ref())
        .block_on()?,
        &EverythingMatcher,
    )
    .block_on()?;

    assert_eq!(plan.hunks().len(), 2);
    let first_id = *plan.hunks().first().unwrap().id();
    let selected = plan
        .select(&[first_id.into()], SelectionAction::Select)
        .block_on()?;
    let file_ids = selected
        .path_value(path)
        .block_on()?
        .to_file_merge()
        .unwrap();
    let contents = file_ids.map(|id| {
        BString::new(testutils::read_file(
            repo.store(),
            path,
            id.as_ref().unwrap(),
        ))
    });
    assert_eq!(
        contents,
        Merge::from_vec(vec![
            BString::from("first left\nmiddle\nlast base\n"),
            BString::from("first base\nmiddle\nlast base\n"),
            BString::from("first right\nmiddle\nlast base\n"),
        ])
    );
    assert_eq!(
        plan.select(&[], SelectionAction::Select)
            .block_on()?
            .tree_ids(),
        before.tree_ids()
    );
    let all_ids = plan
        .hunks()
        .iter()
        .map(|hunk| HunkSelection::from(*hunk.id()))
        .collect::<Vec<_>>();
    assert_eq!(
        plan.select(&all_ids, SelectionAction::Select)
            .block_on()?
            .tree_ids(),
        after.tree_ids()
    );

    let resolution = DiffSelection::build(
        DiffSelectionSource::Trees {
            transition: CopyRecords::default()
                .bind(Diff::new(after.clone(), before.clone()))
                .block_on()?
                .into(),
            tree_labels: Diff::new("conflict", "resolution"),
            context: DiffHunkContext::TreePair,
        }
        .capture(repo.as_ref())
        .block_on()?,
        &EverythingMatcher,
    )
    .block_on()?;
    let first_id = *resolution.hunks().first().unwrap().id();
    let selected = resolution
        .select(&[first_id.into()], SelectionAction::Select)
        .block_on()?;
    let file_ids = selected
        .path_value(path)
        .block_on()?
        .to_file_merge()
        .unwrap();
    let contents = file_ids.map(|id| {
        BString::new(testutils::read_file(
            repo.store(),
            path,
            id.as_ref().unwrap(),
        ))
    });
    assert_eq!(
        contents,
        Merge::from_vec(vec![
            BString::from("first base\nmiddle\nlast left\n"),
            BString::from("first base\nmiddle\nlast base\n"),
            BString::from("first base\nmiddle\nlast right\n"),
        ])
    );
    Ok(())
}

#[test]
fn test_select_conflicts_with_different_arities() -> TestResult {
    let test_repo = TestRepo::init();
    let repo = &test_repo.repo;
    let store = repo.store();
    let path = repo_path("file");
    let before_trees = [
        create_single_tree(repo, &[(path, "before left\nmiddle\ntail left\n")]),
        create_single_tree(repo, &[(path, "before base\nmiddle\ntail base\n")]),
        create_single_tree(repo, &[(path, "before right\nmiddle\ntail right\n")]),
    ];
    let after_trees = [
        create_single_tree(repo, &[(path, "after one\nmiddle\ntail one\n")]),
        create_single_tree(repo, &[(path, "after base one\nmiddle\ntail base one\n")]),
        create_single_tree(repo, &[(path, "after two\nmiddle\ntail two\n")]),
        create_single_tree(repo, &[(path, "after base two\nmiddle\ntail base two\n")]),
        create_single_tree(repo, &[(path, "after three\nmiddle\ntail three\n")]),
    ];
    let before = MergedTree::new(
        store.clone(),
        Merge::from_vec(before_trees.map(|tree| tree.id().clone()).to_vec()),
        ConflictLabels::from_merge(Merge::from_vec(vec![
            "before left".to_owned(),
            "before base".to_owned(),
            "before right".to_owned(),
        ])),
    );
    let after = MergedTree::new(
        store.clone(),
        Merge::from_vec(after_trees.map(|tree| tree.id().clone()).to_vec()),
        ConflictLabels::from_merge(Merge::from_vec(vec![
            "after one".to_owned(),
            "after base one".to_owned(),
            "after two".to_owned(),
            "after base two".to_owned(),
            "after three".to_owned(),
        ])),
    );
    let plan = DiffSelection::build(
        DiffSelectionSource::Trees {
            transition: CopyRecords::default()
                .bind(Diff::new(before.clone(), after.clone()))
                .block_on()?
                .into(),
            tree_labels: Diff::new("before", "after"),
            context: DiffHunkContext::TreePair,
        }
        .capture(repo.as_ref())
        .block_on()?,
        &EverythingMatcher,
    )
    .block_on()?;

    assert_eq!(plan.hunks().len(), 2);
    let first_id = *plan.hunks().first().unwrap().id();
    let selected = plan
        .select(&[first_id.into()], SelectionAction::Select)
        .block_on()?;
    assert_eq!(selected.tree_ids().as_slice().len(), 7);
    assert_eq!(
        plan.select(&[], SelectionAction::Select)
            .block_on()?
            .tree_ids_and_labels(),
        before.tree_ids_and_labels()
    );
    let all_ids = plan
        .hunks()
        .iter()
        .map(|hunk| HunkSelection::from(*hunk.id()))
        .collect::<Vec<_>>();
    assert_eq!(
        plan.select(&all_ids, SelectionAction::Select)
            .block_on()?
            .tree_ids_and_labels(),
        after.tree_ids_and_labels()
    );
    Ok(())
}

#[test]
fn test_structural_atom_ignores_matcher_below_boundary() -> TestResult {
    let test_repo = TestRepo::init();
    let repo = &test_repo.repo;
    let source = repo_path("source");
    let node = repo_path("node");
    let target = node.join(repo_path_component("child"));
    let other = node.join(repo_path_component("other"));
    let before = create_tree(repo, &[(source, "contents\n"), (node, "blocking\n")]);
    let after = create_tree(repo, &[(&target, "contents\n"), (&other, "other\n")]);
    let mut copy_records = CopyRecords::default();
    copy_records.add_records([CopyRecord {
        target: target.clone(),
        source: source.to_owned(),
        target_commit: CommitId::new(vec![2]),
        source_commit: CommitId::new(vec![1]),
        source_file: file_id_at(&before, source),
    }]);
    let build = |matcher: &dyn jj_lib::matchers::Matcher| {
        DiffSelection::build(
            DiffSelectionSource::Trees {
                transition: copy_records
                    .bind(Diff::new(before.clone(), after.clone()))
                    .block_on()?
                    .into(),
                tree_labels: Diff::new("before", "after"),
                context: DiffHunkContext::TreePair,
            }
            .capture(repo.as_ref())
            .block_on()?,
            matcher,
        )
        .block_on()
    };
    let unfiltered = build(&EverythingMatcher)?;
    // The matcher reaches only a path that has nothing to do with the rename.
    let filtered = build(&FilesMatcher::new([&other]))?;

    let unfiltered_hunks = unfiltered.hunks();
    let [unfiltered_hunk] = unfiltered_hunks.as_slice() else {
        panic!("the unfiltered structural transition should remain one atom");
    };
    let filtered_hunks = filtered.hunks();
    let [filtered_hunk] = filtered_hunks.as_slice() else {
        panic!("the structural transition should remain one atom");
    };
    assert_eq!(filtered_hunk.id(), unfiltered_hunk.id());
    let HunkPresentation::Path(view) = filtered_hunk else {
        panic!("the structural transition should be a path atom");
    };
    assert_eq!(view.removed_sources(), &[source.to_owned()]);
    let HunkPresentation::Path(unfiltered_view) = &unfiltered_hunk else {
        panic!("the structural transition should be a path atom");
    };
    assert_eq!(view.entries().count(), unfiltered_view.entries().count());
    assert_eq!(
        filtered
            .select(&[(*filtered_hunk.id()).into()], SelectionAction::Select)
            .block_on()?
            .tree_ids(),
        after.tree_ids()
    );
    Ok(())
}

#[test]
fn test_identical_atom_keeps_its_id_after_the_other_is_taken() -> TestResult {
    let test_repo = TestRepo::init();
    let repo = &test_repo.repo;
    let path = repo_path("file");
    let before = create_tree(repo, &[(path, "x\n1\n2\n3\nx\n")]);
    let after = create_tree(repo, &[(path, "y\n1\n2\n3\ny\n")]);
    let copy_records = CopyRecords::default();
    let plan = DiffSelection::build(
        DiffSelectionSource::Trees {
            transition: copy_records
                .bind(Diff::new(before, after.clone()))
                .block_on()?
                .into(),
            tree_labels: Diff::new("before", "after"),
            context: DiffHunkContext::TreePair,
        }
        .capture(repo.as_ref())
        .block_on()?,
        &EverythingMatcher,
    )
    .block_on()?;
    let [first, second] = plan
        .hunks()
        .iter()
        .map(|hunk| *hunk.id())
        .collect::<Vec<_>>()
        .try_into()
        .unwrap();
    let middle = plan
        .select(&[first.into()], SelectionAction::Select)
        .block_on()?;

    let remaining = DiffSelection::build(
        DiffSelectionSource::Trees {
            transition: copy_records
                .bind(Diff::new(middle, after))
                .block_on()?
                .into(),
            tree_labels: Diff::new("before", "after"),
            context: DiffHunkContext::TreePair,
        }
        .capture(repo.as_ref())
        .block_on()?,
        &EverythingMatcher,
    )
    .block_on()?;
    // The two changes are identical, but the lines around them are not, so
    // taking the first does not hand its ID to the second.
    let remaining_ids = remaining
        .hunks()
        .iter()
        .map(|hunk| *hunk.id())
        .collect::<Vec<_>>();
    assert_eq!(remaining_ids, [second]);
    Ok(())
}

#[test]
fn test_text_atom_contents_exclude_context() -> TestResult {
    let test_repo = TestRepo::init();
    let repo = &test_repo.repo;
    let path = repo_path("file");
    // Re-diffing the middle atom together with the `b` after it would align
    // that `b` with the removed one and number the wrong line.
    let before = create_tree(repo, &[(path, "a\nb\na\nb\n")]);
    let after = create_tree(repo, &[(path, "b\na\nb\na\n")]);
    let plan = DiffSelection::build(
        DiffSelectionSource::Trees {
            transition: CopyRecords::default()
                .bind(Diff::new(before, after))
                .block_on()?
                .into(),
            tree_labels: Diff::new("before", "after"),
            context: DiffHunkContext::TreePair,
        }
        .capture(repo.as_ref())
        .block_on()?,
        &EverythingMatcher,
    )
    .block_on()?;
    let hunks = plan.hunks();
    let (id, middle) = hunks
        .iter()
        .find_map(|hunk| match hunk {
            HunkPresentation::Text(text)
                if text.details().1.after.as_resolved() == Some(&BStr::new("")) =>
            {
                Some((*hunk.id(), text))
            }
            HunkPresentation::Text(_) | HunkPresentation::Path(_) | HunkPresentation::Copy(_) => {
                None
            }
        })
        .expect("one atom only removes lines");
    assert_eq!(
        middle.details().1.before.as_resolved(),
        Some(&BStr::new("b\na\n"))
    );
    assert_eq!(middle.context(), Diff::new(None, Some(BStr::new("b\n"))));

    let selected = plan
        .select(&[format!("{id}:-1").parse()?], SelectionAction::Select)
        .block_on()?;
    assert_eq!(
        testutils::read_file(repo.store(), path, &file_id_at(&selected, path)),
        b"a\na\nb\n"
    );
    Ok(())
}

#[test]
fn test_deselect_undoes_atoms_and_lines() -> TestResult {
    let test_repo = TestRepo::init();
    let repo = &test_repo.repo;
    let path = repo_path("file");
    let before = create_tree(repo, &[(path, "a\nb\nc\nd\ne\n")]);
    let after = create_tree(repo, &[(path, "a\nB1\nB2\nd\nE\n")]);
    let plan = DiffSelection::build(
        DiffSelectionSource::Trees {
            transition: CopyRecords::default()
                .bind(Diff::new(before.clone(), after.clone()))
                .block_on()?
                .into(),
            tree_labels: Diff::new("before", "after"),
            context: DiffHunkContext::TreePair,
        }
        .capture(repo.as_ref())
        .block_on()?,
        &EverythingMatcher,
    )
    .block_on()?;
    let [first, last] = plan
        .hunks()
        .iter()
        .map(|hunk| *hunk.id())
        .collect::<Vec<_>>()
        .try_into()
        .unwrap();
    let deselect_file = |selections: &[String]| -> TestResult<Vec<u8>> {
        let selections = selections
            .iter()
            .map(|selection| selection.parse())
            .collect::<Result<Vec<HunkSelection>, _>>()?;
        let tree = plan.select(&selections, SelectionAction::Undo).block_on()?;
        Ok(testutils::read_file(
            repo.store(),
            path,
            &file_id_at(&tree, path),
        ))
    };

    assert_eq!(
        plan.select(&[], SelectionAction::Undo)
            .block_on()?
            .tree_ids(),
        after.tree_ids()
    );
    assert_eq!(
        plan.select(&[first.into(), last.into()], SelectionAction::Undo)
            .block_on()?
            .tree_ids(),
        before.tree_ids()
    );
    assert_eq!(deselect_file(&[first.to_string()])?, b"a\nb\nc\nd\nE\n");
    // Undoing `-1` brings `b` back; the lines still applied keep the ordering
    // rule of `select`, with remaining removed lines before added ones.
    assert_eq!(
        deselect_file(&[format!("{first}:-1")])?,
        b"a\nb\nB1\nB2\nd\nE\n"
    );
    assert_eq!(deselect_file(&[format!("{first}:+2")])?, b"a\nB1\nd\nE\n");
    assert_eq!(
        deselect_file(&[format!("{first}:-1-2,+1-2")])?,
        b"a\nb\nc\nd\nE\n"
    );

    let error = plan
        .select(&[format!("{first}:-3").parse()?], SelectionAction::Undo)
        .block_on()
        .unwrap_err();
    match error {
        DiffSelectionError::LineNotInHunk { id, side, line } => {
            assert_eq!((id, side, line), (first, HunkLineSide::Removed, 3));
        }
        DiffSelectionError::HunkNotFound(_)
        | DiffSelectionError::LinesUnsupported(_)
        | DiffSelectionError::Backend(_) => panic!("unexpected error: {error}"),
    }
    Ok(())
}

proptest::proptest! {
    #[test]
    fn test_line_ranges_keep_large_endpoints_compact(
        first in 1usize..=usize::MAX,
        second in 1usize..=usize::MAX,
    ) {
        let start = first.min(second);
        let end = first.max(second);
        let id = "00000000000000000000000000000000";
        let selection = format!("{id}:+{start}-{end}").parse::<HunkSelection>().unwrap();
        let redundant = format!("{id}:+{end},+{start}-{end},+{start}").parse::<HunkSelection>().unwrap();
        assert_eq!(selection, redundant);
    }

    #[test]
    fn test_partial_selection_and_complement(
        first in 1usize..=3, second in 1usize..=3,
        side in proptest::prop_oneof![
            proptest::strategy::Just(HunkLineSide::Removed),
            proptest::strategy::Just(HunkLineSide::Added),
        ],
    ) {
        let test_repo = TestRepo::init();
        let repo = &test_repo.repo;
        let path = repo_path("file");
        let before_lines = ["old 1\n", "old 2\n", "old 3\n"];
        let after_lines = ["new 1\n", "new 2\n", "new 3\n"];
        let before = create_tree(repo, &[(path, &before_lines.concat())]);
        let after = create_tree(repo, &[(path, &after_lines.concat())]);
        let captured = DiffSelectionSource::Trees {
            transition: CopyRecords::default()
                .bind(Diff::new(before, after)).block_on().unwrap().into(),
            tree_labels: Diff::new("before", "after"), context: DiffHunkContext::TreePair,
        }.capture(repo.as_ref()).block_on().unwrap();
        let plan = DiffSelection::build(captured, &EverythingMatcher).block_on().unwrap();
        let id = *plan.hunks()[0].id();
        let range = first.min(second)..=first.max(second);
        let sign = match side { HunkLineSide::Removed => '-', HunkLineSide::Added => '+' };
        let selection = format!("{id}:{sign}{}-{}", range.start(), range.end()).parse().unwrap();
        let (expected_selected, expected_undone) = match side {
            HunkLineSide::Removed => (
                before_lines.iter().enumerate()
                    .filter(|(i, _)| !range.contains(&(i + 1)))
                    .map(|(_, line)| *line).collect::<String>(),
                before_lines.iter().enumerate()
                    .filter(|(i, _)| range.contains(&(i + 1)))
                    .map(|(_, line)| *line).chain(after_lines).collect::<String>(),
            ),
            HunkLineSide::Added => (
                before_lines.into_iter().chain(
                    after_lines.iter().enumerate()
                        .filter(|(i, _)| range.contains(&(i + 1)))
                        .map(|(_, line)| *line),
                ).collect::<String>(),
                after_lines.iter().enumerate()
                    .filter(|(i, _)| !range.contains(&(i + 1)))
                    .map(|(_, line)| *line).collect::<String>(),
            ),
        };
        let selected = plan
            .select(std::slice::from_ref(&selection), SelectionAction::Select)
            .block_on().unwrap();
        let undone = plan.select(&[selection], SelectionAction::Undo).block_on().unwrap();
        assert_eq!(
            testutils::read_file(repo.store(), path, &file_id_at(&selected, path)),
            expected_selected.as_bytes(),
        );
        assert_eq!(
            testutils::read_file(repo.store(), path, &file_id_at(&undone, path)),
            expected_undone.as_bytes(),
        );
    }
}

#[test]
fn test_selection_rejects_all_requests_before_writing_files() -> TestResult {
    let test_repo = TestRepo::init();
    let repo = &test_repo.repo;
    let a = repo_path("a");
    let z = repo_path("z");
    let before = create_tree(repo, &[(a, "old a\n"), (z, "old z\n")]);
    let after = create_tree(repo, &[(a, "new a\n"), (z, "new z\n")]);
    // The test backend keys files by both path and content. Writing this oracle
    // at another path does not make the partial result readable at `a`.
    let partial_id = repo
        .store()
        .write_file(repo_path("oracle"), &mut b"old a\nnew a\n".as_slice())
        .block_on()?;
    let captured = DiffSelectionSource::Trees {
        transition: CopyRecords::default()
            .bind(Diff::new(before, after))
            .block_on()?
            .into(),
        tree_labels: Diff::new("before", "after"),
        context: DiffHunkContext::TreePair,
    }
    .capture(repo.as_ref())
    .block_on()?;
    let plan = DiffSelection::build(captured, &EverythingMatcher).block_on()?;
    let ids = plan
        .hunks()
        .iter()
        .map(|hunk| *hunk.id())
        .collect::<Vec<_>>();
    let error = plan
        .select(
            &[
                format!("{}:+1", ids[0]).parse()?,
                format!("{}:+2", ids[1]).parse()?,
            ],
            SelectionAction::Select,
        )
        .block_on()
        .unwrap_err();
    let DiffSelectionError::LineNotInHunk { id, side, line } = error else {
        panic!("unexpected selection error: {error}");
    };
    assert_eq!(id, ids[1]);
    assert_eq!(side, HunkLineSide::Added);
    assert_eq!(line, 2);
    match repo.store().read_file(a, &partial_id).block_on() {
        Err(jj_lib::backend::BackendError::ObjectNotFound { .. }) => {}
        Err(error) => panic!("unexpected read error: {error}"),
        Ok(_) => panic!("invalid request persisted an earlier partial result"),
    }
    Ok(())
}

#[test]
fn test_whole_atom_absorbs_out_of_bounds_range() -> TestResult {
    let test_repo = TestRepo::init();
    let repo = &test_repo.repo;
    let path = repo_path("file");
    let before = create_tree(repo, &[(path, "old\n")]);
    let after = create_tree(repo, &[(path, "new\n")]);
    let captured = DiffSelectionSource::Trees {
        transition: CopyRecords::default()
            .bind(Diff::new(before, after.clone()))
            .block_on()?
            .into(),
        tree_labels: Diff::new("before", "after"),
        context: DiffHunkContext::TreePair,
    }
    .capture(repo.as_ref())
    .block_on()?;
    let plan = DiffSelection::build(captured, &EverythingMatcher).block_on()?;
    let id = *plan.hunks()[0].id();
    let result = plan
        .select(
            &[format!("{id}:+1-{}", usize::MAX).parse()?, id.into()],
            SelectionAction::Select,
        )
        .block_on()?;
    assert_eq!(result.tree_ids(), after.tree_ids());
    Ok(())
}
