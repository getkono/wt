//! Content-level merge detection: whether every change on a commit is already in
//! a target branch even when its SHAs are not — a squash merge, a rebase merge,
//! work split across several pull requests, or a branch of merge commits whose
//! parents all landed. `wt prune` falls back to this when ancestry says "not
//! merged".
//!
//! A branch whose squash commit the target later edited no longer merges as a
//! no-op, so the target's own history since the merge base is searched as
//! well: a commit whose tree is the tip's tree, or a single commit whose diff
//! is the branch's net diff (the squash commit itself), means every byte of
//! the branch is recoverable from the target.
//!
//! The check fails safe: an error, a conflict, or an older `git` reads as "not
//! merged", so a failed check can never be the reason work is deleted.

use std::cell::{OnceCell, RefCell};
use std::collections::HashMap;
use std::path::Path;

use crate::git::cli::GitCli;

/// The content check, run with one `git` in one directory for any number of
/// tips and targets: what does not depend on the tip — the merge-driver
/// overrides and each target's tree — is read once and reused, so each check
/// costs a `merge-base`, one `rev-parse`, and the `merge-tree` itself, plus —
/// only when that merge is not a no-op — two `log`s and two `diff`s of the
/// history search. Lives for the span of one command: config or a target that
/// changes meanwhile is not seen.
#[cfg_attr(not(feature = "cli"), allow(dead_code))]
pub(crate) struct ContentCheck<'a> {
    git: &'a dyn GitCli,
    dir: &'a Path,
    overrides: OnceCell<Option<Vec<String>>>,
    target_trees: RefCell<HashMap<String, Option<String>>>,
}

#[cfg_attr(not(feature = "cli"), allow(dead_code))]
impl<'a> ContentCheck<'a> {
    /// A check run with `git` in `dir`; nothing is read until the first one.
    pub(crate) fn new(git: &'a dyn GitCli, dir: &'a Path) -> Self {
        ContentCheck {
            git,
            dir,
            overrides: OnceCell::new(),
            target_trees: RefCell::new(HashMap::new()),
        }
    }

    /// Whether every change `tip` introduces is already in `target`: merging
    /// `tip` into `target` would leave `target`'s tree unchanged
    /// (`git merge-tree --write-tree`, git ≥ 2.38). Meant for a `tip` that is
    /// *not* an ancestor of `target` — ancestry is the cheaper, stronger test
    /// and callers try it first.
    ///
    /// When that merge is not a clean no-op — typically because the target
    /// edited the same lines after taking the change — the target's history
    /// since the merge base is searched instead (see [`landed_in_history`]):
    /// a commit there whose tree is the tip's tree, or one whose diff is the
    /// branch's net diff, holds all of the branch's work. Either still counts
    /// a change the target later reverted as merged, as ancestry does for a
    /// merge commit that was reverted: the work stays recoverable from the
    /// commit that landed it. A change the target edited and that no single
    /// commit holds whole still reads as "not merged" — the safe direction.
    ///
    /// A branch whose final tree equals its merge base's — nothing but empty
    /// commits, or work it added and then backed out itself — also merges as a
    /// no-op, yet its commits may hold work found nowhere else. It reads as "not
    /// merged", so only a branch that actually changes something can be judged
    /// merged by content.
    ///
    /// `merge-tree` writes the merged tree to the object store; it creates no
    /// ref, so the object is garbage-collected like any other unreachable one.
    /// Unrelated histories, a `git` without `merge-tree --write-tree` and no
    /// history match, or any failure returns `false`. The history search
    /// consults no merge driver, so it runs even when the driver overrides
    /// cannot be built.
    pub(crate) fn is_merged(&self, tip: &str, target: &str) -> bool {
        let (git, dir) = (self.git, self.dir);
        let Some(expected) = self.target_tree(target) else {
            return false;
        };
        let Some((base, tip_tree)) = changed_base(git, dir, tip, target) else {
            return false;
        };
        self.merges_as_no_op(tip, target, &expected)
            || landed_in_history(git, dir, &base, (tip, &tip_tree), target)
    }

    /// Whether `git merge-tree --write-tree target tip` succeeds and writes
    /// `expected`, `target`'s own tree, with the driver overrides applied.
    fn merges_as_no_op(&self, tip: &str, target: &str, expected: &str) -> bool {
        let (git, dir) = (self.git, self.dir);
        let Some(overrides) = self
            .overrides
            .get_or_init(|| driver_overrides(git, dir))
            .as_deref()
        else {
            tracing::debug!(
                tip,
                target,
                "merge drivers unreadable: content check skipped"
            );
            return false;
        };
        let mut argv: Vec<&str> = Vec::new();
        for o in overrides {
            argv.extend(["-c", o.as_str()]);
        }
        argv.extend(["merge-tree", "--write-tree", "--no-messages", target, tip]);
        match git.run_raw(dir, &argv) {
            Ok(out) if out.success => out.stdout.lines().next().map(str::trim) == Some(expected),
            Ok(out) => {
                // Exit 1 is a conflict; anything else (an old git, unrelated
                // histories, an unknown ref) is logged so a surprising "not
                // merged" is traceable.
                tracing::debug!(tip, target, stderr = %out.stderr.trim(), "merge-tree: not a clean no-op");
                false
            }
            Err(error) => {
                tracing::debug!(tip, target, %error, "merge-tree could not run");
                false
            }
        }
    }

    /// `target`'s tree id, read once per target; `None` when it does not
    /// resolve.
    fn target_tree(&self, target: &str) -> Option<String> {
        if let Some(tree) = self.target_trees.borrow().get(target) {
            return tree.clone();
        }
        let tree = self
            .git
            .run(
                self.dir,
                &[
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    &format!("{target}^{{tree}}"),
                ],
            )
            .ok()
            .map(|t| t.trim().to_string());
        self.target_trees
            .borrow_mut()
            .insert(target.to_string(), tree.clone());
        tree
    }
}

/// `-c` overrides for the content check's `merge-tree`: every custom merge
/// driver configured for `dir` made to fail, the built-in `union` driver made
/// a plain text merge, and `merge.default` pinned to `text`. `None` when the
/// configuration cannot be read (which the caller treats as "not merged").
///
/// A custom driver decides a file's merge however it likes — `merge=ours` keeps
/// the target's side outright — so it can make work that never landed merge as
/// a no-op. Failing it turns every file it governs into a conflict: "not
/// merged", the safe direction. The built-in `union` driver keeps both sides of
/// a conflict and can hide a deletion the same way, whether a `merge=union`
/// attribute or `merge.default=union` selects it. It has no config key to find,
/// but git looks up a configured `merge.union.driver` before its built-ins, so
/// it is always overridden — with `git merge-file`, git's own text merge, so a
/// conflict still conflicts while a `union` file (typically a changelog) whose
/// edits both sides made cleanly still merges. It comes after any user
/// `merge.union.driver` found above, so it wins. `merge.default` is pinned as
/// well. `text` must stay usable, and `binary` already conflicts.
fn driver_overrides(git: &dyn GitCli, dir: &Path) -> Option<Vec<String>> {
    let out = git
        .run_raw(
            dir,
            &[
                "config",
                "-z",
                "--name-only",
                "--get-regexp",
                r"^merge\..+\.driver$",
            ],
        )
        .ok()?;
    // Exit 1 with no output is "no such key"; anything else is a read failure.
    if !out.success && !(out.stdout.is_empty() && out.stderr.trim().is_empty()) {
        return None;
    }
    // A driver name git's `-c` would split wrongly cannot be overridden, so
    // one fails the whole check.
    let mut overrides: Vec<String> = out
        .stdout
        .split('\0')
        .filter(|key| !key.is_empty())
        .map(|key| (!key.contains('=')).then(|| format!("{key}=exit 1")))
        .collect::<Option<_>>()?;
    overrides.push("merge.union.driver=git merge-file --marker-size=%L %A %O %B".into());
    overrides.push("merge.default=text".into());
    Some(overrides)
}

/// The merge base of `tip` and `target`, and `tip`'s tree, when that tree
/// differs from the base's — i.e. the branch's net effect is not empty. Any
/// failure (no merge base included) reads as `None`, which the caller treats
/// as "not merged".
fn changed_base(git: &dyn GitCli, dir: &Path, tip: &str, target: &str) -> Option<(String, String)> {
    let base = git.run(dir, &["merge-base", target, tip]).ok()?;
    let base = base.trim();
    // Both trees in one call: `rev-parse` prints one id per argument, and
    // fails as a whole if either does not resolve.
    let trees = git
        .run(
            dir,
            &[
                "rev-parse",
                &format!("{base}^{{tree}}"),
                &format!("{tip}^{{tree}}"),
            ],
        )
        .ok()?;
    match trees.split_whitespace().collect::<Vec<_>>()[..] {
        [base_tree, tip_tree] if base_tree != tip_tree => {
            Some((base.to_string(), tip_tree.to_string()))
        }
        _ => None,
    }
}

/// Fixed `git diff` options, so the branch's net diff and each target commit's
/// diff are rendered alike whatever the user's diff configuration: no colour,
/// external driver, textconv, or rename detection, full binary patches, and
/// git's default prefixes, context, and algorithm. A binary delta patch is
/// relative to its preimage, so it can be identical for different files;
/// [`normalize_diff`] keeps a binary file's blob ids for that reason.
const DIFF_OPTS: &[&str] = &[
    "--no-color",
    "--no-ext-diff",
    "--no-textconv",
    "--no-renames",
    "--no-relative",
    "--binary",
    "--unified=3",
    "--diff-algorithm=myers",
    "--src-prefix=a/",
    "--dst-prefix=b/",
    "--submodule=short",
];

/// Whether the work of `tip` (whose tree is `tip_tree`, and whose merge base
/// with `target` is `base`) is held by one commit of `base..target`: a commit
/// whose tree is `tip_tree`, or a non-merge commit whose diff is the branch's
/// net diff `git diff base tip`, compared without line numbers or blob ids
/// (the squash commit, landed after the target moved on). Any failure — a
/// path list too long for one command line included — reads as `false`.
fn landed_in_history(
    git: &dyn GitCli,
    dir: &Path,
    base: &str,
    (tip, tip_tree): (&str, &str),
    target: &str,
) -> bool {
    let range = format!("{base}..{target}");
    let Ok(trees) = git.run(dir, &["log", "--format=%T", &range]) else {
        return false;
    };
    if trees.lines().any(|t| t.trim() == tip_tree) {
        tracing::debug!(
            tip,
            target,
            "content check: a target commit has the tip's tree"
        );
        return true;
    }
    let Ok(net) = git.run(dir, &[&["diff"], DIFF_OPTS, &[base, tip]].concat()) else {
        return false;
    };
    let Ok(paths) = git.run(
        dir,
        &["diff", "--no-renames", "--name-only", "-z", base, tip],
    ) else {
        return false;
    };
    let paths: Vec<&str> = paths.split('\0').filter(|p| !p.is_empty()).collect();
    if paths.is_empty() {
        return false;
    }
    // Only commits touching the branch's paths can match; `--full-diff` still
    // shows each one's whole diff, so a commit that also changed other paths
    // does not match. A NUL never occurs in git's diff output (a file holding
    // one is binary, and `--binary` encodes it), so it separates the commits.
    let log = [
        &[
            "--literal-pathspecs",
            "log",
            "-p",
            "--no-merges",
            "--full-diff",
            "--format=%x00",
        ],
        DIFF_OPTS,
        &[range.as_str(), "--"],
        &paths,
    ]
    .concat();
    let Ok(commits) = git.run(dir, &log) else {
        return false;
    };
    // Output is decoded lossily, so bytes that are not UTF-8 all read as
    // U+FFFD and different edits could compare equal. A net diff holding one
    // never matches: the safe direction, "not merged". A commit diff equal to
    // one free of it holds none either, so both were decoded exactly.
    if net.contains(char::REPLACEMENT_CHARACTER) {
        return false;
    }
    let net = normalize_diff(&net);
    let found = commits.split('\0').any(|diff| normalize_diff(diff) == net);
    if found {
        tracing::debug!(
            tip,
            target,
            "content check: a target commit has the branch's net diff"
        );
    }
    found
}

/// `diff` without what depends on where it was applied: hunk headers lose
/// their line numbers and `index` lines their blob ids (keeping the mode), and
/// blank lines are dropped (a diff line always has a prefix, so only the
/// separators around a `git log` entry are blank). What each hunk adds,
/// removes, and keeps as context is untouched. A binary file keeps its blob
/// ids: its delta patch has no context lines, and the same delta applied to a
/// different preimage makes a different file.
fn normalize_diff(diff: &str) -> Vec<&str> {
    let lines: Vec<&str> = diff.lines().filter(|line| !line.is_empty()).collect();
    let mut out = Vec::with_capacity(lines.len());
    let mut start = 0;
    while start < lines.len() {
        // One file's section: from its `diff --git` header to the next one.
        let end = lines[start + 1..]
            .iter()
            .position(|line| line.starts_with("diff --git "))
            .map_or(lines.len(), |at| start + 1 + at);
        let section = &lines[start..end];
        let binary = section.contains(&"GIT binary patch");
        out.extend(section.iter().map(|&line| {
            if line.starts_with("@@ ") {
                "@@"
            } else if let Some(rest) = line.strip_prefix("index ").filter(|_| !binary) {
                // `index OLD..NEW[ MODE]`
                rest.split_once(' ').map_or("index", |(_, mode)| mode)
            } else {
                line
            }
        }));
        start = end;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::cli::RealGit;
    use crate::testutil::TestRepo;

    /// One check on its own, nothing reused.
    fn is_content_merged(git: &dyn GitCli, dir: &Path, tip: &str, target: &str) -> bool {
        ContentCheck::new(git, dir).is_merged(tip, target)
    }

    fn merged(repo: &TestRepo, tip: &str) -> bool {
        is_content_merged(&RealGit, repo.root(), tip, "refs/heads/main")
    }

    /// Creates `name` off the current `main`, commits `files` on it (one commit
    /// per file), and returns to `main`.
    fn topic(repo: &TestRepo, name: &str, files: &[(&str, &str)]) {
        repo.git(&["checkout", "-q", "-b", name]);
        for (path, content) in files {
            repo.write(path, content);
            repo.commit_all(&format!("add {path}"));
        }
        repo.git(&["checkout", "-q", "main"]);
    }

    /// Squash-merges `name` into `main` as one new commit.
    fn squash(repo: &TestRepo, name: &str) {
        repo.git(&["merge", "-q", "--squash", name]);
        repo.git(&["commit", "-q", "-m", &format!("squash {name}")]);
    }

    #[test]
    fn a_squash_merged_branch_is_merged() {
        let repo = TestRepo::init();
        topic(&repo, "feat", &[("a.txt", "a\n"), ("b.txt", "b\n")]);
        assert!(!merged(&repo, "feat"));
        squash(&repo, "feat");
        assert!(merged(&repo, "feat"));
    }

    #[test]
    fn a_rebase_merged_branch_is_merged() {
        let repo = TestRepo::init();
        topic(&repo, "feat", &[("a.txt", "a\n"), ("b.txt", "b\n")]);
        repo.write("other.txt", "o\n");
        repo.commit_all("main moves on");
        repo.git(&["cherry-pick", "main..feat"]);
        assert!(merged(&repo, "feat"));
    }

    #[test]
    fn work_split_across_several_squashes_is_merged() {
        // The branch's two changes reach main through two separate "PRs".
        let repo = TestRepo::init();
        topic(&repo, "combo", &[("a.txt", "a\n"), ("b.txt", "b\n")]);
        topic(&repo, "part-a", &[("a.txt", "a\n")]);
        squash(&repo, "part-a");
        assert!(!merged(&repo, "combo"));
        topic(&repo, "part-b", &[("b.txt", "b\n")]);
        squash(&repo, "part-b");
        assert!(merged(&repo, "combo"));
    }

    #[test]
    fn a_merge_only_branch_whose_parents_landed_is_merged() {
        // An integration branch: nothing but merges of branches main later took.
        let repo = TestRepo::init();
        topic(&repo, "x", &[("x.txt", "x\n")]);
        topic(&repo, "y", &[("y.txt", "y\n")]);
        repo.git(&["checkout", "-q", "-b", "integration"]);
        repo.git(&["merge", "-q", "--no-ff", "-m", "merge x", "x"]);
        repo.git(&["merge", "-q", "--no-ff", "-m", "merge y", "y"]);
        repo.git(&["checkout", "-q", "main"]);
        assert!(!merged(&repo, "integration"));
        squash(&repo, "x");
        squash(&repo, "y");
        assert!(merged(&repo, "integration"));
    }

    #[test]
    fn a_change_main_reverted_is_merged() {
        // Its effect is gone from main, but the squash commit still holds it,
        // as a reverted merge commit would.
        let repo = TestRepo::init();
        topic(&repo, "feat", &[("a.txt", "a\n")]);
        squash(&repo, "feat");
        repo.git(&["revert", "--no-edit", "HEAD"]);
        assert!(merged(&repo, "feat"));
    }

    #[test]
    fn a_change_main_edited_since_is_merged() {
        // Main took the change, then rewrote the line: merge-tree conflicts,
        // but the squash commit's tree is the tip's tree.
        let repo = TestRepo::init();
        topic(&repo, "feat", &[("a.txt", "one\n")]);
        squash(&repo, "feat");
        repo.write("a.txt", "two\n");
        repo.commit_all("main edits a");
        assert!(merged(&repo, "feat"));
    }

    #[test]
    fn a_squash_landed_after_main_moved_on_then_edited_is_merged() {
        // The squash commit's tree is not the tip's (main had moved on), but
        // its diff is the branch's net diff, at other line numbers.
        let repo = TestRepo::init();
        repo.write("f.txt", "1\n2\n3\n4\n5\n6\n7\n8\n9\n");
        repo.commit_all("f");
        topic(
            &repo,
            "feat",
            &[("f.txt", "1\n2\n3\n4\n5\n6\n7\nfeat\n8\n9\n")],
        );
        repo.write("f.txt", "0\n1\n2\n3\n4\n5\n6\n7\n8\n9\n");
        repo.write("other.txt", "o\n");
        repo.commit_all("main moves on");
        squash(&repo, "feat");
        repo.write("f.txt", "0\n1\n2\n3\n4\n5\n6\n7\nFEAT\n8\n9\n");
        repo.commit_all("main edits the feat line");
        assert!(merged(&repo, "feat"));
    }

    #[test]
    fn a_main_commit_with_the_change_and_more_is_not_a_squash_match() {
        // Main's commit adds the branch's line *and* another file, then edits
        // the line: the branch's own diff matches no single commit.
        let repo = TestRepo::init();
        topic(&repo, "feat", &[("a.txt", "one\n")]);
        repo.write("other.txt", "o\n");
        repo.commit_all("main moves on");
        repo.write("a.txt", "one\n");
        repo.write("b.txt", "b\n");
        repo.commit_all("main adds a and b");
        repo.write("a.txt", "two\n");
        repo.commit_all("main edits a");
        // Neither tree matches either: the tip lacks `other.txt` and `b.txt`.
        assert!(!merged(&repo, "feat"));
    }

    #[test]
    fn unlanded_work_on_a_path_main_also_edits_is_not_merged() {
        // A conflict, and no commit of main holds the branch's line.
        let repo = TestRepo::init();
        topic(&repo, "feat", &[("a.txt", "branch\n")]);
        repo.write("a.txt", "main\n");
        repo.commit_all("main adds a differently");
        assert!(!merged(&repo, "feat"));
    }

    /// Writes raw bytes to `rel` in the repo's working tree.
    fn write_bytes(repo: &TestRepo, rel: &str, content: &[u8]) {
        std::fs::write(repo.root().join(rel), content).unwrap();
    }

    #[test]
    fn an_edit_differing_only_in_non_utf8_bytes_is_not_merged() {
        // Latin-1 `é` on the branch, `è` on main: both decode lossily to the
        // same replacement character, but the bytes differ.
        let repo = TestRepo::init();
        repo.write("a.txt", "a\nb\nc\n");
        repo.commit_all("base");
        repo.git(&["checkout", "-q", "-b", "feat"]);
        write_bytes(&repo, "a.txt", b"a\nb\xe9\nc\n");
        repo.commit_all("branch edits b");
        repo.git(&["checkout", "-q", "main"]);
        write_bytes(&repo, "a.txt", b"a\nb\xe8\nc\n");
        repo.commit_all("main edits b differently");
        assert!(!merged(&repo, "feat"));
    }

    /// `len` bytes that do not compress, so git encodes an edit of a file
    /// holding them as a delta rather than a literal.
    fn noise(seed: u32, len: usize) -> Vec<u8> {
        let mut state = seed;
        (0..len)
            .map(|_| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                state.to_be_bytes()[0]
            })
            .collect()
    }

    /// A binary file: a NUL, `body`, then 100 copies of `tail`.
    fn blob(body: &[u8], tail: u8) -> Vec<u8> {
        [&[0][..], body, &[tail; 100]].concat()
    }

    #[test]
    fn the_same_binary_delta_on_another_preimage_is_not_merged() {
        // The branch and a main commit both turn the trailing `X`s into `Y`s,
        // each copying an unchanged body the other does not have: the delta
        // patches can be identical, but the files are not.
        let repo = TestRepo::init();
        write_bytes(&repo, "f.bin", &blob(&noise(1, 6000), b'X'));
        repo.commit_all("base");
        repo.git(&["checkout", "-q", "-b", "feat"]);
        write_bytes(&repo, "f.bin", &blob(&noise(1, 6000), b'Y'));
        repo.commit_all("branch edits the tail");
        repo.git(&["checkout", "-q", "main"]);
        write_bytes(&repo, "f.bin", &blob(&noise(2, 6000), b'X'));
        repo.commit_all("main replaces the body");
        write_bytes(&repo, "f.bin", &blob(&noise(2, 6000), b'Y'));
        repo.commit_all("main edits the tail");
        assert!(!merged(&repo, "feat"));
    }

    #[test]
    fn a_binary_squash_main_edited_since_is_merged() {
        let repo = TestRepo::init();
        write_bytes(&repo, "f.bin", &blob(&noise(1, 6000), b'X'));
        repo.commit_all("base");
        repo.git(&["checkout", "-q", "-b", "feat"]);
        write_bytes(&repo, "f.bin", &blob(&noise(1, 6000), b'Y'));
        repo.commit_all("branch edits the tail");
        repo.git(&["checkout", "-q", "main"]);
        squash(&repo, "feat");
        write_bytes(&repo, "f.bin", &blob(&noise(1, 6000), b'Z'));
        repo.commit_all("main edits the tail again");
        assert!(merged(&repo, "feat"));
    }

    #[test]
    fn diffs_compare_without_line_numbers_or_blob_ids() {
        let at = |n: usize, blob: &str| {
            format!(
                "\ndiff --git a/f b/f\nindex {blob}..ffff 100644\n--- a/f\n+++ b/f\n@@ -{n},3 +{n},4 @@ ctx\n a\n+b\n c\n"
            )
        };
        assert_eq!(
            normalize_diff(&at(1, "aaaa")),
            normalize_diff(&at(40, "bbbb"))
        );
        assert_eq!(normalize_diff("index aaaa..bbbb\n"), ["index"]);
        assert_eq!(normalize_diff("index aaaa..bbbb 100755\n"), ["100755"]);
        assert_ne!(
            normalize_diff(&at(1, "a")),
            normalize_diff(&at(1, "a").replace("+b", "+B"))
        );
        // A binary file keeps its blob ids; the text file after it does not.
        let binary = "diff --git a/g b/g\nindex 1111..2222 100644\nGIT binary patch\ndelta 5\n";
        assert_eq!(
            normalize_diff(&format!("{binary}{}", at(1, "aaaa"))),
            [
                "diff --git a/g b/g",
                "index 1111..2222 100644",
                "GIT binary patch",
                "delta 5",
                "diff --git a/f b/f",
                "100644",
                "--- a/f",
                "+++ b/f",
                "@@",
                " a",
                "+b",
                " c",
            ]
        );
    }

    #[test]
    fn a_branch_with_unlanded_work_is_not_merged() {
        let repo = TestRepo::init();
        topic(&repo, "feat", &[("a.txt", "a\n"), ("b.txt", "b\n")]);
        repo.git(&["cherry-pick", "feat~1"]); // only the first commit lands
        assert!(!merged(&repo, "feat"));
    }

    #[test]
    fn a_branch_that_backed_out_its_own_work_is_not_merged() {
        // Its net effect is empty, so merging it changes nothing — but the
        // added file lives only in its history.
        let repo = TestRepo::init();
        topic(&repo, "exp", &[("s.txt", "big work\n")]);
        repo.git(&["checkout", "-q", "exp"]);
        repo.git(&["rm", "-q", "s.txt"]);
        repo.git(&["commit", "-q", "-m", "move away"]);
        repo.git(&["checkout", "-q", "main"]);
        repo.write("other.txt", "o\n");
        repo.commit_all("main moves on");
        assert!(!merged(&repo, "exp"));
    }

    #[test]
    fn a_branch_of_empty_commits_is_not_merged() {
        let repo = TestRepo::init();
        repo.git(&["checkout", "-q", "-b", "empty"]);
        repo.git(&["commit", "-q", "--allow-empty", "-m", "note to self"]);
        repo.git(&["checkout", "-q", "main"]);
        repo.write("other.txt", "o\n");
        repo.commit_all("main moves on");
        assert!(!merged(&repo, "empty"));
    }

    /// Configures an `ours` merge driver for `CHANGELOG.md`, the documented
    /// "keep my side" recipe.
    fn keep_ours_changelog(repo: &TestRepo) {
        repo.git(&["config", "merge.ours.driver", "true"]);
        repo.write(".gitattributes", "CHANGELOG.md merge=ours\n");
        repo.write("CHANGELOG.md", "# Changelog\n");
        repo.commit_all("changelog");
    }

    #[test]
    fn a_custom_merge_driver_cannot_make_unlanded_work_merged() {
        // The driver resolves the branch's CHANGELOG edit to main's side, so a
        // plain merge-tree writes main's tree — though the notes are nowhere
        // in main.
        let repo = TestRepo::init();
        keep_ours_changelog(&repo);
        topic(&repo, "notes", &[("CHANGELOG.md", "# Changelog\nnotes\n")]);
        repo.write("CHANGELOG.md", "# Changelog\nmain\n");
        repo.commit_all("main edits the changelog");
        assert!(!merged(&repo, "notes"));
    }

    #[test]
    fn a_custom_driver_on_untouched_paths_leaves_real_merges_alone() {
        let repo = TestRepo::init();
        keep_ours_changelog(&repo);
        topic(&repo, "feat", &[("a.txt", "a\n")]);
        squash(&repo, "feat");
        assert!(merged(&repo, "feat"));
    }

    #[test]
    fn a_union_default_cannot_make_unlanded_work_merged() {
        // `union` keeps both sides of a conflict: main's edit of `b` plus
        // nothing from the branch's deletion of it, which is main's own tree.
        let repo = TestRepo::init();
        repo.git(&["config", "merge.default", "union"]);
        repo.write("f.txt", "a\nb\nc\n");
        repo.commit_all("f");
        topic(&repo, "del", &[("f.txt", "a\nc\n")]);
        repo.write("f.txt", "a\nB\nc\n");
        repo.commit_all("main edits b");
        assert!(!merged(&repo, "del"));
    }

    #[test]
    fn a_union_attribute_cannot_make_unlanded_work_merged() {
        // The built-in `union` driver, picked by `.gitattributes` rather than
        // config, resolves main's edit of `b` against the branch's deletion of
        // it to main's own tree.
        let repo = TestRepo::init();
        repo.write(".gitattributes", "f.txt merge=union\n");
        repo.write("f.txt", "a\nb\nc\n");
        repo.commit_all("f");
        topic(&repo, "del", &[("f.txt", "a\nc\n")]);
        repo.write("f.txt", "a\nB\nc\n");
        repo.commit_all("main edits b");
        assert!(!merged(&repo, "del"));
    }

    #[test]
    fn a_union_attribute_on_untouched_paths_leaves_real_merges_alone() {
        let repo = TestRepo::init();
        repo.write(".gitattributes", "CHANGELOG.md merge=union\n");
        repo.commit_all("attributes");
        topic(&repo, "feat", &[("a.txt", "a\n")]);
        squash(&repo, "feat");
        assert!(merged(&repo, "feat"));
    }

    #[test]
    fn a_union_file_both_sides_edited_cleanly_is_merged() {
        // The usual `union` changelog: the branch's entry was squashed in,
        // then main added another elsewhere. A text merge is clean and a
        // no-op, so the branch still counts as merged.
        let repo = TestRepo::init();
        repo.write(".gitattributes", "CHANGELOG.md merge=union\n");
        repo.write("CHANGELOG.md", "# log\n\none\n\ntwo\n\nthree\n");
        repo.commit_all("changelog");
        topic(
            &repo,
            "feat",
            &[("CHANGELOG.md", "# log\nfeat\n\none\n\ntwo\n\nthree\n")],
        );
        squash(&repo, "feat");
        repo.write(
            "CHANGELOG.md",
            "# log\nfeat\n\none\n\ntwo\n\nthree\nlater\n",
        );
        repo.commit_all("main adds an entry");
        assert!(merged(&repo, "feat"));
    }

    #[test]
    fn a_user_union_driver_is_still_overridden() {
        // A configured `merge.union.driver` is found by the scan and set to
        // fail, then replaced by the text merge; either way the deletion
        // conflicts.
        let repo = TestRepo::init();
        repo.git(&["config", "merge.union.driver", "true"]);
        repo.write(".gitattributes", "f.txt merge=union\n");
        repo.write("f.txt", "a\nb\nc\n");
        repo.commit_all("f");
        topic(&repo, "del", &[("f.txt", "a\nc\n")]);
        repo.write("f.txt", "a\nB\nc\n");
        repo.commit_all("main edits b");
        assert!(!merged(&repo, "del"));
    }

    #[test]
    fn unreadable_driver_config_is_not_merged() {
        // A config git refuses to parse fails the check rather than skipping
        // the driver overrides.
        let repo = TestRepo::init();
        topic(&repo, "feat", &[("a.txt", "a\n")]);
        squash(&repo, "feat");
        assert!(merged(&repo, "feat"));
        let config = repo.root().join(".git/config");
        let mut text = std::fs::read_to_string(&config).unwrap();
        text.push_str("[broken\n");
        std::fs::write(&config, text).unwrap();
        assert!(!merged(&repo, "feat"));
    }

    #[test]
    fn a_driver_name_that_cannot_be_overridden_fails_safe() {
        // Work split across two squashes after main moved on: only the
        // merge-tree no-op shows it landed, and that needs the overrides.
        let repo = TestRepo::init();
        repo.git(&["config", "merge.a=b.driver", "true"]);
        topic(&repo, "combo", &[("a.txt", "a\n"), ("b.txt", "b\n")]);
        repo.write("other.txt", "o\n");
        repo.commit_all("main moves on");
        topic(&repo, "part-a", &[("a.txt", "a\n")]);
        squash(&repo, "part-a");
        topic(&repo, "part-b", &[("b.txt", "b\n")]);
        squash(&repo, "part-b");
        assert!(!merged(&repo, "combo"));
        repo.git(&["config", "--unset", "merge.a=b.driver"]);
        assert!(merged(&repo, "combo"));
    }

    #[test]
    fn a_squash_match_needs_no_merge_driver_overrides() {
        // The tip's tree is a commit of main, so no merge is consulted.
        let repo = TestRepo::init();
        repo.git(&["config", "merge.a=b.driver", "true"]);
        topic(&repo, "feat", &[("a.txt", "a\n")]);
        squash(&repo, "feat");
        assert!(merged(&repo, "feat"));
    }

    #[test]
    fn unrelated_histories_are_not_merged() {
        let repo = TestRepo::init();
        repo.git(&["checkout", "-q", "--orphan", "stray"]);
        repo.write("stray.txt", "s\n");
        repo.commit_all("unrelated root");
        repo.git(&["checkout", "-q", "main"]);
        assert!(!merged(&repo, "stray"));
    }

    /// Real git that records the subcommand of every call.
    #[derive(Default)]
    struct Counting(RefCell<Vec<String>>);
    impl GitCli for Counting {
        fn run_raw(
            &self,
            repo: &Path,
            args: &[&str],
        ) -> crate::error::Result<crate::git::GitOutput> {
            let verb = args
                .iter()
                .find(|a| !a.starts_with('-') && !a.contains('='))
                .unwrap_or(&"");
            self.0.borrow_mut().push(verb.to_string());
            RealGit.run_raw(repo, args)
        }
    }

    impl Counting {
        fn count(&self, verb: &str) -> usize {
            self.0.borrow().iter().filter(|v| *v == verb).count()
        }
    }

    #[test]
    fn one_check_reads_drivers_and_target_tree_once_for_many_tips() {
        let repo = TestRepo::init();
        for name in ["a", "b", "c"] {
            topic(&repo, name, &[(&format!("{name}.txt"), "x\n")]);
            squash(&repo, name);
        }
        topic(&repo, "open", &[("open.txt", "x\n")]);
        let git = Counting::default();
        let check = ContentCheck::new(&git, repo.root());
        for name in ["a", "b", "c"] {
            assert!(check.is_merged(name, "refs/heads/main"), "{name}");
        }
        assert!(!check.is_merged("open", "refs/heads/main"));
        assert_eq!(git.count("config"), 1);
        assert_eq!(git.count("merge-tree"), 4);
        // One target tree, then one combined base/tip tree read per tip.
        assert_eq!(git.count("rev-parse"), 1 + 4);
        // Only `open`, whose merge is not a no-op, searches main's history.
        assert_eq!(git.count("log"), 2);
        assert_eq!(git.count("diff"), 2);
    }

    #[test]
    fn an_unknown_ref_is_not_merged() {
        let repo = TestRepo::init();
        assert!(!merged(&repo, "no-such-branch"));
        assert!(!is_content_merged(
            &RealGit,
            repo.root(),
            "main",
            "refs/heads/no-such-target"
        ));
    }
}
