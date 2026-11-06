//! Files with identical content, ignoring line endings and trailing whitespace: copied
//! modules, the most actionable duplication there is.

use std::collections::HashMap;

/// Files shorter than this are skipped, so empty barrels and one-line re-exports don't match.
pub const MIN_LINES: usize = 10;

/// Files whose content is identical; `files` are indices into the input, in input order.
#[derive(Debug, PartialEq)]
pub struct IdenticalFiles {
    pub files: Vec<usize>,
    /// Lines in each copy.
    pub lines: usize,
}

/// Groups of at least two identical `sources` with `min_lines` or more lines, largest first.
pub fn find_identical_files(sources: &[String], min_lines: usize) -> Vec<IdenticalFiles> {
    let mut by_content: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, source) in sources.iter().enumerate() {
        by_content.entry(normalise(source)).or_default().push(i);
    }
    let mut groups: Vec<IdenticalFiles> = by_content
        .into_iter()
        .filter(|(_, files)| files.len() > 1)
        .map(|(content, files)| IdenticalFiles {
            files,
            lines: content.lines().count(),
        })
        .filter(|g| g.lines >= min_lines)
        .collect();
    groups.sort_by(|x, y| y.lines.cmp(&x.lines).then(x.files.cmp(&y.files)));
    groups
}

/// `a \r\n\n` and `a\n\n` compare equal; blank lines are kept, so line counts stay honest.
fn normalise(source: &str) -> String {
    let lines: Vec<&str> = source.lines().map(str::trim_end).collect();
    lines.join("\n").trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_identical_files_ignoring_whitespace_at_line_ends() {
        let body = (0..12).map(|i| format!("line {i}\n")).collect::<String>();
        let sources = vec![
            body.clone(),
            "other\n".repeat(12),
            body.replace('\n', "  \r\n"),
            body.clone(),
        ];
        let groups = find_identical_files(&sources, MIN_LINES);
        assert_eq!(
            groups,
            vec![IdenticalFiles {
                files: vec![0, 2, 3],
                lines: 12
            }]
        );
    }

    #[test]
    fn skips_short_files() {
        let sources = vec!["export * from './a';\n".to_string(); 2];
        assert!(find_identical_files(&sources, MIN_LINES).is_empty());
    }
}
