//! Matches the functions of two sources by name, so the diff pane can connect each pair of blocks.

use crate::cpg::FunctionBlock;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Unchanged,
    Changed,
    Added,
    Removed,
}

/// One row of the diff: an index into the old blocks, into the new blocks, or both when the function was matched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pairing {
    pub status: Status,
    pub old: Option<usize>,
    pub new: Option<usize>,
}

/// Matches functions by name, pairing each old function with the first unclaimed new function of the same name. A pair is
/// unchanged when the text is identical, otherwise changed. Unmatched old functions are removed and unmatched new ones added.
pub fn match_blocks(old: &[FunctionBlock], new: &[FunctionBlock]) -> Vec<Pairing> {
    let mut claimed = vec![false; new.len()];
    let mut pairings = Vec::new();
    for (i, block) in old.iter().enumerate() {
        let found = new
            .iter()
            .enumerate()
            .position(|(j, candidate)| !claimed[j] && candidate.name == block.name);
        match found {
            Some(j) => {
                claimed[j] = true;
                let status = if new[j].text == block.text {
                    Status::Unchanged
                } else {
                    Status::Changed
                };
                pairings.push(Pairing {
                    status,
                    old: Some(i),
                    new: Some(j),
                });
            }
            None => pairings.push(Pairing {
                status: Status::Removed,
                old: Some(i),
                new: None,
            }),
        }
    }
    for (j, _) in claimed.iter().enumerate().filter(|(_, taken)| !**taken) {
        pairings.push(Pairing {
            status: Status::Added,
            old: None,
            new: Some(j),
        });
    }
    pairings
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Pending,
    Applied,
    Nacked,
}

/// Reviewer decisions, one per pairing. Unchanged pairings need no review and start applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Review {
    reviewable: Vec<bool>,
    decisions: Vec<Decision>,
}

impl Review {
    pub fn new(pairings: &[Pairing]) -> Self {
        let reviewable: Vec<bool> = pairings.iter().map(|p| p.status != Status::Unchanged).collect();
        let decisions = reviewable
            .iter()
            .map(|&needs_review| if needs_review { Decision::Pending } else { Decision::Applied })
            .collect();
        Self { reviewable, decisions }
    }

    pub fn decision(&self, index: usize) -> Option<Decision> {
        self.decisions.get(index).copied()
    }

    /// Records `decision` for a reviewable pairing. Returns false for unchanged pairings, out-of-range indices, and
    /// attempts to set `Pending`.
    pub fn decide(&mut self, index: usize, decision: Decision) -> bool {
        if decision == Decision::Pending || !self.reviewable.get(index).copied().unwrap_or(false) {
            return false;
        }
        self.decisions[index] = decision;
        true
    }

    pub fn pending(&self) -> usize {
        self.decisions.iter().filter(|&&d| d == Decision::Pending).count()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplyError {
    /// The reviewable pairing at `index` has no decision yet.
    Pending { index: usize },
}

/// Builds the patched source. Applied changes take the new text, applied removals are deleted, and applied additions are
/// appended at the end. NACKed changes keep the old text. Refuses to run while a reviewable pairing is still pending.
/// `old` must be the blocks parsed from `old_source`.
pub fn apply_review(
    old_source: &str,
    old: &[FunctionBlock],
    new: &[FunctionBlock],
    pairings: &[Pairing],
    review: &Review,
) -> Result<String, ApplyError> {
    let mut edits: Vec<(usize, usize, &str)> = Vec::new();
    let mut appended: Vec<&str> = Vec::new();
    for (index, pairing) in pairings.iter().enumerate() {
        let decision = review.decision(index).unwrap_or(Decision::Pending);
        if decision == Decision::Pending {
            return Err(ApplyError::Pending { index });
        }
        let applied = decision == Decision::Applied;
        match (pairing.old, pairing.new) {
            (Some(i), Some(j)) if applied && old[i].text != new[j].text => {
                edits.push((old[i].start_byte, old[i].end_byte, new[j].text.as_str()));
            }
            (Some(i), None) if applied => edits.push((old[i].start_byte, old[i].end_byte, "")),
            (None, Some(j)) if applied => appended.push(new[j].text.as_str()),
            _ => {}
        }
    }

    edits.sort_by_key(|edit| edit.0);
    let mut patched = String::with_capacity(old_source.len());
    let mut cursor = 0;
    for (start, end, replacement) in edits {
        patched.push_str(&old_source[cursor..start]);
        patched.push_str(replacement);
        cursor = end;
    }
    patched.push_str(&old_source[cursor..]);
    for text in &appended {
        patched.push_str("\n\n");
        patched.push_str(text);
    }
    if !appended.is_empty() {
        patched.push('\n');
    }
    Ok(patched)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(name: &str, text: &str) -> FunctionBlock {
        FunctionBlock {
            name: name.into(),
            start_line: 1,
            start_byte: 0,
            end_byte: text.len(),
            text: text.into(),
        }
    }

    #[test]
    fn unchanged_changed_added_and_removed_functions_are_classified() {
        let old = [block("load", "a"), block("save", "b"), block("drop_cache", "c")];
        let new = [block("load", "a"), block("save", "b2"), block("audit", "d")];
        let pairings = match_blocks(&old, &new);
        let statuses: Vec<Status> = pairings.iter().map(|p| p.status).collect();
        assert_eq!(
            statuses,
            vec![Status::Unchanged, Status::Changed, Status::Removed, Status::Added]
        );
        assert_eq!(pairings[1].old, Some(1));
        assert_eq!(pairings[1].new, Some(1));
        assert_eq!(pairings[3].new, Some(2));
    }

    #[test]
    fn unchanged_pairings_start_applied_and_cannot_be_decided() {
        let pairings = match_blocks(&[block("a", "1"), block("b", "2")], &[block("a", "1"), block("b", "3")]);
        let mut review = Review::new(&pairings);
        assert_eq!(review.decision(0), Some(Decision::Applied));
        assert!(!review.decide(0, Decision::Nacked));
        assert_eq!(review.decision(1), Some(Decision::Pending));
        assert!(review.decide(1, Decision::Nacked));
        assert_eq!(review.decision(1), Some(Decision::Nacked));
        assert_eq!(review.pending(), 0);
    }

    #[test]
    fn duplicate_names_pair_in_order_and_never_twice() {
        let old = [block("helper", "one"), block("helper", "two")];
        let new = [block("helper", "two")];
        let pairings = match_blocks(&old, &new);
        assert_eq!(pairings[0].status, Status::Changed);
        assert_eq!(pairings[0].new, Some(0));
        assert_eq!(pairings[1].status, Status::Removed);
    }
}
