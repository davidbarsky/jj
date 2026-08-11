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

//! Stable handles for selecting parts of a tree transition.
//!
//! A *semantic atom* is the smallest part of a tree transition that can be
//! taken or left on its own and still leave behind a tree the caller could have
//! asked for. Three shapes qualify:
//!
//! - A run of changed lines, for a path whose two sides are both plain text and
//!   differ only in their lines.
//! - A whole path, taken or left entire whenever that is not so: some side or
//!   conflict term has no file at all, a term is not a regular file, a term is
//!   binary, or the sides differ in executable or copy metadata or in backend
//!   file identity rather than in their lines. [`PathHunkReason`] records
//!   which. An ordinary text file being added or deleted lands here too, since
//!   one side has no file to diff its lines against.
//! - A copy or rename, whose source and target are only meaningful together.
//!
//! Every atom is named by a [`DiffHunkId`], and one [`DiffSelection`] both
//! enumerates the atoms and reconstructs the tree from them, so a caller can
//! hand back the IDs it was given without restating where the boundaries fell.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;
use std::fmt;
use std::num::NonZeroUsize;
use std::ops::Range;
use std::str::FromStr;

use bstr::BStr;
use bstr::BString;
use futures::StreamExt as _;
use futures::TryStreamExt as _;
use futures::stream;
use itertools::Itertools as _;

use crate::backend::BackendError;
use crate::backend::BackendResult;
use crate::backend::CommitId;
use crate::backend::CopyId;
use crate::backend::FileId;
use crate::backend::MergedTreeValue;
use crate::backend::MergedTreeValueExt as _;
use crate::backend::TreeValue;
use crate::commit::Commit;
use crate::commit::conflict_label_for_commits;
use crate::conflict_labels::ConflictLabels;
use crate::content_hash::ContentHash;
use crate::content_hash::blake2b_hash;
use crate::copies::CopiesTreeDiffEntry;
use crate::copies::CopiesTreeDiffEntryPath;
use crate::copies::CopyOperation;
use crate::copies::CopyRecords;
use crate::copies::CopyTransition;
use crate::diff::ContentDiff;
use crate::diff::ContentDiffSelection;
use crate::diff::DiffHunkKind;
use crate::diff::find_line_ranges;
use crate::diff_presentation::FileContent;
use crate::hex_util;
use crate::matchers::EverythingMatcher;
use crate::matchers::Matcher;
use crate::matchers::PrefixMatcher;
use crate::merge::Diff;
use crate::merge::Merge;
use crate::merged_tree::MergedTree;
use crate::merged_tree_builder::MergedTreeBuilder;
use crate::repo::Repo;
use crate::repo_path::RepoPath;
use crate::repo_path::RepoPathBuf;
use crate::rewrite::merge_commit_trees;
use crate::rewrite::restore_tree;
use crate::store::Store;

/// Identifies one semantic atom by its path and content.
///
/// The bytes hash only the atom itself, the unchanged line on either side of a
/// text atom, and how many atoms identical in all of that precede it in the
/// same file. The ID therefore stays valid while the rest of the tree changes
/// and across rewrites of the commit that carries it, but not once a line next
/// to the atom changes. Taking one of two identical atoms can only hand its ID
/// to the other when their neighboring lines match too. The flip side is
/// that identical changes in different transitions share an ID: a caller
/// choosing among several transitions must treat an ID found in more than one
/// as ambiguous.
#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct DiffHunkId([u8; HUNK_ID_BYTES]);

const HUNK_ID_BYTES: usize = 16;

/// Rejects anything that is not exactly one hunk ID.
#[derive(Debug, thiserror::Error)]
pub enum DiffHunkIdParseError {
    /// The input was not 32 hexadecimal characters.
    #[error("hunk ID must be 32 hexadecimal characters, got {0}")]
    Length(usize),
    /// The input contained a non-hexadecimal character.
    #[error("hunk ID must contain only hexadecimal characters")]
    NotHex,
}

impl FromStr for DiffHunkId {
    type Err = DiffHunkIdParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.len() != HUNK_ID_BYTES * 2 {
            return Err(DiffHunkIdParseError::Length(value.len()));
        }
        let Some(bytes) = hex_util::decode_hex(value) else {
            return Err(DiffHunkIdParseError::NotHex);
        };
        let bytes: [u8; HUNK_ID_BYTES] = bytes
            .try_into()
            .expect("32 hexadecimal characters decode to 16 bytes");
        Ok(Self(bytes))
    }
}

impl fmt::Display for DiffHunkId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in &self.0 {
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for DiffHunkId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("DiffHunkId")
            .field(&self.to_string())
            .finish()
    }
}

/// Names one atom to select, optionally narrowed to some of its lines.
///
/// Parses from `ID`, or from `ID:` followed by comma-separated line numbers
/// or inclusive ranges, each prefixed with `-` for a removed line or `+` for
/// an added line: `ID:-2-4,+3`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HunkSelection {
    /// The atom to select from.
    pub id: DiffHunkId,
    /// The lines to take, or `None` to take the whole atom.
    pub lines: Option<HunkLineSelection>,
}

impl From<DiffHunkId> for HunkSelection {
    fn from(id: DiffHunkId) -> Self {
        Self { id, lines: None }
    }
}

/// Picks removed and added lines of a text atom by their presented numbers.
///
/// Each side is numbered from 1 at the atom's first changed line, so a number
/// depends only on what the atom's [`DiffHunkId`] hashes and stays meaningful
/// for as long as the ID does. The selection is never empty.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HunkLineSelection {
    lines: Diff<LineRanges>,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct LineNumber(NonZeroUsize);

#[derive(Clone, Debug, Eq, PartialEq)]
struct LineRange {
    start: LineNumber,
    end: LineNumber,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct LineRanges(Vec<LineRange>);

impl FromIterator<LineRange> for LineRanges {
    fn from_iter<T: IntoIterator<Item = LineRange>>(iter: T) -> Self {
        let mut ranges = iter.into_iter().collect::<Vec<_>>();
        ranges.sort_unstable_by_key(|range| range.start);
        let mut merged: Vec<LineRange> = Vec::new();
        for range in ranges {
            match merged.last_mut() {
                Some(previous) if range.start.0.get() <= previous.end.0.get().saturating_add(1) => {
                    previous.end = previous.end.max(range.end);
                }
                Some(_) | None => merged.push(range),
            }
        }
        Self(merged)
    }
}

impl Extend<LineRange> for LineRanges {
    fn extend<T: IntoIterator<Item = LineRange>>(&mut self, iter: T) {
        *self = self.0.drain(..).chain(iter).collect();
    }
}

/// Distinguishes the two numbered sides of a text atom.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HunkLineSide {
    /// Lines present only before the transition.
    Removed,
    /// Lines present only after the transition.
    Added,
}

impl fmt::Display for HunkLineSide {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Removed => formatter.write_str("removed"),
            Self::Added => formatter.write_str("added"),
        }
    }
}

/// Rejects a hunk selection that names no atom or no lines.
#[derive(Debug, thiserror::Error)]
pub enum HunkSelectionParseError {
    /// The part before `:` was not a hunk ID.
    #[error(transparent)]
    Id(#[from] DiffHunkIdParseError),
    /// Nothing followed the `:`.
    #[error("expected line numbers after `:`")]
    EmptyLines,
    /// A line item did not say which side it selects from.
    #[error("line selection `{0}` must start with `-` (removed) or `+` (added)")]
    MissingSide(String),
    /// A line item was not a positive number or an ascending range.
    #[error("line selection `{0}` must be a line number or an ascending range such as `2-4`")]
    InvalidLines(String),
}

impl FromStr for HunkSelection {
    type Err = HunkSelectionParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let Some((id, spec)) = value.split_once(':') else {
            return Ok(Self {
                id: value.parse()?,
                lines: None,
            });
        };
        let id = id.parse()?;
        let mut lines = Diff::new(Vec::new(), Vec::new());
        for item in spec.split(',') {
            if item.is_empty() {
                continue;
            }
            let (side, range) = if let Some(range) = item.strip_prefix('-') {
                (&mut lines.before, range)
            } else if let Some(range) = item.strip_prefix('+') {
                (&mut lines.after, range)
            } else {
                return Err(HunkSelectionParseError::MissingSide(item.to_owned()));
            };
            let (start, end) = match range.split_once('-') {
                Some(bounds) => bounds,
                None => (range, range),
            };
            let invalid = || HunkSelectionParseError::InvalidLines(item.to_owned());
            let start = LineNumber(start.parse().map_err(|_| invalid())?);
            let end = LineNumber(end.parse().map_err(|_| invalid())?);
            if start > end {
                return Err(invalid());
            }
            side.push(LineRange { start, end });
        }
        if lines.before.is_empty() && lines.after.is_empty() {
            return Err(HunkSelectionParseError::EmptyLines);
        }
        Ok(Self {
            id,
            lines: Some(HunkLineSelection {
                lines: lines.map(|ranges| ranges.into_iter().collect()),
            }),
        })
    }
}

/// Explains why a path transition cannot be represented by text atoms.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PathHunkReason {
    /// At least one side or conflict term has no file value.
    FilePresence,
    /// At least one side or conflict term is not a regular file.
    NonFile,
    /// At least one regular-file term contains binary data.
    Binary,
    /// Executable or copy metadata changes across otherwise textual files.
    FileMetadata,
    /// Backend file identities differ without a line or metadata difference.
    FileIdentity,
}

/// Identifies the captured path transition that explains a text atom's
/// conflict or file description.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TextHunkPresentation {
    /// Both sides are regular files with matching executable metadata.
    RegularFile,
    /// Both sides are executable files with matching metadata.
    ExecutableFile,
    /// Both sides retain conflict terms from the captured transition.
    ModifiedConflict,
    /// The captured transition resolves a conflict into a file.
    ResolvedConflict,
    /// The captured transition creates a conflict from a file.
    CreatedConflict,
}

/// Records which change a tree transition belongs to.
///
/// This labels where listed hunks come from; it does not enter their IDs.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DiffHunkContext {
    /// The transition is between two arbitrary trees.
    TreePair,
    /// The transition is the change made by a particular commit.
    Commit(CommitId),
}

/// Identifies the authoritative inputs for a captured selection transition.
pub enum DiffSelectionSource<'a> {
    /// Uses the commit's parent transition and provenance.
    Commit(&'a Commit),
    /// Uses merged endpoints, preserving each side's commit order.
    Range {
        /// Commits merged into the before tree.
        from: &'a [Commit],
        /// Commits merged into the after tree.
        to: &'a [Commit],
    },
    /// Uses replacement content with the source commit's parent transition.
    /// Provenance may name the original commit before rewriting.
    Rewritten {
        /// Source of parents and copy history.
        commit: &'a Commit,
        /// Replacement content for the after tree.
        tree: MergedTree,
        /// Presentation provenance retained across rewrites.
        context: DiffHunkContext,
    },
    /// Accepts an already-bound tree transition from a lower-level producer.
    Trees {
        /// Trees and their already-bound copy relations.
        transition: Box<CopyTransition>,
        /// Names for otherwise unlabeled conflict axes.
        tree_labels: Diff<&'a str>,
        /// Presentation provenance, independent of stable hunk identity.
        context: DiffHunkContext,
    },
}

/// Keeps trees, labels, provenance, and complete copy metadata together.
pub struct CapturedDiff {
    transition: CopyTransition,
    tree_labels: Diff<String>,
    context: DiffHunkContext,
}

impl DiffSelectionSource<'_> {
    /// Captures complete copy metadata before any fileset is applied.
    pub async fn capture(self, repo: &dyn Repo) -> BackendResult<CapturedDiff> {
        let (from, to, trees, context) = match self {
            Self::Trees {
                transition,
                tree_labels,
                context,
            } => {
                let captured = CapturedDiff {
                    transition: *transition,
                    tree_labels: tree_labels.map(str::to_owned),
                    context,
                };
                return Ok(captured);
            }
            Self::Commit(commit) => (
                commit.parents().await?,
                vec![commit.clone()],
                Diff::new(commit.parent_tree(repo.index()).await?, commit.tree()),
                DiffHunkContext::Commit(commit.id().clone()),
            ),
            Self::Rewritten {
                commit,
                tree,
                context,
            } => (
                commit.parents().await?,
                vec![commit.clone()],
                Diff::new(commit.parent_tree(repo.index()).await?, tree),
                context,
            ),
            Self::Range { from, to } => (
                from.to_vec(),
                to.to_vec(),
                Diff::new(
                    merge_commit_trees(repo, from).await?,
                    merge_commit_trees(repo, to).await?,
                ),
                DiffHunkContext::TreePair,
            ),
        };
        let roots = from
            .iter()
            .map(|commit| commit.id().clone())
            .collect::<Vec<_>>();
        let heads = to
            .iter()
            .map(|commit| commit.id().clone())
            .collect::<Vec<_>>();
        let records =
            CopyRecords::collect(repo.store(), &roots, &heads, &EverythingMatcher).await?;
        let captured = CapturedDiff {
            transition: records.bind(trees).await?,
            tree_labels: Diff::new(
                conflict_label_for_commits(&from),
                conflict_label_for_commits(&to),
            ),
            context,
        };
        Ok(captured)
    }
}

impl CapturedDiff {
    /// Returns the captured tree pair without allowing it to be replaced.
    pub fn trees(&self) -> &Diff<MergedTree> {
        self.transition.trees()
    }

    /// Returns the labels derived with the captured tree pair.
    pub fn tree_labels(&self) -> Diff<&str> {
        self.tree_labels.as_deref()
    }
}

impl fmt::Display for PathHunkReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FilePresence => formatter.write_str("file presence"),
            Self::NonFile => formatter.write_str("non-file value"),
            Self::Binary => formatter.write_str("binary content"),
            Self::FileMetadata => formatter.write_str("file metadata"),
            Self::FileIdentity => formatter.write_str("file identity"),
        }
    }
}

/// Retains the file-valued entries hidden by a structural atom.
///
/// Structural boundaries replace several ordinary file entries in the
/// selection model. Retaining those entries preserves their removal/addition
/// order and copy relationship under one selection identity.
#[derive(Clone, Debug)]
struct PathPresentationEntry {
    path: CopiesTreeDiffEntryPath,
    values: Diff<MergedTreeValue>,
}

/// Provides the captured transition without allowing it to be recombined with
/// another hunk's identity or selection state.
pub enum HunkPresentation<'a> {
    /// A text transition retained with the hunk that owns its identity.
    Text(TextHunkPresentationView<'a>),
    /// Structural entries retained with the transition that owns the hunk.
    Path(PathHunkPresentationView<'a>),
    /// A copy transition borrowed from the same hunk capture.
    Copy(CopyHunkPresentationView<'a>),
}

/// The immutable text view bound to one captured hunk identity.
pub struct TextHunkPresentationView<'a> {
    id: &'a DiffHunkId,
    path: &'a RepoPath,
    contents: Diff<Merge<&'a BStr>>,
    context: Diff<Option<&'a BStr>>,
    presentation: TextHunkPresentation,
}

/// The immutable structural view bound to one captured hunk identity.
pub struct PathHunkPresentationView<'a> {
    id: &'a DiffHunkId,
    path: &'a RepoPath,
    values: &'a Diff<MergedTreeValue>,
    removed_sources: &'a [RepoPathBuf],
    reason: PathHunkReason,
    entries: &'a [PathPresentationEntry],
}

/// The immutable copy view bound to one captured hunk identity.
pub struct CopyHunkPresentationView<'a> {
    id: &'a DiffHunkId,
    paths: Diff<&'a RepoPath>,
    values: &'a Diff<MergedTreeValue>,
    operation: CopyOperation,
}

impl<'a> HunkPresentation<'a> {
    /// Returns the identity bound to this captured presentation.
    pub fn id(&self) -> &'a DiffHunkId {
        match self {
            Self::Text(view) => view.id,
            Self::Path(view) => view.id,
            Self::Copy(view) => view.id,
        }
    }

    /// Uses the target path for copies and renames so grouping follows where
    /// the selected change lands.
    pub fn path(&self) -> &'a RepoPath {
        match self {
            Self::Text(view) => view.path,
            Self::Path(view) => view.path,
            Self::Copy(view) => view.paths.after,
        }
    }
}

impl<'a> TextHunkPresentationView<'a> {
    /// Returns the captured text transition and its path-level description.
    ///
    /// The contents are exactly the atom's own lines, numbered from 1 on each
    /// side for [`HunkLineSelection`]. They must be shown as one changed block:
    /// diffing them again, or together with their context, can align repeated
    /// lines differently and so number the wrong line.
    pub fn details(&self) -> (&'a RepoPath, &Diff<Merge<&'a BStr>>, TextHunkPresentation) {
        (self.path, &self.contents, self.presentation)
    }

    /// Returns the unchanged lines to show before and after the atom.
    ///
    /// At most one side is present, and only for resolved text. These lines
    /// cannot be selected, so they are meant to be shown without numbers.
    pub fn context(&self) -> Diff<Option<&'a BStr>> {
        self.context
    }
}

impl<'a> PathHunkPresentationView<'a> {
    /// Returns the captured structural transition and its indivisibility
    /// reason.
    pub fn details(&self) -> (&'a RepoPath, &'a Diff<MergedTreeValue>, PathHunkReason) {
        (self.path, self.values, self.reason)
    }

    /// Returns rename sources retained by this structural transition.
    pub fn removed_sources(&self) -> &'a [RepoPathBuf] {
        self.removed_sources
    }

    /// Iterates the file transitions retained beneath this structural atom.
    pub fn entries(
        &self,
    ) -> impl Iterator<Item = (&CopiesTreeDiffEntryPath, &Diff<MergedTreeValue>)> + 'a {
        self.entries
            .iter()
            .map(|entry| (&entry.path, &entry.values))
    }
}

impl<'a> CopyHunkPresentationView<'a> {
    /// Returns the captured paths, values, and copy operation together.
    pub fn details(&self) -> (Diff<&'a RepoPath>, &'a Diff<MergedTreeValue>, CopyOperation) {
        (self.paths, self.values, self.operation)
    }
}

/// Failures that leave the input tree unchanged.
#[derive(Debug, thiserror::Error)]
pub enum DiffSelectionError {
    /// A supplied ID does not belong to this tree transition.
    #[error("no hunk has ID `{0}`")]
    HunkNotFound(DiffHunkId),
    /// Lines were requested from an atom that is not a run of resolved text
    /// lines, so it can only be taken whole.
    #[error("hunk `{0}` can only be selected whole")]
    LinesUnsupported(DiffHunkId),
    /// A requested line number is context or lies outside the atom.
    #[error("hunk `{id}` has no {side} line {line}")]
    LineNotInHunk {
        /// The atom the line was requested from.
        id: DiffHunkId,
        /// The side the number was read against.
        side: HunkLineSide,
        /// The presented line number.
        line: usize,
    },
    /// Reading or writing the semantic tree values failed.
    #[error(transparent)]
    Backend(#[from] BackendError),
}

#[derive(Debug)]
enum FileMode {
    Regular,
    Executable,
}

impl From<bool> for FileMode {
    fn from(executable: bool) -> Self {
        match executable {
            false => Self::Regular,
            true => Self::Executable,
        }
    }
}

impl From<&FileMode> for bool {
    fn from(mode: &FileMode) -> Self {
        match mode {
            FileMode::Regular => false,
            FileMode::Executable => true,
        }
    }
}

#[derive(Debug)]
enum SelectionPlan {
    Text {
        path: RepoPathBuf,
        text: TextPlan,
        executable: FileMode,
        copy_id: CopyId,
        presentation: TextHunkPresentation,
    },
    Atomic {
        id: DiffHunkId,
        change: AtomicPathChange,
        presentation: AtomicPresentation,
    },
}

#[derive(Debug)]
struct AtomicPathChange {
    path: RepoPathBuf,
    values: Diff<MergedTreeValue>,
    removed_sources: Vec<RepoPathBuf>,
}

#[derive(Debug)]
enum AtomicPresentation {
    Path {
        reason: PathHunkReason,
        entries: Vec<PathPresentationEntry>,
    },
    Copy {
        source: RepoPathBuf,
        operation: CopyOperation,
    },
}

#[derive(Debug)]
enum TextPlan {
    Resolved {
        contents: Diff<BString>,
        atoms: Vec<ResolvedTextAtom>,
    },
    Conflicted {
        segments: Vec<ContentSegment>,
    },
}

#[derive(Debug)]
enum ContentSegment {
    Matching(Diff<Merge<BString>>),
    Changed {
        id: DiffHunkId,
        contents: Diff<Merge<BString>>,
    },
}

#[derive(Debug)]
struct ResolvedTextAtom {
    id: DiffHunkId,
    ranges: Diff<Range<usize>>,
    context: TextContext,
}

#[derive(Debug)]
enum TextContext {
    None,
    Leading(BString),
    Trailing(BString),
}

/// Determines which side supplies changes outside the captured scope.
#[derive(Clone, Copy, Debug)]
pub enum SelectionAction {
    /// Applies selected changes to the before tree.
    Select,
    /// Keeps selected changes in scope and all after values outside it.
    Keep,
    /// Undoes selected changes in scope and preserves after values outside it.
    Undo,
}

enum SelectionBase {
    Before,
    After,
}
enum AtomChoice {
    Before,
    After,
}

struct ResolvedSelection<'a> {
    catalog: &'a DiffSelection,
    atomic_edits: Vec<&'a AtomicPathChange>,
    file_edits: Vec<FileEdit<'a>>,
    base: SelectionBase,
}

enum TreeEdit<'a> {
    Atomic(&'a AtomicPathChange),
    File(FileEdit<'a>),
}

struct FileEdit<'a> {
    path: &'a RepoPath,
    contents: Vec<BString>,
    executable: &'a FileMode,
    copy_id: &'a CopyId,
}

enum PathClassification {
    Text {
        contents: Diff<Merge<BString>>,
        executable: FileMode,
        copy_id: CopyId,
        presentation: TextHunkPresentation,
    },
    Path(PathHunkReason),
}

struct SelectionEntry {
    path: CopiesTreeDiffEntryPath,
    values: Diff<MergedTreeValue>,
    removed_sources: Vec<RepoPathBuf>,
    presentation_entries: Vec<PathPresentationEntry>,
}

/// A catalog and reconstruction plan for one exact tree transition.
#[derive(Debug)]
pub struct DiffSelection {
    trees: Diff<MergedTree>,
    tree_labels: Diff<String>,
    context: DiffHunkContext,
    plans: Vec<SelectionPlan>,
}

/// The inputs a [`DiffHunkId`] hashes.
///
/// Only the atom's path, content, and adjacent lines are included, never the
/// surrounding trees or commit, so that the ID survives changes elsewhere in
/// the tree and rewrites of the commit that leave the atom alone.
#[derive(ContentHash)]
struct HunkIdentity {
    schema_version: u64,
    path: RepoPathBuf,
    /// Counts earlier atoms in the same file with an identical value, which is
    /// the only thing left to tell such atoms apart.
    occurrence: u64,
    value: HunkIdentityValue,
}

/// One line of an unchanged run next to an atom, taken from each before and
/// after term.
#[derive(Clone, ContentHash)]
struct NeighborLine {
    before: Vec<Vec<u8>>,
    after: Vec<Vec<u8>>,
}

#[derive(Clone, ContentHash)]
enum HunkIdentityValue {
    Text {
        before: Vec<Vec<u8>>,
        after: Vec<Vec<u8>>,
        /// The unchanged line just above, which tells identical atoms apart by
        /// where they sit.
        line_above: Option<NeighborLine>,
        /// The unchanged line just below.
        line_below: Option<NeighborLine>,
    },
    Path {
        before: MergedTreeValue,
        after: MergedTreeValue,
        removed_sources: Vec<RepoPathBuf>,
    },
    Copy {
        before_path: RepoPathBuf,
        after_path: RepoPathBuf,
        operation: CopyOperation,
        before: MergedTreeValue,
        after: MergedTreeValue,
    },
}

impl DiffSelection {
    /// Returns a plan holding every semantic atom between the two trees.
    ///
    /// Enumeration and reconstruction are derived here together, from one pass
    /// over the transition, so an ID reported by [`Self::hunks`] is guaranteed
    /// to be one [`Self::select`] can act on; nothing recomputes the boundaries
    /// later and risks landing them elsewhere.
    ///
    /// A matcher that reaches beneath a tree/non-tree transition exposes that
    /// transition's outermost structural boundary as one indivisible atom.
    pub async fn build(captured: CapturedDiff, matcher: &dyn Matcher) -> BackendResult<Self> {
        let CapturedDiff {
            transition,
            tree_labels,
            context,
        } = captured;
        let trees = transition.trees();
        let mut plans = Vec::new();
        let mut entries = BTreeMap::new();
        let matcher = transition.matcher(matcher);
        let mut stream = transition.diff(&matcher);
        while let Some(CopiesTreeDiffEntry { path, values }) = stream.next().await {
            let values = values?;
            entries.insert(
                path.target.clone(),
                SelectionEntry {
                    path: path.clone(),
                    values: values.clone(),
                    removed_sources: Vec::new(),
                    presentation_entries: vec![PathPresentationEntry { path, values }],
                },
            );
        }
        let mut structural_boundaries = Vec::new();
        for path in entries.keys() {
            let mut boundary = None;
            for ancestor in path.ancestors() {
                if ancestor.is_root() {
                    continue;
                }
                let values = Diff::new(
                    trees.before.path_value(ancestor).await?,
                    trees.after.path_value(ancestor).await?,
                );
                let mut has_tree = false;
                let mut has_non_tree = false;
                let terms = values.before.iter().chain(values.after.iter());
                for value in terms {
                    match value {
                        None => {}
                        Some(
                            TreeValue::File {
                                id: _,
                                executable: _,
                                copy_id: _,
                            }
                            | TreeValue::Symlink(_)
                            | TreeValue::GitSubmodule(_),
                        ) => has_non_tree = true,
                        Some(TreeValue::Tree(_)) => has_tree = true,
                    }
                }
                if has_tree && has_non_tree {
                    boundary = Some((ancestor.to_owned(), values));
                }
            }
            if let Some((path, values)) = boundary {
                // Path-ordered entries group each outermost boundary contiguously.
                match structural_boundaries.last() {
                    Some((previous, _)) if previous == &path => {}
                    Some(_) | None => structural_boundaries.push((path, values)),
                }
            }
        }
        for (path, values) in structural_boundaries {
            // The boundary is taken or left whole, so what it carries is read
            // from its entire subtree: were it limited to the paths the matcher
            // reached, its ID would depend on the matcher, and selecting it would
            // move a renamed file in without removing the rename's source.
            let subtree = PrefixMatcher::new([&path]);
            let subtree = transition.matcher(&subtree);
            let mut presentation_entries = Vec::new();
            let mut stream = transition.diff(&subtree);
            while let Some(CopiesTreeDiffEntry {
                path: entry_path,
                values,
            }) = stream.next().await
            {
                let values = values?;
                if entry_path.target.starts_with(&path) {
                    presentation_entries.push(PathPresentationEntry {
                        path: entry_path,
                        values,
                    });
                }
            }
            let removed_sources = presentation_entries
                .iter()
                .filter_map(|entry| match &entry.path.source {
                    Some((source, CopyOperation::Rename)) => {
                        (!source.starts_with(&path)).then(|| source.clone())
                    }
                    Some((_source, CopyOperation::Copy)) => None,
                    None => None,
                })
                .unique()
                .collect::<Vec<_>>();
            entries.retain(|entry_path, _| !entry_path.starts_with(&path));
            entries.insert(
                path.clone(),
                SelectionEntry {
                    path: CopiesTreeDiffEntryPath {
                        source: None,
                        target: path,
                    },
                    values,
                    removed_sources,
                    presentation_entries,
                },
            );
        }
        for (
            path,
            SelectionEntry {
                path: entry_path,
                values,
                removed_sources,
                presentation_entries,
            },
        ) in entries
        {
            if let Some((source, operation)) = entry_path.source.clone() {
                let paths = Diff::new(source, entry_path.target);
                let id = hunk_id(
                    &path,
                    0,
                    HunkIdentityValue::Copy {
                        before_path: paths.before.clone(),
                        after_path: paths.after.clone(),
                        operation,
                        before: values.before.clone(),
                        after: values.after.clone(),
                    },
                );
                let removed_sources = match operation {
                    CopyOperation::Copy => Vec::new(),
                    CopyOperation::Rename => vec![paths.before.clone()],
                };
                plans.push(SelectionPlan::Atomic {
                    id,
                    change: AtomicPathChange {
                        path: paths.after,
                        values,
                        removed_sources,
                    },
                    presentation: AtomicPresentation::Copy {
                        source: paths.before,
                        operation,
                    },
                });
                continue;
            }
            match classify_path(trees.before.store(), &path, &values).await? {
                PathClassification::Text {
                    contents,
                    executable,
                    copy_id,
                    presentation,
                } => {
                    let before_terms = contents.before.as_slice().len();
                    let all_inputs = contents
                        .before
                        .iter()
                        .chain(contents.after.iter())
                        .collect::<Vec<_>>();
                    let diff = ContentDiff::by_line(all_inputs);
                    // Runs are split out before any atom is identified, because an
                    // atom's identity includes the unchanged lines on either side.
                    let mut runs = Vec::new();
                    let hunks_with_ranges = diff.hunks().zip(diff.hunk_ranges());
                    for (hunk, hunk_ranges) in hunks_with_ranges {
                        let (before_contents, after_contents) =
                            hunk.contents.split_at(before_terms);
                        let (before_ranges, after_ranges) =
                            hunk_ranges.ranges.split_at(before_terms);
                        let run_contents = Diff::new(
                            semantic_merge(before_contents),
                            semantic_merge(after_contents),
                        );
                        let is_semantic_change = match hunk.kind {
                            DiffHunkKind::Matching => false,
                            DiffHunkKind::Different => run_contents.before != run_contents.after,
                        };
                        let ranges = is_semantic_change
                            .then(|| Diff::new(before_ranges.to_vec(), after_ranges.to_vec()));
                        runs.push((ranges, run_contents));
                    }
                    drop(diff);
                    let matching_run = |index: Option<usize>| match index.and_then(|i| runs.get(i))
                    {
                        Some((None, contents)) => Some(contents),
                        Some((Some(_), _)) | None => None,
                    };
                    let neighbor_line = |contents: &Diff<Merge<BString>>, last: bool| {
                        let Diff { before, after } = contents.as_ref().map(|terms| {
                            terms
                                .iter()
                                .map(|term| {
                                    let ranges = find_line_ranges(term);
                                    let range = if last { ranges.last() } else { ranges.first() };
                                    range
                                        .map_or_else(Vec::new, |range| term[range.clone()].to_vec())
                                })
                                .collect::<Vec<_>>()
                        });
                        NeighborLine { before, after }
                    };
                    let mut text =
                        match (contents.before.as_resolved(), contents.after.as_resolved()) {
                            (Some(before), Some(after)) => TextPlan::Resolved {
                                contents: Diff::new(before.clone(), after.clone()),
                                atoms: Vec::new(),
                            },
                            (Some(_) | None, None) | (None, Some(_)) => TextPlan::Conflicted {
                                segments: Vec::new(),
                            },
                        };
                    let mut occurrences = HashMap::new();
                    let indexed_runs = runs.iter().enumerate();
                    for (index, (ranges, contents)) in indexed_runs {
                        let Some(ranges) = ranges else {
                            match &mut text {
                                TextPlan::Resolved { .. } => {}
                                TextPlan::Conflicted { segments } => {
                                    segments.push(ContentSegment::Matching(contents.clone()));
                                }
                            }
                            continue;
                        };
                        let above = matching_run(index.checked_sub(1));
                        let below = matching_run(Some(index + 1));
                        let value = HunkIdentityValue::Text {
                            before: contents.before.iter().map(|term| term.to_vec()).collect(),
                            after: contents.after.iter().map(|term| term.to_vec()).collect(),
                            line_above: above.map(|above| neighbor_line(above, true)),
                            line_below: below.map(|below| neighbor_line(below, false)),
                        };
                        // Only atoms identical down to their surrounding lines need
                        // telling apart by order.
                        let first_id = hunk_id(&path, 0, value.clone());
                        let occurrence = occurrences.entry(first_id).or_insert(0);
                        let id = match *occurrence {
                            0 => first_id,
                            occurrence => hunk_id(&path, occurrence, value),
                        };
                        *occurrence += 1;
                        match &mut text {
                            TextPlan::Resolved { atoms, .. } => {
                                let context = match (below, above) {
                                    (Some(run), _) => TextContext::Trailing(
                                        run.after.as_resolved().expect("resolved file run").clone(),
                                    ),
                                    (None, Some(run)) => TextContext::Leading(
                                        run.after.as_resolved().expect("resolved file run").clone(),
                                    ),
                                    (None, None) => TextContext::None,
                                };
                                let ranges = ranges.as_ref().map(|ranges| {
                                    let [range]: [Range<usize>; 1] = ranges
                                        .clone()
                                        .try_into()
                                        .expect("resolved file has one term");
                                    range
                                });
                                atoms.push(ResolvedTextAtom {
                                    id,
                                    ranges,
                                    context,
                                });
                            }
                            TextPlan::Conflicted { segments } => {
                                segments.push(ContentSegment::Changed {
                                    id,
                                    contents: contents.clone(),
                                });
                            }
                        }
                    }
                    if occurrences.is_empty() {
                        let id = hunk_id(
                            &path,
                            0,
                            HunkIdentityValue::Path {
                                before: values.before.clone(),
                                after: values.after.clone(),
                                removed_sources: removed_sources.clone(),
                            },
                        );
                        plans.push(SelectionPlan::Atomic {
                            id,
                            change: AtomicPathChange {
                                path,
                                values,
                                removed_sources,
                            },
                            presentation: AtomicPresentation::Path {
                                reason: PathHunkReason::FileIdentity,
                                entries: presentation_entries,
                            },
                        });
                        continue;
                    }
                    plans.push(SelectionPlan::Text {
                        path,
                        text,
                        executable,
                        copy_id,
                        presentation,
                    });
                }
                PathClassification::Path(reason) => {
                    let id = hunk_id(
                        &path,
                        0,
                        HunkIdentityValue::Path {
                            before: values.before.clone(),
                            after: values.after.clone(),
                            removed_sources: removed_sources.clone(),
                        },
                    );
                    plans.push(SelectionPlan::Atomic {
                        id,
                        change: AtomicPathChange {
                            path,
                            values,
                            removed_sources,
                        },
                        presentation: AtomicPresentation::Path {
                            reason,
                            entries: presentation_entries,
                        },
                    });
                }
            }
        }
        Ok(Self {
            trees: trees.clone(),
            tree_labels,
            context,
            plans,
        })
    }

    /// Keeps axis names and conflict terms paired so presentation code never
    /// infers names from merge storage positions.
    pub fn tree_labels(&self) -> Diff<(&str, &ConflictLabels)> {
        Diff::new(
            (&self.tree_labels.before, self.trees.before.labels()),
            (&self.tree_labels.after, self.trees.after.labels()),
        )
    }

    /// Returns the source commit supplied when the plan was built, if any.
    pub fn hunk_source(&self) -> Option<&CommitId> {
        match &self.context {
            DiffHunkContext::TreePair => None,
            DiffHunkContext::Commit(id) => Some(id),
        }
    }

    /// Derives atoms in deterministic path and content order from the plans
    /// that reconstruct them.
    pub fn hunks(&self) -> Vec<HunkPresentation<'_>> {
        let mut hunks = Vec::new();
        for plan in &self.plans {
            match plan {
                SelectionPlan::Text {
                    path,
                    text,
                    executable: _,
                    copy_id: _,
                    presentation,
                } => {
                    match text {
                        TextPlan::Resolved { contents, atoms } => {
                            for ResolvedTextAtom {
                                id,
                                ranges,
                                context,
                            } in atoms
                            {
                                let contents = contents.as_ref().zip(ranges.as_ref()).map(
                                    |(contents, range)| {
                                        Merge::resolved(BStr::new(&contents[range.clone()]))
                                    },
                                );
                                let context = match context {
                                    TextContext::None => Diff::new(None, None),
                                    TextContext::Leading(text) => {
                                        Diff::new(Some(BStr::new(text)), None)
                                    }
                                    TextContext::Trailing(text) => {
                                        Diff::new(None, Some(BStr::new(text)))
                                    }
                                };
                                hunks.push(HunkPresentation::Text(TextHunkPresentationView {
                                    id,
                                    path,
                                    contents,
                                    context,
                                    presentation: *presentation,
                                }));
                            }
                        }
                        TextPlan::Conflicted { segments } => {
                            for segment in segments {
                                match segment {
                                    ContentSegment::Matching(_) => {}
                                    ContentSegment::Changed { id, contents } => hunks.push(
                                        HunkPresentation::Text(TextHunkPresentationView {
                                            id,
                                            path,
                                            contents: contents
                                                .as_ref()
                                                .map(|terms| terms.map(BStr::new)),
                                            context: Diff::new(None, None),
                                            presentation: *presentation,
                                        }),
                                    ),
                                }
                            }
                        }
                    }
                }
                SelectionPlan::Atomic {
                    id,
                    change,
                    presentation,
                } => {
                    let AtomicPathChange {
                        path,
                        values,
                        removed_sources,
                    } = change;
                    match presentation {
                        AtomicPresentation::Path { reason, entries } => {
                            hunks.push(HunkPresentation::Path(PathHunkPresentationView {
                                id,
                                path,
                                values,
                                removed_sources,
                                reason: *reason,
                                entries,
                            }));
                        }
                        AtomicPresentation::Copy { source, operation } => {
                            hunks.push(HunkPresentation::Copy(CopyHunkPresentationView {
                                id,
                                paths: Diff::new(source, path),
                                values,
                                operation: *operation,
                            }));
                        }
                    }
                }
            }
        }
        hunks
    }

    /// Resolves requests before any backend writes. Whole-atom requests absorb
    /// partial requests for the same ID, including unsupported line requests.
    /// Partial replacements retain unselected removed lines before selected adds.
    pub async fn select(
        &self,
        selections: &[HunkSelection],
        action: SelectionAction,
    ) -> Result<MergedTree, DiffSelectionError> {
        Ok(self.resolve(selections, action)?.write().await?)
    }

    fn resolve(
        &self,
        selections: &[HunkSelection],
        action: SelectionAction,
    ) -> Result<ResolvedSelection<'_>, DiffSelectionError> {
        let plan_ids = self
            .hunks()
            .into_iter()
            .map(|hunk| *hunk.id())
            .collect::<HashSet<_>>();
        let mut requested: HashMap<DiffHunkId, Option<Diff<LineRanges>>> = HashMap::new();
        for HunkSelection { id, lines } in selections {
            if !plan_ids.contains(id) {
                return Err(DiffSelectionError::HunkNotFound(*id));
            }
            let lines = lines.as_ref().map(|lines| lines.lines.clone());
            match (requested.get_mut(id), lines) {
                (None, lines) => {
                    requested.insert(*id, lines);
                }
                (Some(None), _) => {}
                (Some(existing @ Some(_)), None) => *existing = None,
                (Some(Some(existing)), Some(lines)) => {
                    existing.before.extend(lines.before.0);
                    existing.after.extend(lines.after.0);
                }
            }
        }
        let whole = |id: &DiffHunkId| -> Result<AtomChoice, DiffSelectionError> {
            match (requested.get(id), action) {
                (Some(Some(_)), _) => Err(DiffSelectionError::LinesUnsupported(*id)),
                (None, SelectionAction::Undo)
                | (Some(None), SelectionAction::Select | SelectionAction::Keep) => {
                    Ok(AtomChoice::After)
                }
                (None, SelectionAction::Select | SelectionAction::Keep)
                | (Some(None), SelectionAction::Undo) => Ok(AtomChoice::Before),
            }
        };
        let carrier = Carrier::new(&self.trees);
        let mut atomic_edits = Vec::new();
        let mut file_edits = Vec::new();
        for plan in &self.plans {
            match plan {
                SelectionPlan::Atomic {
                    id,
                    change,
                    presentation: _,
                } => match whole(id)? {
                    AtomChoice::Before => {}
                    AtomChoice::After => atomic_edits.push(change),
                },
                SelectionPlan::Text {
                    path,
                    text,
                    executable,
                    copy_id,
                    presentation: _,
                } => {
                    let output = match text {
                        TextPlan::Resolved { contents, atoms } => {
                            let mut replacements = Vec::new();
                            for ResolvedTextAtom {
                                id,
                                ranges,
                                context: _,
                            } in atoms
                            {
                                let Some(Some(numbers)) = requested.get(id) else {
                                    match whole(id)? {
                                        AtomChoice::Before => {}
                                        AtomChoice::After => replacements.push(ranges.clone()),
                                    }
                                    continue;
                                };
                                let mut lines = Diff::new(Vec::new(), Vec::new());
                                for (side, text, range, numbers, selected) in [
                                    (
                                        HunkLineSide::Removed,
                                        &contents.before,
                                        &ranges.before,
                                        &numbers.before,
                                        &mut lines.before,
                                    ),
                                    (
                                        HunkLineSide::Added,
                                        &contents.after,
                                        &ranges.after,
                                        &numbers.after,
                                        &mut lines.after,
                                    ),
                                ] {
                                    let line_ranges = find_line_ranges(&text[range.clone()]);
                                    let count = line_ranges.len();
                                    for LineRange { start, end } in &numbers.0 {
                                        if end.0.get() > count {
                                            return Err(DiffSelectionError::LineNotInHunk {
                                                id: *id,
                                                side,
                                                line: start.0.get().max(count.saturating_add(1)),
                                            });
                                        }
                                    }
                                    let mut requested_lines = numbers
                                        .0
                                        .iter()
                                        .flat_map(|r| r.start.0.get()..=r.end.0.get())
                                        .peekable();
                                    for (index, line) in line_ranges.into_iter().enumerate() {
                                        let choice = match requested_lines.peek() {
                                            Some(&number) if number == index + 1 => {
                                                requested_lines.next();
                                                match action {
                                                    SelectionAction::Select
                                                    | SelectionAction::Keep => AtomChoice::After,
                                                    SelectionAction::Undo => AtomChoice::Before,
                                                }
                                            }
                                            Some(_) | None => match action {
                                                SelectionAction::Select | SelectionAction::Keep => {
                                                    AtomChoice::Before
                                                }
                                                SelectionAction::Undo => AtomChoice::After,
                                            },
                                        };
                                        match choice {
                                            AtomChoice::Before => {}
                                            AtomChoice::After => selected.push(
                                                range.start + line.start..range.start + line.end,
                                            ),
                                        }
                                    }
                                }
                                let unchanged_after = ranges.after.start..ranges.after.start;
                                replacements.extend(
                                    lines
                                        .before
                                        .into_iter()
                                        .map(|removed| Diff::new(removed, unchanged_after.clone())),
                                );
                                let unchanged_before = ranges.before.end..ranges.before.end;
                                replacements.extend(
                                    lines
                                        .after
                                        .into_iter()
                                        .map(|added| Diff::new(unchanged_before.clone(), added)),
                                );
                            }
                            if replacements.is_empty() {
                                continue;
                            }
                            vec![
                                ContentDiffSelection::new(contents.as_ref(), replacements)
                                    .materialize(),
                            ]
                        }
                        TextPlan::Conflicted { segments } => {
                            let mut output = vec![BString::default(); carrier.term_count()];
                            let mut changed = AtomChoice::Before;
                            for segment in segments {
                                let (contents, choice) = match segment {
                                    ContentSegment::Matching(contents) => {
                                        (contents, AtomChoice::Before)
                                    }
                                    ContentSegment::Changed { id, contents } => {
                                        (contents, whole(id)?)
                                    }
                                };
                                let lifted = match choice {
                                    AtomChoice::Before => carrier.lift_before(&contents.before),
                                    AtomChoice::After => {
                                        changed = AtomChoice::After;
                                        carrier.lift_after(&contents.after)
                                    }
                                };
                                for (buffer, contents) in output.iter_mut().zip(lifted.iter()) {
                                    buffer.extend_from_slice(contents);
                                }
                            }
                            match changed {
                                AtomChoice::Before => continue,
                                AtomChoice::After => output,
                            }
                        }
                    };
                    file_edits.push(FileEdit {
                        path,
                        contents: output,
                        executable,
                        copy_id,
                    });
                }
            }
        }
        let base = match action {
            SelectionAction::Select => SelectionBase::Before,
            SelectionAction::Keep | SelectionAction::Undo => SelectionBase::After,
        };
        let selection = ResolvedSelection {
            catalog: self,
            atomic_edits,
            file_edits,
            base,
        };
        Ok(selection)
    }
}

impl ResolvedSelection<'_> {
    async fn write(self) -> BackendResult<MergedTree> {
        let Self {
            catalog,
            atomic_edits,
            file_edits,
            base,
        } = self;
        let tree = if atomic_edits.is_empty() && file_edits.is_empty() {
            catalog.trees.before.clone()
        } else {
            let carrier = Carrier::new(&catalog.trees);
            let store = catalog.trees.before.store();
            let before_labels = catalog
                .trees
                .before
                .labels_by_term(&catalog.tree_labels.before)
                .map(|label| (*label).to_owned());
            let after_labels = catalog
                .trees
                .after
                .labels_by_term(&catalog.tree_labels.after)
                .map(|label| (*label).to_owned());
            let labels = carrier.labels(&before_labels, &after_labels);
            let mut builders = carrier
                .lift_before(catalog.trees.before.tree_ids())
                .into_iter()
                .map(|id| MergedTreeBuilder::new(MergedTree::resolved(store.clone(), id)))
                .collect::<Vec<_>>();
            let edits = atomic_edits
                .into_iter()
                .map(TreeEdit::Atomic)
                .chain(file_edits.into_iter().map(TreeEdit::File));
            for edit in edits {
                let (path, values) = match edit {
                    TreeEdit::Atomic(AtomicPathChange {
                        path,
                        values,
                        removed_sources,
                    }) => {
                        for source in removed_sources {
                            for builder in &mut builders {
                                builder.set_or_remove(source.clone(), None);
                            }
                        }
                        (
                            path.as_ref(),
                            carrier
                                .lift_after(&values.after)
                                .into_iter()
                                .collect::<Vec<_>>(),
                        )
                    }
                    TreeEdit::File(FileEdit {
                        path,
                        contents,
                        executable,
                        copy_id,
                    }) => {
                        let mut values = Vec::with_capacity(contents.len());
                        for contents in contents {
                            let id = store.write_file(path, &mut contents.as_slice()).await?;
                            values.push(Some(TreeValue::File {
                                id,
                                executable: executable.into(),
                                copy_id: copy_id.clone(),
                            }));
                        }
                        (
                            path,
                            normalized_terms(&Merge::from_vec(values), carrier.term_count()),
                        )
                    }
                };
                for (builder, value) in builders.iter_mut().zip(values) {
                    builder.set_or_remove(path.to_owned(), value);
                }
            }
            let tree_ids = stream::iter(builders)
                .map(async |builder| {
                    let tree = builder.write_tree().await?;
                    BackendResult::Ok(
                        tree.into_tree_ids()
                            .into_resolved()
                            .expect("resolved carrier term"),
                    )
                })
                .buffered(store.concurrency())
                .try_collect::<Vec<_>>()
                .await?;
            let tree = MergedTree::new(store.clone(), Merge::from_vec(tree_ids), labels)
                .resolve()
                .await?;
            if tree.tree_ids() == catalog.trees.before.tree_ids() {
                catalog.trees.before.clone()
            } else if tree.tree_ids() == catalog.trees.after.tree_ids() {
                catalog.trees.after.clone()
            } else {
                tree
            }
        };
        match base {
            SelectionBase::Before => Ok(tree),
            SelectionBase::After => {
                let mut paths = Vec::new();
                for plan in &catalog.plans {
                    match plan {
                        SelectionPlan::Text { path, .. } => paths.push(path.as_ref()),
                        SelectionPlan::Atomic {
                            change:
                                AtomicPathChange {
                                    path,
                                    removed_sources,
                                    ..
                                },
                            ..
                        } => {
                            paths.push(path.as_ref());
                            paths.extend(removed_sources.iter().map(AsRef::as_ref));
                        }
                    }
                }
                restore_tree(
                    &tree,
                    &catalog.trees.after,
                    catalog.tree_labels.before.clone(),
                    catalog.tree_labels.after.clone(),
                    &PrefixMatcher::new(&paths),
                )
                .await
            }
        }
    }
}

fn hunk_id(path: &RepoPath, occurrence: u64, value: HunkIdentityValue) -> DiffHunkId {
    let identity = HunkIdentity {
        schema_version: 2,
        path: path.to_owned(),
        occurrence,
        value,
    };
    let hash = blake2b_hash(&identity);
    DiffHunkId(
        hash[..HUNK_ID_BYTES]
            .try_into()
            .expect("blake2b-512 output is 64 bytes"),
    )
}

fn semantic_merge(contents: &[&BStr]) -> Merge<BString> {
    if contents.windows(2).all(|pair| pair[0] == pair[1]) {
        Merge::resolved(contents[0].to_owned())
    } else {
        Merge::from_vec(
            contents
                .iter()
                .map(|contents| (*contents).to_owned())
                .collect::<Vec<_>>(),
        )
    }
}

async fn classify_path(
    store: &Store,
    path: &RepoPath,
    values: &Diff<MergedTreeValue>,
) -> BackendResult<PathClassification> {
    let Some(before_file_ids) = values.before.to_file_merge() else {
        return Ok(PathClassification::Path(PathHunkReason::NonFile));
    };
    let Some(after_file_ids) = values.after.to_file_merge() else {
        return Ok(PathClassification::Path(PathHunkReason::NonFile));
    };
    let Ok(before_file_ids) = before_file_ids.try_map(|id| id.clone().ok_or(())) else {
        return Ok(PathClassification::Path(PathHunkReason::FilePresence));
    };
    let Ok(after_file_ids) = after_file_ids.try_map(|id| id.clone().ok_or(())) else {
        return Ok(PathClassification::Path(PathHunkReason::FilePresence));
    };
    let read = async |file_ids: Merge<FileId>| {
        file_ids
            .try_map_async(async |id| {
                let contents = store.read_file(path, id).await?.read_all().await?;
                BackendResult::Ok(FileContent::from(BString::new(contents)))
            })
            .await
    };
    let contents = Diff::new(read(before_file_ids).await?, read(after_file_ids).await?);
    if contents
        .before
        .iter()
        .chain(contents.after.iter())
        .any(|content| content.is_binary)
    {
        return Ok(PathClassification::Path(PathHunkReason::Binary));
    }
    let contents = contents.map(|contents| {
        contents.into_map(
            |FileContent {
                 is_binary: _,
                 contents,
             }| contents,
        )
    });
    let Some(executable) = common_metadata(Diff::new(
        values.before.to_executable_merge(),
        values.after.to_executable_merge(),
    )) else {
        return Ok(PathClassification::Path(PathHunkReason::FileMetadata));
    };
    let Some(copy_id) = common_metadata(Diff::new(
        values.before.to_copy_id_merge(),
        values.after.to_copy_id_merge(),
    )) else {
        return Ok(PathClassification::Path(PathHunkReason::FileMetadata));
    };
    Ok(PathClassification::Text {
        contents,
        executable: executable.into(),
        copy_id,
        presentation: match (values.before.is_resolved(), values.after.is_resolved()) {
            (true, true) if executable => TextHunkPresentation::ExecutableFile,
            (true, true) => TextHunkPresentation::RegularFile,
            (false, false) => TextHunkPresentation::ModifiedConflict,
            (false, true) => TextHunkPresentation::ResolvedConflict,
            (true, false) => TextHunkPresentation::CreatedConflict,
        },
    })
}

fn common_metadata<T: Clone + Eq>(
    Diff { before, after }: Diff<Option<Merge<Option<T>>>>,
) -> Option<T> {
    let before = before?;
    let after = after?;
    let value = before.first().as_ref()?;
    if before.iter().all(|term| term.as_ref() == Some(value))
        && after.iter().all(|term| term.as_ref() == Some(value))
    {
        Some(value.clone())
    } else {
        None
    }
}

/// The merge axes of the output tree: the before axes followed by the after
/// axes, sharing axis zero.
#[derive(Clone, Copy, Debug)]
struct Carrier {
    before_terms: usize,
    after_terms: usize,
}

impl Carrier {
    fn new(trees: &Diff<MergedTree>) -> Self {
        Self {
            before_terms: trees.before.tree_ids().as_slice().len(),
            after_terms: trees.after.tree_ids().as_slice().len(),
        }
    }

    fn term_count(self) -> usize {
        self.before_terms + self.after_terms - 1
    }

    fn lift_before<T: Clone>(self, before: &Merge<T>) -> Merge<T> {
        let before = normalized_terms(before, self.before_terms);
        let mut values = before.clone();
        values.extend(std::iter::repeat_n(before[0].clone(), self.after_terms - 1));
        Merge::from_vec(values)
    }

    fn lift_after<T: Clone>(self, after: &Merge<T>) -> Merge<T> {
        let after = normalized_terms(after, self.after_terms);
        let mut values = vec![after[0].clone(); self.before_terms];
        values.extend(after.iter().skip(1).cloned());
        Merge::from_vec(values)
    }

    fn labels(self, before: &Merge<String>, after: &Merge<String>) -> ConflictLabels {
        let mut labels = before.iter().cloned().collect::<Vec<_>>();
        labels.extend(after.iter().skip(1).cloned());
        ConflictLabels::from_merge(Merge::from_vec(labels))
    }
}

fn normalized_terms<T: Clone>(merge: &Merge<T>, term_count: usize) -> Vec<T> {
    if merge.is_resolved() {
        vec![merge.first().clone(); term_count]
    } else {
        assert_eq!(merge.as_slice().len(), term_count);
        merge.iter().cloned().collect()
    }
}
