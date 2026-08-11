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

use test_case::test_case;

use crate::common::TestEnvironment;
use crate::common::create_commit_with_files;

#[test]
fn test_text_hunk_ids_precede_their_changes() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "repo"]).success();
    let work_dir = test_env.work_dir("repo");
    create_commit_with_files(
        &work_dir,
        "base",
        &[],
        &[("file", "first before\nmiddle\nlast before\n")],
    );
    work_dir.run_jj(["new", "base"]).success();
    work_dir.write_file("file", "first after\nmiddle\nlast after\n");

    let output = work_dir.run_jj(["diff", "--hunks"]).success();
    let lines = output.stdout.raw().lines().collect::<Vec<_>>();
    let hunk_positions = lines
        .iter()
        .enumerate()
        .filter_map(|(index, line)| line.starts_with("hunk ").then_some(index))
        .collect::<Vec<_>>();
    assert_eq!(hunk_positions.len(), 2);
    let first_hunk = &lines[hunk_positions[0] + 1..hunk_positions[1]];
    let second_hunk = &lines[hunk_positions[1] + 1..];
    assert!(
        first_hunk
            .iter()
            .any(|line| line.contains("first before") || line.contains("first after"))
    );
    assert!(
        !first_hunk
            .iter()
            .any(|line| line.contains("last before") || line.contains("last after"))
    );
    assert!(
        second_hunk
            .iter()
            .any(|line| line.contains("last before") || line.contains("last after"))
    );
    assert!(
        !second_hunk
            .iter()
            .any(|line| line.contains("first before") || line.contains("first after"))
    );
}

#[test_case(&[]; "snapshot conflicts")]
#[test_case(&["--config", "diff.color-words.conflict=pair"]; "pair conflicts")]
fn test_conflict_hunk_ids_precede_their_changes_in_each_mode(extra_args: &[&str]) {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "repo"]).success();
    let work_dir = test_env.work_dir("repo");
    create_commit_with_files(
        &work_dir,
        "base",
        &[],
        &[("file", "base\nmiddle\ntail base\n")],
    );
    create_commit_with_files(
        &work_dir,
        "left",
        &["base"],
        &[("file", "left\nmiddle\ntail changed\n")],
    );
    create_commit_with_files(
        &work_dir,
        "right",
        &["base"],
        &[("file", "right\nmiddle\ntail changed\n")],
    );
    create_commit_with_files(&work_dir, "conflict", &["left", "right"], &[]);
    work_dir.run_jj(["new", "conflict"]).success();
    let conflict_contents = std::fs::read_to_string(work_dir.root().join("file")).unwrap();
    work_dir.write_file(
        "file",
        conflict_contents
            .replace("left\n", "left child\n")
            .replace("tail changed", "tail changed child"),
    );
    work_dir
        .run_jj(["bookmark", "create", "-r@", "modified"])
        .success();

    work_dir.run_jj(["new", "conflict"]).success();
    work_dir.write_file("file", "left resolved\nmiddle\ntail changed child\n");
    work_dir
        .run_jj(["bookmark", "create", "-r@", "resolved"])
        .success();

    let created = work_dir
        .run_jj(
            extra_args
                .iter()
                .copied()
                .chain(["diff", "--hunks", "--from", "base", "--to", "conflict"])
                .collect::<Vec<_>>(),
        )
        .success();
    let created_lines = created.stdout.raw().lines().collect::<Vec<_>>();
    assert!(created.stdout.raw().contains("Created conflict in file:\n"));
    let created_hunk_positions = created_lines
        .iter()
        .enumerate()
        .filter_map(|(index, line)| line.starts_with("hunk ").then_some(index))
        .collect::<Vec<_>>();
    assert_eq!(created_hunk_positions.len(), 2);
    let created_first_hunk =
        &created_lines[created_hunk_positions[0] + 1..created_hunk_positions[1]];
    let created_second_hunk = &created_lines[created_hunk_positions[1] + 1..];
    assert!(
        created_first_hunk
            .iter()
            .any(|line| line.contains("left") || line.contains("right"))
    );
    assert!(
        !created_first_hunk
            .iter()
            .any(|line| line.contains("tail changed"))
    );
    assert!(
        created_second_hunk
            .iter()
            .any(|line| line.contains("tail changed"))
    );
    assert!(
        !created_second_hunk
            .iter()
            .any(|line| line.contains("left") || line.contains("right"))
    );

    let modified = work_dir
        .run_jj(
            extra_args
                .iter()
                .copied()
                .chain(["diff", "--hunks", "--from", "conflict", "--to", "modified"])
                .collect::<Vec<_>>(),
        )
        .success();
    let modified_lines = modified.stdout.raw().lines().collect::<Vec<_>>();
    assert!(
        modified
            .stdout
            .raw()
            .contains("Modified conflict in file:\n")
    );
    let modified_hunk_positions = modified_lines
        .iter()
        .enumerate()
        .filter_map(|(index, line)| line.starts_with("hunk ").then_some(index))
        .collect::<Vec<_>>();
    assert_eq!(modified_hunk_positions.len(), 2);
    let modified_first_hunk =
        &modified_lines[modified_hunk_positions[0] + 1..modified_hunk_positions[1]];
    let modified_second_hunk = &modified_lines[modified_hunk_positions[1] + 1..];
    assert!(
        modified_first_hunk
            .iter()
            .any(|line| line.contains("left child"))
    );
    assert!(
        !modified_first_hunk
            .iter()
            .any(|line| line.contains("tail changed child"))
    );
    assert!(
        modified_second_hunk
            .iter()
            .any(|line| line.contains("tail changed child"))
    );
    assert!(
        !modified_second_hunk
            .iter()
            .any(|line| line.contains("left child"))
    );

    let resolved = work_dir
        .run_jj(
            extra_args
                .iter()
                .copied()
                .chain(["diff", "--hunks", "--from", "conflict", "--to", "resolved"])
                .collect::<Vec<_>>(),
        )
        .success();
    let resolved_lines = resolved.stdout.raw().lines().collect::<Vec<_>>();
    assert!(
        resolved
            .stdout
            .raw()
            .contains("Resolved conflict in file:\n")
    );
    let resolved_hunk_positions = resolved_lines
        .iter()
        .enumerate()
        .filter_map(|(index, line)| line.starts_with("hunk ").then_some(index))
        .collect::<Vec<_>>();
    assert_eq!(resolved_hunk_positions.len(), 2);
    let resolved_first_hunk =
        &resolved_lines[resolved_hunk_positions[0] + 1..resolved_hunk_positions[1]];
    let resolved_second_hunk = &resolved_lines[resolved_hunk_positions[1] + 1..];
    assert!(
        resolved_first_hunk
            .iter()
            .any(|line| line.contains("left resolved"))
    );
    assert!(
        !resolved_first_hunk
            .iter()
            .any(|line| line.contains("tail changed child"))
    );
    assert!(
        resolved_second_hunk
            .iter()
            .any(|line| line.contains("tail changed child"))
    );
    assert!(
        !resolved_second_hunk
            .iter()
            .any(|line| line.contains("left resolved"))
    );
}

#[test]
fn test_structural_hunk_id_owns_removed_and_renamed_entries() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "repo"]).success();
    let work_dir = test_env.work_dir("repo");
    work_dir.write_file("source", "contents\n");
    work_dir.write_file("node", "blocking\n");
    work_dir.run_jj(["commit", "-m", "base"]).success();
    std::fs::remove_file(work_dir.root().join("node")).unwrap();
    std::fs::create_dir(work_dir.root().join("node")).unwrap();
    std::fs::rename(
        work_dir.root().join("source"),
        work_dir.root().join("node/child"),
    )
    .unwrap();

    let output = work_dir.run_jj(["diff", "--hunks"]).success();
    let lines = output.stdout.raw().lines().collect::<Vec<_>>();
    let hunk_positions = lines
        .iter()
        .enumerate()
        .filter_map(|(index, line)| line.starts_with("hunk ").then_some(index))
        .collect::<Vec<_>>();
    assert_eq!(hunk_positions.len(), 1);
    let hunk = &lines[hunk_positions[0] + 1..];
    assert!(
        hunk.iter()
            .any(|line| line.contains("Removed regular file node:"))
    );
    assert!(
        hunk.iter()
            .any(|line| line.contains("Removed source source:"))
    );
    assert!(hunk.iter().any(|line| line.contains("node/child")));
}
