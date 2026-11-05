use std::ops::Range;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub base: Option<usize>,
    pub head: Option<usize>,
    pub changed: bool,
}

#[derive(Default)]
pub struct LineDiff {
    pub rows: Vec<Row>,
    pub hunks: Vec<Range<usize>>,
    pub coarse: bool,
}

impl LineDiff {
    pub fn build(base: &str, head: &str) -> Self {
        let before = base.split_inclusive('\n').collect::<Vec<_>>();
        let after = head.split_inclusive('\n').collect::<Vec<_>>();
        let prefix = before
            .iter()
            .zip(&after)
            .take_while(|(left, right)| left == right)
            .count();
        let suffix = before[prefix..]
            .iter()
            .rev()
            .zip(after[prefix..].iter().rev())
            .take_while(|(left, right)| left == right)
            .count();
        let left = &before[prefix..before.len() - suffix];
        let right = &after[prefix..after.len() - suffix];
        let mut pairs = (0..prefix).map(|index| (index, index)).collect::<Vec<_>>();
        let cells = (left.len() + 1).saturating_mul(right.len() + 1);
        let coarse = cells > 2_000_000 && !left.is_empty() && !right.is_empty();
        if !coarse && !left.is_empty() && !right.is_empty() {
            let stride = right.len() + 1;
            let mut lengths = vec![0u32; cells];
            for base_index in (0..left.len()).rev() {
                for head_index in (0..right.len()).rev() {
                    lengths[base_index * stride + head_index] =
                        if left[base_index] == right[head_index] {
                            lengths[(base_index + 1) * stride + head_index + 1] + 1
                        } else {
                            lengths[(base_index + 1) * stride + head_index]
                                .max(lengths[base_index * stride + head_index + 1])
                        };
                }
            }
            let (mut base_index, mut head_index) = (0, 0);
            while base_index < left.len() && head_index < right.len() {
                if left[base_index] == right[head_index] {
                    pairs.push((prefix + base_index, prefix + head_index));
                    base_index += 1;
                    head_index += 1;
                } else if lengths[(base_index + 1) * stride + head_index]
                    >= lengths[base_index * stride + head_index + 1]
                {
                    base_index += 1;
                } else {
                    head_index += 1;
                }
            }
        }
        pairs.extend((0..suffix).map(|offset| {
            (
                before.len() - suffix + offset,
                after.len() - suffix + offset,
            )
        }));
        pairs.push((before.len(), after.len()));
        let mut result = Self {
            coarse,
            ..Default::default()
        };
        let (mut base_index, mut head_index) = (0, 0);
        for (base_match, head_match) in pairs {
            let count = (base_match - base_index).max(head_match - head_index);
            if count > 0 {
                let start = result.rows.len();
                for offset in 0..count {
                    result.rows.push(Row {
                        base: (base_index + offset < base_match).then_some(base_index + offset),
                        head: (head_index + offset < head_match).then_some(head_index + offset),
                        changed: true,
                    });
                }
                result.hunks.push(start..result.rows.len());
            }
            if base_match < before.len() && head_match < after.len() {
                result.rows.push(Row {
                    base: Some(base_match),
                    head: Some(head_match),
                    changed: false,
                });
            }
            base_index = base_match + 1;
            head_index = head_match + 1;
        }
        result
    }

    pub fn row_for_line(&self, side: &str, line: usize) -> Option<usize> {
        let index = line.checked_sub(1)?;
        self.rows
            .iter()
            .position(|row| (if side == "base" { row.base } else { row.head }) == Some(index))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_reconstructs(base: &str, head: &str) -> LineDiff {
        let diff = LineDiff::build(base, head);
        let before = base.split_inclusive('\n').collect::<Vec<_>>();
        let after = head.split_inclusive('\n').collect::<Vec<_>>();
        assert_eq!(
            diff.rows
                .iter()
                .filter_map(|row| row.base)
                .collect::<Vec<_>>(),
            (0..before.len()).collect::<Vec<_>>()
        );
        assert_eq!(
            diff.rows
                .iter()
                .filter_map(|row| row.head)
                .collect::<Vec<_>>(),
            (0..after.len()).collect::<Vec<_>>()
        );
        for row in diff.rows.iter().filter(|row| !row.changed) {
            assert_eq!(before[row.base.unwrap()], after[row.head.unwrap()]);
        }
        diff
    }

    #[test]
    fn insertions_deletions_replacements_and_repeated_unicode_lines_align() {
        for (base, head) in [
            ("", ""),
            ("", "新增\n"),
            ("删除\n", ""),
            ("a\n中文😀\nc\n", "a\n新增\n中文😀\nc\n"),
            ("a\nx\nx\nz\n", "a\nx\nz\n"),
            ("same\nold\nend\n", "same\nnew\nend\n"),
            ("a\r\n", "a\n"),
            ("a\n", "a"),
        ] {
            assert_reconstructs(base, head);
        }
        let diff = assert_reconstructs("a\nb\nc\n", "a\ninsert\nb\nc\n");
        assert_eq!(diff.hunks, vec![1..2]);
        assert_eq!(
            diff.rows[1],
            Row {
                base: None,
                head: Some(1),
                changed: true
            }
        );
        assert_eq!(diff.row_for_line("base", 3), Some(3));
        assert_eq!(diff.row_for_line("head", 0), None);
    }

    #[test]
    fn large_replacements_have_a_bounded_fallback_without_losing_lines() {
        let diff = assert_reconstructs(&"before\n".repeat(3000), &"after\n".repeat(3000));
        assert!(diff.coarse);
        assert_eq!(diff.hunks, vec![0..3000]);
        let same = "same\n".repeat(3000);
        assert!(!assert_reconstructs(&same, &(same.clone() + "extra\n")).coarse);
    }
}
