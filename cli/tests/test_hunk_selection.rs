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
fn test_diff_hunks_color() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "repo"]).success();
    let work_dir = test_env.work_dir("repo");
    work_dir.write_file("file", "base\n");
    work_dir.run_jj(["commit", "-m", "base"]).success();
    work_dir.write_file("file", "first\n");
    work_dir.run_jj(["commit", "-m", "first"]).success();
    work_dir.write_file("file", "second\n");

    let output = work_dir
        .run_jj(["diff", "--hunks", "--color=always", "-r", "@-::@"])
        .success();
    let hunk_lines = output
        .stdout
        .raw()
        .lines()
        .filter(|line| line.contains("hunk"))
        .collect::<Vec<_>>();
    assert_eq!(hunk_lines.len(), 2);
    for hunk_line in hunk_lines {
        assert!(hunk_line.starts_with("\x1b[2m\x1b[38;5;6mhunk"));
        assert!(hunk_line.contains("\x1b[1m"));
        let source = hunk_line.find("source=").unwrap();
        assert!(hunk_line[..source].ends_with("\x1b[2m"));
    }
}

#[test]
fn test_diff_hunks_and_commit_selection() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "repo"]).success();
    let work_dir = test_env.work_dir("repo");
    work_dir.write_file("file", "first before\nmiddle\nlast before\n");
    work_dir.run_jj(["commit", "-m", "base"]).success();
    work_dir.write_file("file", "first after\nmiddle\nlast after\n");

    let listing = work_dir.run_jj(["diff", "--hunks"]).success();
    let hunk_ids = listing
        .stdout
        .raw()
        .lines()
        .filter_map(|line| line.strip_prefix("hunk "))
        .map(|line| line.split_whitespace().next().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(hunk_ids.len(), 2);
    let first_id = hunk_ids[0];
    assert_eq!(first_id.len(), 32);
    assert!(
        listing
            .stdout
            .raw()
            .starts_with("Modified regular file file:\n")
    );
    assert!(listing.stdout.raw().contains(&format!(
        "\nhunk {first_id}\n   1     : first before\n        1: first after\n         : middle\n"
    )));
    assert!(!listing.stdout.raw().contains("before:"));
    assert!(!listing.stdout.raw().contains("after:"));

    let output = work_dir
        .run_jj(["commit", "--hunk", first_id, "-m", "selected"])
        .success();
    assert!(output.stderr.raw().contains("Remaining changes:\n"));
    assert!(output.stderr.raw().contains("  file: 1 hunk\n"));
    assert!(!output.stderr.raw().contains(" text file"));
    work_dir.write_file("other", "contents\n");
    let listing = work_dir.run_jj(["diff", "--hunks"]).success();
    let remaining_id = listing
        .stdout
        .raw()
        .lines()
        .find_map(|line| line.strip_prefix("hunk "))
        .and_then(|line| line.split_whitespace().next())
        .unwrap();
    assert_ne!(remaining_id, first_id);
    assert!(
        listing
            .stdout
            .raw()
            .contains(&format!("hunk {remaining_id}"))
    );
    assert_eq!(
        work_dir
            .run_jj(["file", "show", "-r", "@-", "file"])
            .success()
            .stdout
            .raw(),
        "first after\nmiddle\nlast before\n"
    );
    assert_eq!(
        work_dir
            .run_jj(["file", "show", "-r", "@", "file"])
            .success()
            .stdout
            .raw(),
        "first after\nmiddle\nlast after\n"
    );
    let output = work_dir
        .run_jj(["commit", "--hunk", remaining_id, "-m", "remaining"])
        .success();
    assert!(output.stderr.raw().contains("Remaining changes:\n"));
    assert!(output.stderr.raw().contains("  other: 1 hunk\n"));
    assert!(!output.stderr.raw().contains(" path other"));
}

#[test]
fn test_remaining_hunk_summary_counts_by_file() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "repo"]).success();
    let work_dir = test_env.work_dir("repo");
    work_dir.write_file(
        "file",
        "first before\nseparator one\nmiddle before\nseparator two\nlast before\n",
    );
    work_dir.run_jj(["commit", "-m", "base"]).success();
    work_dir.write_file(
        "file",
        "first after\nseparator one\nmiddle after\nseparator two\nlast after\n",
    );
    work_dir.write_file("other", "contents\n");
    let listing = work_dir.run_jj(["diff", "--hunks"]).success();
    let hunk_ids = listing
        .stdout
        .raw()
        .lines()
        .filter_map(|line| line.strip_prefix("hunk "))
        .map(|line| line.split_whitespace().next().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(hunk_ids.len(), 4);

    let output = work_dir
        .run_jj(["commit", "--hunk", hunk_ids[0], "-m", "selected"])
        .success();
    assert!(
        output
            .stderr
            .raw()
            .contains("Remaining changes:\n  file: 2 hunks\n  other: 1 hunk\n")
    );
    assert!(!output.stderr.raw().contains(hunk_ids[1]));
    assert!(!output.stderr.raw().contains(hunk_ids[2]));
    assert!(!output.stderr.raw().contains(hunk_ids[3]));
}

#[test]
fn test_restore_hunk_summary_uses_rewritten_destination() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "repo"]).success();
    let work_dir = test_env.work_dir("repo");
    work_dir.write_file("base-file", "base\n");
    work_dir.run_jj(["commit", "-m", "base"]).success();
    work_dir.write_file("target-file", "target\n");
    work_dir.run_jj(["commit", "-m", "target"]).success();
    work_dir.write_file("source-file", "source\n");
    work_dir.run_jj(["commit", "-m", "source"]).success();
    work_dir.write_file("unrelated", "working copy\n");

    let listing = work_dir
        .run_jj(["diff", "--hunks", "--from", "@-", "--to", "@--"])
        .success();
    let hunk_id = listing
        .stdout
        .raw()
        .lines()
        .find_map(|line| line.strip_prefix("hunk "))
        .and_then(|line| line.split_whitespace().next())
        .unwrap();
    let output = work_dir
        .run_jj([
            "restore", "--from", "@-", "--into", "@--", "--hunk", hunk_id,
        ])
        .success();
    assert!(
        output
            .stderr
            .raw()
            .contains("Remaining changes:\n  source-file: 1 hunk\n  target-file: 1 hunk\n")
    );
    assert!(!output.stderr.raw().contains("unrelated"));
}

#[test]
fn test_split_hunk_summary_uses_non_working_copy_revision() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "repo"]).success();
    let work_dir = test_env.work_dir("repo");
    work_dir.write_file("base-file", "base\n");
    work_dir.run_jj(["commit", "-m", "base"]).success();
    work_dir.write_file("first-file", "first\n");
    work_dir.write_file("second-file", "second\n");
    work_dir.run_jj(["commit", "-m", "target"]).success();
    work_dir.write_file("unrelated", "working copy\n");

    let listing = work_dir.run_jj(["diff", "--hunks", "-r", "@-"]).success();
    let hunk_id = listing
        .stdout
        .raw()
        .lines()
        .find_map(|line| line.strip_prefix("hunk "))
        .and_then(|line| line.split_whitespace().next())
        .unwrap();
    let output = work_dir
        .run_jj(["split", "-r", "@-", "--hunk", hunk_id, "-m", "selected"])
        .success();
    assert!(
        output
            .stderr
            .raw()
            .contains("Remaining changes:\n  second-file: 1 hunk\n")
    );
    assert!(!output.stderr.raw().contains("unrelated"));
}

#[test]
fn test_squash_hunk_summary_aggregates_non_working_copy_sources() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "repo"]).success();
    let work_dir = test_env.work_dir("repo");
    work_dir.write_file("first-file", "base\n");
    work_dir.write_file("second-file", "base\n");
    work_dir.run_jj(["commit", "-m", "base"]).success();
    work_dir.write_file("first-file", "first\n");
    work_dir.run_jj(["commit", "-m", "first"]).success();
    work_dir.write_file("second-file", "second\n");
    work_dir.run_jj(["describe", "-m", "second"]).success();
    work_dir.run_jj(["new"]).success();
    work_dir.write_file("unrelated", "working copy\n");

    let listing = work_dir
        .run_jj(["diff", "--hunks", "-r", "@--::@-"])
        .success();
    let first_file = listing.stdout.raw().split_once("first-file:").unwrap().1;
    let hunk_id = first_file
        .lines()
        .find_map(|line| line.strip_prefix("hunk "))
        .and_then(|line| line.split_whitespace().next())
        .unwrap();
    let output = work_dir
        .run_jj([
            "squash",
            "--from",
            "@--::@-",
            "--into",
            "@---",
            "--hunk",
            hunk_id,
            "--use-destination-message",
        ])
        .success();
    assert!(
        output
            .stderr
            .raw()
            .contains("Remaining changes:\n  second-file: 1 hunk\n")
    );
    assert!(!output.stderr.raw().contains("first-file"));
    assert!(!output.stderr.raw().contains("unrelated"));
}

#[test]
fn test_hunk_selection_from_files_and_inline() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "repo"]).success();
    let work_dir = test_env.work_dir("repo");
    for path in ["a", "b", "c", "d"] {
        work_dir.write_file(path, "before\n");
    }
    work_dir.run_jj(["commit", "-m", "base"]).success();
    for path in ["a", "b", "c", "d"] {
        work_dir.write_file(path, "after\n");
    }

    let listing = work_dir.run_jj(["diff", "--hunks"]).success();
    let hunk_ids = listing
        .stdout
        .raw()
        .lines()
        .filter_map(|line| line.strip_prefix("hunk "))
        .map(|line| line.split_whitespace().next().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(hunk_ids.len(), 4);
    std::fs::write(
        work_dir.root().join(".jj/first-hunks"),
        format!("# selected from the first file\n\n{}\n", hunk_ids[0]),
    )
    .unwrap();
    std::fs::write(
        work_dir.root().join(".jj/second-hunks"),
        format!("{}\n", hunk_ids[1]),
    )
    .unwrap();

    work_dir
        .run_jj([
            "squash",
            "--hunk-file",
            ".jj/first-hunks",
            "--hunk",
            hunk_ids[2],
            "--hunk-file",
            ".jj/second-hunks",
            "--use-destination-message",
        ])
        .success();
    let contents = ["a", "b", "c", "d"].map(|path| {
        work_dir
            .run_jj(["file", "show", "-r", "@-", path])
            .success()
            .stdout
            .raw()
            .to_owned()
    });
    assert_eq!(contents, ["after\n", "after\n", "after\n", "before\n"]);
}

#[test]
fn test_split_hunk_selection_from_stdin() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "repo"]).success();
    let work_dir = test_env.work_dir("repo");
    work_dir.write_file("a", "before\n");
    work_dir.write_file("b", "before\n");
    work_dir.run_jj(["commit", "-m", "base"]).success();
    work_dir.write_file("a", "after\n");
    work_dir.write_file("b", "after\n");
    let listing = work_dir.run_jj(["diff", "--hunks"]).success();
    let first_id = listing
        .stdout
        .raw()
        .lines()
        .find_map(|line| line.strip_prefix("hunk "))
        .and_then(|line| line.split_whitespace().next())
        .unwrap();

    work_dir
        .run_jj_with(|cmd| {
            cmd.args(["split", "--hunk-file", "-", "-m", "selected"])
                .write_stdin(format!("{first_id}\n"))
        })
        .success();
    assert_eq!(
        work_dir
            .run_jj(["file", "show", "-r", "@-", "a"])
            .success()
            .stdout
            .raw(),
        "after\n"
    );
    assert_eq!(
        work_dir
            .run_jj(["file", "show", "-r", "@-", "b"])
            .success()
            .stdout
            .raw(),
        "before\n"
    );
}

#[test_case("commit"; "commit")]
#[test_case("restore"; "restore")]
#[test_case("split"; "split")]
#[test_case("squash"; "squash")]
fn test_hunk_selection_invalid_file(command: &str) {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "repo"]).success();
    let work_dir = test_env.work_dir("repo");
    work_dir.write_file("file", "contents\n");

    std::fs::write(work_dir.root().join(".jj/bad-hunks"), "# comment\nabcd\n").unwrap();
    let output = work_dir.run_jj([command, "--hunk-file", ".jj/bad-hunks"]);
    assert!(!output.status.success());
    assert!(
        output
            .stderr
            .raw()
            .contains("Failed to parse hunk ID at .jj/bad-hunks:2")
    );
    assert!(
        output
            .stderr
            .raw()
            .contains("hunk ID must be 32 hexadecimal characters, got 4")
    );
}

#[test_case("commit"; "commit")]
#[test_case("restore"; "restore")]
#[test_case("split"; "split")]
#[test_case("squash"; "squash")]
fn test_hunk_selection_empty_file(command: &str) {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "repo"]).success();
    let work_dir = test_env.work_dir("repo");
    work_dir.write_file("file", "contents\n");

    std::fs::write(work_dir.root().join(".jj/empty-hunks"), "\n# comment\n").unwrap();
    let output = work_dir.run_jj([command, "--hunk-file", ".jj/empty-hunks"]);
    assert!(!output.status.success());
    assert!(
        output
            .stderr
            .raw()
            .contains("No hunk IDs found in `--hunk-file` input")
    );
}

#[test]
fn test_split_hunk_selection() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "repo"]).success();
    let work_dir = test_env.work_dir("repo");
    work_dir.write_file("file", "first before\nmiddle\nlast before\n");
    work_dir.run_jj(["commit", "-m", "base"]).success();
    work_dir.write_file("file", "first after\nmiddle\nlast after\n");
    let listing = work_dir.run_jj(["diff", "--hunks"]).success();
    let first_id = listing
        .stdout
        .raw()
        .lines()
        .find_map(|line| line.strip_prefix("hunk "))
        .and_then(|line| line.split_whitespace().next())
        .unwrap();

    work_dir
        .run_jj(["split", "--hunk", first_id, "-m", "selected"])
        .success();
    assert_eq!(
        work_dir
            .run_jj(["file", "show", "-r", "@-", "file"])
            .success()
            .stdout
            .raw(),
        "first after\nmiddle\nlast before\n"
    );
}

#[test]
fn test_squash_hunk_selection() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "repo"]).success();
    let work_dir = test_env.work_dir("repo");
    work_dir.write_file("file", "first before\nmiddle\nlast before\n");
    work_dir.run_jj(["commit", "-m", "base"]).success();
    work_dir.write_file("file", "first after\nmiddle\nlast after\n");
    let listing = work_dir.run_jj(["diff", "--hunks"]).success();
    let first_id = listing
        .stdout
        .raw()
        .lines()
        .find_map(|line| line.strip_prefix("hunk "))
        .and_then(|line| line.split_whitespace().next())
        .unwrap();

    work_dir
        .run_jj(["squash", "--hunk", first_id, "--use-destination-message"])
        .success();
    assert_eq!(
        work_dir
            .run_jj(["file", "show", "-r", "@-", "file"])
            .success()
            .stdout
            .raw(),
        "first after\nmiddle\nlast before\n"
    );
}

#[test]
fn test_squash_hunk_selection_before_rewritten_source() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "repo"]).success();
    let work_dir = test_env.work_dir("repo");
    work_dir.write_file("file", "first before\nmiddle\nlast before\n");
    work_dir.run_jj(["commit", "-m", "base"]).success();
    work_dir.write_file("file", "first after\nmiddle\nlast after\n");
    work_dir.run_jj(["commit", "-m", "source"]).success();
    let listing = work_dir.run_jj(["diff", "--hunks", "-r", "@-"]).success();
    let first_id = listing
        .stdout
        .raw()
        .lines()
        .find_map(|line| line.strip_prefix("hunk "))
        .and_then(|line| line.split_whitespace().next())
        .unwrap();

    work_dir
        .run_jj([
            "squash",
            "--from",
            "@-",
            "--insert-before",
            "@-",
            "--hunk",
            first_id,
            "-m",
            "selected",
        ])
        .success();

    assert_eq!(
        work_dir
            .run_jj(["file", "show", "-r", "@--", "file"])
            .success()
            .stdout
            .raw(),
        "first after\nmiddle\nlast before\n"
    );
}

#[test]
fn test_squash_hunk_selection_from_multiple_sources() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "repo"]).success();
    let work_dir = test_env.work_dir("repo");
    work_dir.write_file("first-file", "base\n");
    work_dir.write_file("second-file", "base\n");
    work_dir.run_jj(["commit", "-m", "base"]).success();
    work_dir.write_file("first-file", "first\n");
    work_dir.run_jj(["commit", "-m", "first"]).success();
    work_dir.write_file("second-file", "second\n");
    work_dir.run_jj(["describe", "-m", "second"]).success();

    let listing = work_dir
        .run_jj(["diff", "--hunks", "-r", "@-::@"])
        .success();
    let first_file = listing.stdout.raw().split_once("first-file:").unwrap().1;
    let first_id = first_file
        .lines()
        .find_map(|line| line.strip_prefix("hunk "))
        .and_then(|line| line.split_whitespace().next())
        .unwrap();
    assert!(listing.stdout.raw().contains(" source="));

    work_dir
        .run_jj([
            "squash",
            "--from",
            "@-::@",
            "--into",
            "@--",
            "--hunk",
            first_id,
            "--use-destination-message",
        ])
        .success();
    assert_eq!(
        work_dir
            .run_jj(["file", "show", "-r", "@-", "first-file"])
            .success()
            .stdout
            .raw(),
        "first\n"
    );
    assert_eq!(
        work_dir
            .run_jj(["file", "show", "-r", "@", "second-file"])
            .success()
            .stdout
            .raw(),
        "second\n"
    );
}

#[test]
fn test_diff_hunks_groups_rename() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "repo"]).success();
    let work_dir = test_env.work_dir("repo");
    work_dir.write_file("old", "contents\n");
    work_dir.run_jj(["commit", "-m", "base"]).success();
    std::fs::rename(work_dir.root().join("old"), work_dir.root().join("new")).unwrap();

    let listing = work_dir.run_jj(["diff", "--hunks"]).success();
    let hunk_ids = listing
        .stdout
        .raw()
        .lines()
        .filter_map(|line| line.strip_prefix("hunk "))
        .map(|line| line.split_whitespace().next().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(hunk_ids.len(), 1);
    assert!(listing.stdout.raw().contains(" (old => new):\n"));

    let id = hunk_ids[0];
    work_dir
        .run_jj(["commit", "--hunk", id, "-m", "renamed"])
        .success();
    assert_eq!(
        work_dir
            .run_jj(["file", "show", "-r", "@-", "new"])
            .success()
            .stdout
            .raw(),
        "contents\n"
    );
    let old_path = work_dir.run_jj(["file", "show", "-r", "@-", "old"]);
    assert!(!old_path.status.success());
}

#[test]
fn test_hunk_selection_matches_rename_source() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "repo"]).success();
    let work_dir = test_env.work_dir("repo");
    work_dir.write_file("source", "contents\n");
    work_dir.run_jj(["commit", "-m", "base"]).success();
    std::fs::rename(
        work_dir.root().join("source"),
        work_dir.root().join("target"),
    )
    .unwrap();

    let listing = work_dir.run_jj(["diff", "--hunks", "source"]).success();
    let ids = listing
        .stdout
        .raw()
        .lines()
        .filter_map(|line| line.strip_prefix("hunk "))
        .map(|line| line.split_whitespace().next().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(ids.len(), 1);
    assert!(listing.stdout.raw().contains(" (source => target):\n"));

    work_dir
        .run_jj(["commit", "--hunk", ids[0], "-m", "renamed", "source"])
        .success();
    assert_eq!(
        work_dir
            .run_jj(["file", "show", "-r", "@-", "target"])
            .success()
            .stdout
            .raw(),
        "contents\n"
    );
    assert!(
        !work_dir
            .run_jj(["file", "show", "-r", "@-", "source"])
            .status
            .success()
    );
}

#[test]
fn test_hunk_selection_structural_rename_removes_source() {
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

    let listing = work_dir.run_jj(["diff", "--hunks"]).success();
    let ids = listing
        .stdout
        .raw()
        .lines()
        .filter_map(|line| line.strip_prefix("hunk "))
        .map(|line| line.split_whitespace().next().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(ids.len(), 1);
    assert!(
        listing.stdout.raw().find("hunk ").unwrap()
            < listing
                .stdout
                .raw()
                .find("Removed regular file node:")
                .unwrap()
    );

    work_dir
        .run_jj(["commit", "--hunk", ids[0], "-m", "structural rename"])
        .success();
    assert_eq!(
        work_dir
            .run_jj(["file", "show", "-r", "@-", "node/child"])
            .success()
            .stdout
            .raw(),
        "contents\n"
    );
    assert!(
        !work_dir
            .run_jj(["file", "show", "-r", "@-", "source"])
            .status
            .success()
    );
}

#[test]
fn test_restore_hunk_selection_and_stale_handle() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "repo"]).success();
    let work_dir = test_env.work_dir("repo");
    work_dir.write_file("file", "first before\nmiddle\nlast before\n");
    work_dir.run_jj(["commit", "-m", "base"]).success();
    work_dir.write_file("file", "first after\nmiddle\nlast after\n");
    // Restore takes the IDs of the changes it undoes, as `jj diff` lists them.
    let listing = work_dir.run_jj(["diff", "--hunks"]).success();
    let first_id = listing
        .stdout
        .raw()
        .lines()
        .find_map(|line| line.strip_prefix("hunk "))
        .and_then(|line| line.split_whitespace().next())
        .unwrap();

    work_dir.run_jj(["restore", "--hunk", first_id]).success();
    assert_eq!(
        work_dir
            .run_jj(["file", "show", "-r", "@", "file"])
            .success()
            .stdout
            .raw(),
        "first before\nmiddle\nlast after\n"
    );
    let stale = work_dir.run_jj(["restore", "--hunk", first_id]);
    assert!(!stale.status.success());
    assert!(stale.stderr.raw().contains("no hunk has ID"));
}

#[test]
fn test_restore_hunk_selection_groups_rename() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "repo"]).success();
    let work_dir = test_env.work_dir("repo");
    work_dir.write_file("old", "contents\n");
    work_dir.run_jj(["commit", "-m", "base"]).success();
    std::fs::rename(work_dir.root().join("old"), work_dir.root().join("new")).unwrap();

    let listing = work_dir.run_jj(["diff", "--hunks"]).success();
    let hunk_ids = listing
        .stdout
        .raw()
        .lines()
        .filter_map(|line| line.strip_prefix("hunk "))
        .map(|line| line.split_whitespace().next().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(hunk_ids.len(), 1);
    assert!(listing.stdout.raw().contains(" (old => new):\n"));

    let id = hunk_ids[0];
    work_dir.run_jj(["restore", "--hunk", id]).success();
    assert_eq!(
        work_dir
            .run_jj(["file", "show", "-r", "@", "old"])
            .success()
            .stdout
            .raw(),
        "contents\n"
    );
    let new_path = work_dir.run_jj(["file", "show", "-r", "@", "new"]);
    assert!(!new_path.status.success());
}

#[test]
fn test_hunk_option_conflicts_and_fileset_scope() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "repo"]).success();
    let work_dir = test_env.work_dir("repo");
    work_dir.write_file("file1", "before\n");
    work_dir.write_file("file2", "before\n");
    work_dir.run_jj(["commit", "-m", "base"]).success();
    work_dir.write_file("file1", "after\n");
    work_dir.write_file("file2", "after\n");

    let listing = work_dir.run_jj(["diff", "--hunks", "file1"]).success();
    assert!(listing.stdout.raw().contains("file1:\n"));
    assert!(!listing.stdout.raw().contains("file2"));
    let output = work_dir.run_jj([
        "commit",
        "--hunk",
        "00000000000000000000000000000000",
        "--interactive",
    ]);
    assert!(!output.status.success());
    assert!(output.stderr.raw().contains("cannot be used with"));

    let output = work_dir.run_jj([
        "commit",
        "--hunk",
        "00000000000000000000000000000000",
        "--tool",
        "fake",
    ]);
    assert!(!output.status.success());
    assert!(output.stderr.raw().contains("cannot be used with"));

    let output = work_dir.run_jj(["commit", "--hunk-file", "hunks", "--interactive"]);
    assert!(!output.status.success());
    assert!(output.stderr.raw().contains("cannot be used with"));

    let output = work_dir.run_jj(["commit", "--hunk", "abcd", "-m", "invalid"]);
    assert!(!output.status.success());
    assert!(
        output
            .stderr
            .raw()
            .contains("hunk ID must be 32 hexadecimal characters, got 4")
    );

    let output = work_dir.run_jj([
        "commit",
        "--hunk",
        "0000000000000000000000000000000",
        "-m",
        "invalid",
    ]);
    assert!(!output.status.success());
    assert!(
        output
            .stderr
            .raw()
            .contains("hunk ID must be 32 hexadecimal characters, got 31")
    );

    let listing = work_dir
        .run_jj(["diff", "--hunks", "--context", "0", "file1"])
        .success();
    assert!(listing.stdout.raw().contains("\nhunk "));

    let output = work_dir.run_jj(["diff", "--hunks", "--ignore-all-space"]);
    assert!(!output.status.success());
    assert!(output.stderr.raw().contains("cannot be used with"));
}

#[test]
fn test_commit_hunk_selection_respects_fileset_scope() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "repo"]).success();
    let work_dir = test_env.work_dir("repo");
    work_dir.write_file("selected", "before\n");
    work_dir.write_file("unmatched", "before\n");
    work_dir.run_jj(["commit", "-m", "base"]).success();
    work_dir.write_file("selected", "after\n");
    work_dir.write_file("unmatched", "after\n");
    let listing = work_dir.run_jj(["diff", "--hunks", "selected"]).success();
    let id = listing
        .stdout
        .raw()
        .lines()
        .find_map(|line| line.strip_prefix("hunk "))
        .and_then(|line| line.split_whitespace().next())
        .unwrap();

    work_dir
        .run_jj(["commit", "--hunk", id, "-m", "selected", "selected"])
        .success();

    assert_eq!(
        work_dir
            .run_jj(["file", "show", "-r", "@-", "selected"])
            .success()
            .stdout
            .raw(),
        "after\n"
    );
    assert_eq!(
        work_dir
            .run_jj(["file", "show", "-r", "@-", "unmatched"])
            .success()
            .stdout
            .raw(),
        "before\n"
    );
    assert_eq!(
        work_dir
            .run_jj(["file", "show", "-r", "@", "unmatched"])
            .success()
            .stdout
            .raw(),
        "after\n"
    );
}

#[test]
fn test_diff_hunks_describes_path_atom() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "repo"]).success();
    let work_dir = test_env.work_dir("repo");
    work_dir.write_file("added", "contents\n");

    let listing = work_dir.run_jj(["diff", "--hunks"]).success();

    assert!(listing.stdout.raw().contains("Added regular file added:\n"));
    assert_eq!(listing.stdout.raw().matches("\nhunk ").count(), 1);
    assert!(listing.stdout.raw().contains(": contents\n"));
}

#[test]
fn test_diff_hunks_describes_binary_path_atom() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "repo"]).success();
    let work_dir = test_env.work_dir("repo");
    work_dir.write_file("binary", b"before\0");
    work_dir.run_jj(["commit", "-m", "base"]).success();
    work_dir.write_file("binary", b"after\0");

    let listing = work_dir.run_jj(["diff", "--hunks"]).success();

    assert!(
        listing
            .stdout
            .raw()
            .contains("Modified regular file binary:\n")
    );
    assert_eq!(listing.stdout.raw().matches("\nhunk ").count(), 1);
    assert!(listing.stdout.raw().contains("    (binary)\n"));
}

#[test]
fn test_diff_hunks_describes_file_metadata_path_atom() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "repo"]).success();
    let work_dir = test_env.work_dir("repo");
    work_dir.write_file("mode", "contents\n");
    work_dir.run_jj(["commit", "-m", "base"]).success();
    work_dir.run_jj(["file", "chmod", "x", "mode"]).success();

    let listing = work_dir.run_jj(["diff", "--hunks"]).success();

    assert!(
        listing
            .stdout
            .raw()
            .contains("Non-executable file became executable at mode:\n")
    );
    assert_eq!(listing.stdout.raw().matches("\nhunk ").count(), 1);
}

#[cfg(unix)]
#[test]
fn test_diff_hunks_describes_non_file_path_atom() -> testutils::TestResult {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "repo"]).success();
    let work_dir = test_env.work_dir("repo");
    std::os::unix::fs::symlink("target", work_dir.root().join("link"))?;

    let listing = work_dir.run_jj(["diff", "--hunks"]).success();

    assert!(listing.stdout.raw().contains("Added symlink link:\n"));
    assert_eq!(listing.stdout.raw().matches("\nhunk ").count(), 1);
    Ok(())
}

#[test]
fn test_diff_hunks_renders_conflict() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "repo"]).success();
    let work_dir = test_env.work_dir("repo");
    create_commit_with_files(&work_dir, "base", &[], &[("file", "base\n")]);
    create_commit_with_files(&work_dir, "a", &["base"], &[("file", "a\n")]);
    create_commit_with_files(&work_dir, "b", &["base"], &[("file", "b\n")]);
    create_commit_with_files(&work_dir, "conflict", &["a", "b"], &[]);

    let listing = work_dir
        .run_jj(["diff", "--hunks", "--from", "base", "--to", "conflict"])
        .success();

    assert!(listing.stdout.raw().contains("Created conflict in file:\n"));
    assert_eq!(listing.stdout.raw().matches("\nhunk ").count(), 1);
}

#[test_case(&[]; "snapshot conflicts")]
#[test_case(&["--config", "diff.color-words.conflict=pair"]; "pair conflicts")]
fn test_diff_hunks_renders_conflict_and_resolved_tail(extra_args: &[&str]) {
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
    let contents = std::fs::read_to_string(work_dir.root().join("file")).unwrap();
    work_dir.write_file(
        "file",
        contents.replace("tail changed", "tail changed child"),
    );

    let listing = work_dir
        .run_jj(
            extra_args
                .iter()
                .copied()
                .chain(["diff", "--hunks", "--from", "conflict", "--to", "@"])
                .collect::<Vec<_>>(),
        )
        .success();
    assert_eq!(listing.stdout.raw().matches("\nhunk ").count(), 1);
}

#[test]
fn test_diff_hunk_selection_shows_only_selected_lines() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "repo"]).success();
    let work_dir = test_env.work_dir("repo");
    work_dir.write_file("file", "a\nb\nc\nd\ne\n");
    work_dir.run_jj(["commit", "-m", "base"]).success();
    work_dir.write_file("file", "a\nB1\nB2\nd\nE\n");

    let listing = work_dir.run_jj(["diff", "--hunks"]).success();
    let hunk_ids = listing
        .stdout
        .raw()
        .lines()
        .filter_map(|line| line.strip_prefix("hunk "))
        .collect::<Vec<_>>();
    assert_eq!(hunk_ids.len(), 2);
    let first_id = hunk_ids[0];

    let output = work_dir
        .run_jj(["diff", "--git", "--hunk", &format!("{first_id}:-2,+2")])
        .success();
    insta::assert_snapshot!(output, @"
    diff --git a/file b/file
    index 9405325339..9cf3314558 100644
    --- a/file
    +++ b/file
    @@ -1,5 +1,5 @@ a
     a
     b
    -c
    +B2
     d
     e
    [EOF]
    ");
    let output = work_dir
        .run_jj(["diff", "--summary", "--hunk", hunk_ids[1]])
        .success();
    insta::assert_snapshot!(output, @r"
    M file
    [EOF]
    ");

    let output = work_dir.run_jj(["diff", "--hunk", &format!("{first_id}:+3")]);
    assert_eq!(
        output.stderr.raw(),
        format!("Error: hunk `{first_id}` has no added line 3\n")
    );

    work_dir.run_jj(["new"]).success();
    let output = work_dir.run_jj(["diff", "-r", "@--::@", "--hunk", first_id]);
    assert_eq!(
        output.stderr.raw(),
        "Error: `--hunk` and `--hunk-file` require a single revision or `--from`/`--to`\n"
    );
    let output = work_dir.run_jj(["diff", "--hunks", "--hunk", first_id]);
    assert!(output.stderr.raw().contains("cannot be used with"));
}

#[test]
fn test_absorb_hunk_selection() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "repo"]).success();
    let work_dir = test_env.work_dir("repo");
    work_dir.write_file("file", "1a\n2a\n3a\n4a\n");
    work_dir.run_jj(["commit", "-m", "base"]).success();
    work_dir.write_file("file", "1b\n2a\n3a\n4a\n");
    work_dir.run_jj(["commit", "-m", "first"]).success();
    work_dir.write_file("file", "1b\n2a\n3a\n4b\n");
    work_dir.run_jj(["commit", "-m", "last"]).success();
    work_dir.write_file("file", "1c\n2a\n3a\n4c\n");

    let listing = work_dir.run_jj(["diff", "--hunks"]).success();
    let hunk_ids = listing
        .stdout
        .raw()
        .lines()
        .filter_map(|line| line.strip_prefix("hunk "))
        .collect::<Vec<_>>();
    assert_eq!(hunk_ids.len(), 2);

    let output = work_dir.run_jj(["absorb", "--hunk", hunk_ids[0]]).success();
    assert!(
        output
            .stderr
            .raw()
            .contains("Absorbed changes into 1 revisions:")
    );
    assert_eq!(
        work_dir
            .run_jj(["file", "show", "-r", "@--", "file"])
            .success()
            .stdout
            .raw(),
        "1c\n2a\n3a\n4a\n"
    );
    assert_eq!(
        work_dir
            .run_jj(["file", "show", "-r", "@-", "file"])
            .success()
            .stdout
            .raw(),
        "1c\n2a\n3a\n4b\n"
    );
    assert_eq!(
        work_dir
            .run_jj(["diff", "--git", "--context=0"])
            .success()
            .stdout
            .raw()
            .lines()
            .filter(|line| line.starts_with(['-', '+']))
            .filter(|line| !line.starts_with("---") && !line.starts_with("+++"))
            .collect::<Vec<_>>(),
        ["-4b", "+4c"]
    );
}

#[test]
fn test_squash_hunk_file_with_line_selection() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "repo"]).success();
    let work_dir = test_env.work_dir("repo");
    work_dir.write_file("file", "keep\nold\n");
    work_dir.run_jj(["commit", "-m", "base"]).success();
    work_dir.write_file("file", "keep\nnew 1\nnew 2\n");

    let listing = work_dir.run_jj(["diff", "--hunks"]).success();
    let hunk_id = listing
        .stdout
        .raw()
        .lines()
        .find_map(|line| line.strip_prefix("hunk "))
        .unwrap();
    // The context line `keep` is shown unnumbered, so the removed `old` is `-1`
    // and the added `new 2` is `+2`.
    assert_eq!(
        listing.stdout.raw(),
        format!(
            "Modified regular file file:\n\nhunk {hunk_id}\n         : keep\n   1     : \
             old\n        1: new 1\n        2: new 2\n"
        )
    );
    work_dir.write_file(
        ".jj/hunks",
        format!("# only the second new line\n{hunk_id}:+2\n"),
    );

    work_dir
        .run_jj(["squash", "--hunk-file", ".jj/hunks"])
        .success();
    assert_eq!(
        work_dir
            .run_jj(["file", "show", "-r", "@-", "file"])
            .success()
            .stdout
            .raw(),
        "keep\nold\nnew 2\n"
    );
    assert_eq!(
        work_dir
            .run_jj(["file", "show", "-r", "@", "file"])
            .success()
            .stdout
            .raw(),
        "keep\nnew 1\nnew 2\n"
    );
}

#[test]
fn test_hunk_id_survives_unrelated_edits_and_rewrites() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "repo"]).success();
    let work_dir = test_env.work_dir("repo");
    work_dir.write_file("file", "a\nb\n");
    work_dir.write_file("other", "x\n");
    work_dir.run_jj(["commit", "-m", "base"]).success();
    work_dir.write_file("file", "A\nb\n");

    let listing = work_dir.run_jj(["diff", "--hunks"]).success();
    let hunk_id = listing
        .stdout
        .raw()
        .lines()
        .find_map(|line| line.strip_prefix("hunk "))
        .unwrap()
        .to_owned();

    // Snapshotting an unrelated edit and rewording both rewrite `@`.
    work_dir.write_file("other", "y\n");
    work_dir.run_jj(["describe", "-m", "reworded"]).success();
    let output = work_dir
        .run_jj(["diff", "--summary", "--hunk", &hunk_id])
        .success();
    insta::assert_snapshot!(output, @r"
    M file
    [EOF]
    ");
    work_dir
        .run_jj(["commit", "--hunk", &hunk_id, "-m", "selected"])
        .success();
    assert_eq!(
        work_dir
            .run_jj(["diff", "--summary", "-r", "@-"])
            .success()
            .stdout
            .raw(),
        "M file\n"
    );
}

#[test]
fn test_squash_rejects_hunk_id_shared_by_sources() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "repo"]).success();
    let work_dir = test_env.work_dir("repo");
    create_commit_with_files(&work_dir, "base", &[], &[("file", "a\n")]);
    create_commit_with_files(&work_dir, "left", &["base"], &[("file", "b\n")]);
    create_commit_with_files(&work_dir, "right", &["base"], &[("file", "b\n")]);
    create_commit_with_files(&work_dir, "dest", &["base"], &[]);

    let listing = work_dir.run_jj(["diff", "--hunks", "-r", "left"]).success();
    let hunk_id = listing
        .stdout
        .raw()
        .lines()
        .find_map(|line| line.strip_prefix("hunk "))
        .unwrap()
        .to_owned();

    let output = work_dir.run_jj([
        "squash", "--from", "left", "--from", "right", "--into", "dest", "--hunk", &hunk_id,
    ]);
    assert_eq!(
        output.stderr.raw(),
        format!(
            "Error: Hunk `{hunk_id}` matches identical changes in more than one source \
             revision\nHint: Use `--from` to name the revision to move it from\n"
        )
    );

    work_dir
        .run_jj([
            "squash", "--from", "left", "--into", "dest", "-u", "--hunk", &hunk_id,
        ])
        .success();
    assert_eq!(
        work_dir
            .run_jj(["file", "show", "-r", "dest", "file"])
            .success()
            .stdout
            .raw(),
        "b\n"
    );
}

#[test_case(":-1", Ok("-b"); "first removed line")]
#[test_case(":-2", Ok("-a"); "second removed line")]
#[test_case(":-3", Err("has no removed line 3"); "reject context line")]
fn test_diff_hunks_numbers_exactly_the_selectable_lines(spec: &str, expected: Result<&str, &str>) {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "repo"]).success();
    let work_dir = test_env.work_dir("repo");
    work_dir.write_file("file", "a\nb\na\nb\n");
    work_dir.run_jj(["commit", "-m", "base"]).success();
    work_dir.write_file("file", "b\na\nb\na\n");

    // The middle hunk removes `b` and `a` and is followed by an unchanged `b`.
    // Diffing the hunk again together with that `b` used to show the first
    // removed `b` as unchanged and number the unchanged one instead.
    let listing = work_dir.run_jj(["diff", "--hunks"]).success();
    let hunk_ids = listing
        .stdout
        .raw()
        .lines()
        .filter_map(|line| line.strip_prefix("hunk "))
        .collect::<Vec<_>>();
    let [first, middle, last] = hunk_ids.as_slice() else {
        panic!("expected three hunks, got {hunk_ids:?}");
    };
    assert_eq!(
        listing.stdout.raw(),
        format!(
            "Modified regular file file:\n\nhunk {first}\n        1: b\n         : a\n\nhunk \
             {middle}\n   1     : b\n   2     : a\n         : b\n\nhunk {last}\n         : b\n        \
             1: a\n"
        )
    );
    match expected {
        Ok(removed) => {
            let output = work_dir
                .run_jj([
                    "diff",
                    "--git",
                    "--context=0",
                    "--hunk",
                    &format!("{middle}{spec}"),
                ])
                .success();
            let changed = output
                .stdout
                .raw()
                .lines()
                .filter(|line| line.starts_with(['-', '+']))
                .filter(|line| !line.starts_with("---") && !line.starts_with("+++"))
                .collect::<Vec<_>>();
            assert_eq!(changed, [removed], "{spec}");
        }
        Err(message) => {
            let output = work_dir.run_jj(["diff", "--hunk", &format!("{middle}{spec}")]);
            assert!(!output.status.success());
            assert_eq!(
                output.stderr.raw(),
                format!("Error: hunk `{middle}` {message}\n")
            );
        }
    }
}

#[test]
fn test_restore_hunk_with_paths_preserves_unrelated_changes() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "repo"]).success();
    let work_dir = test_env.work_dir("repo");
    work_dir.write_file("selected", "before\n");
    work_dir.write_file("unrelated", "unrelated before\n");
    work_dir.run_jj(["commit", "-m", "base"]).success();
    work_dir.write_file("selected", "after\n");
    work_dir.write_file("unrelated", "unrelated after\n");

    let listing = work_dir.run_jj(["diff", "--hunks", "selected"]).success();
    let hunk_id = listing
        .stdout
        .raw()
        .lines()
        .find_map(|line| line.strip_prefix("hunk "))
        .unwrap();
    work_dir
        .run_jj(["restore", "--hunk", hunk_id, "selected"])
        .success();

    assert_eq!(work_dir.read_file("selected"), "before\n");
    assert_eq!(work_dir.read_file("unrelated"), "unrelated after\n");
}

#[test_case("new"; "target path")]
#[test_case("old"; "source path")]
fn test_restore_hunk_with_paths_undoes_rename(path: &str) {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "repo"]).success();
    let work_dir = test_env.work_dir("repo");
    work_dir.write_file("old", "moved one\nmoved two\nmoved three\n");
    work_dir.run_jj(["commit", "-m", "base"]).success();
    std::fs::rename(work_dir.root().join("old"), work_dir.root().join("new")).unwrap();

    let listing = work_dir.run_jj(["diff", "--hunks", path]).success();
    let hunk_id = listing
        .stdout
        .raw()
        .lines()
        .find_map(|line| line.strip_prefix("hunk "))
        .unwrap()
        .to_owned();
    work_dir
        .run_jj(["restore", "--hunk", &hunk_id, path])
        .success();
    assert_eq!(
        work_dir
            .run_jj(["diff", "--summary"])
            .success()
            .stdout
            .raw(),
        "",
        "restoring with path {path}"
    );
}

#[test_case(":-1", "keep\nold 1\nnew 1\nnew 2\n"; "removed line")]
#[test_case(":+2", "keep\nnew 1\n"; "added line")]
#[test_case("", "keep\nold 1\nold 2\n"; "whole hunk")]
fn test_restore_hunk_line_selection_uses_forward_numbers(spec: &str, expected: &str) {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "repo"]).success();
    let work_dir = test_env.work_dir("repo");
    work_dir.write_file("file", "keep\nold 1\nold 2\n");
    work_dir.run_jj(["commit", "-m", "base"]).success();
    work_dir.write_file("file", "keep\nnew 1\nnew 2\n");
    let listing = work_dir.run_jj(["diff", "--hunks"]).success();
    let hunk_id = listing
        .stdout
        .raw()
        .lines()
        .find_map(|line| line.strip_prefix("hunk "))
        .unwrap()
        .to_owned();

    // `-1` is `old 1` and `+2` is `new 2`, as in the listing of `@`.
    work_dir
        .run_jj(["restore", "--hunk", &format!("{hunk_id}{spec}")])
        .success();
    assert_eq!(
        work_dir
            .run_jj(["file", "show", "-r", "@", "file"])
            .success()
            .stdout
            .raw(),
        expected,
        "restore {spec}"
    );
}

#[test]
fn test_filtered_structural_hunk_moves_rename_whole() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "repo"]).success();
    let work_dir = test_env.work_dir("repo");
    work_dir.write_file("src/a.txt", "renamed contents\nline 2\nline 3\n");
    work_dir.write_file("d", "file d\n");
    work_dir.run_jj(["commit", "-m", "base"]).success();
    std::fs::remove_file(work_dir.root().join("d")).unwrap();
    std::fs::create_dir(work_dir.root().join("d")).unwrap();
    std::fs::rename(
        work_dir.root().join("src/a.txt"),
        work_dir.root().join("d/a.txt"),
    )
    .unwrap();
    work_dir.write_file("d/b.txt", "new b\n");

    let hunk_id = |paths: &[&str]| {
        let listing = work_dir
            .run_jj(["diff", "--hunks"].iter().chain(paths))
            .success();
        listing
            .stdout
            .raw()
            .lines()
            .find_map(|line| line.strip_prefix("hunk "))
            .unwrap()
            .to_owned()
    };
    let unfiltered = hunk_id(&[]);
    assert_eq!(hunk_id(&["d/b.txt"]), unfiltered);

    work_dir
        .run_jj(["commit", "--hunk", &unfiltered, "-m", "moved", "d/b.txt"])
        .success();
    assert_eq!(
        work_dir
            .run_jj(["diff", "--summary", "-r", "@-"])
            .success()
            .stdout
            .raw(),
        "D d\nR {src => d}/a.txt\nA d/b.txt\n"
    );
    assert_eq!(
        work_dir
            .run_jj(["diff", "--summary"])
            .success()
            .stdout
            .raw(),
        ""
    );
}
