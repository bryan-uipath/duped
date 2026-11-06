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
/// Bytes are compared undecoded, so files differing only in invalid UTF-8 don't match.
pub fn find_identical_files(sources: &[Vec<u8>], min_lines: usize) -> Vec<IdenticalFiles> {
    let mut by_content: HashMap<Vec<u8>, Vec<usize>> = HashMap::new();
    for (i, source) in sources.iter().enumerate() {
        by_content.entry(normalise(source)).or_default().push(i);
    }
    let mut groups: Vec<IdenticalFiles> = by_content
        .into_iter()
        .filter(|(_, files)| files.len() > 1)
        .map(|(content, files)| IdenticalFiles {
            files,
            lines: content.split(|&b| b == b'\n').count(),
        })
        .filter(|g| g.lines >= min_lines)
        .collect();
    groups.sort_by(|x, y| y.lines.cmp(&x.lines).then(x.files.cmp(&y.files)));
    groups
}

/// `a \r\nb` and `a\nb` compare equal; inner blank lines are kept, trailing ones dropped.
fn normalise(source: &[u8]) -> Vec<u8> {
    let lines: Vec<&[u8]> = source
        .split(|&b| b == b'\n')
        .map(<[u8]>::trim_ascii_end)
        .collect();
    lines.join(&b'\n').trim_ascii_end().to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_identical_files_ignoring_whitespace_at_line_ends() {
        let body = (0..12).map(|i| format!("line {i}\n")).collect::<String>();
        let sources: Vec<Vec<u8>> = [
            body.clone(),
            "other\n".repeat(12),
            body.replace('\n', "  \r\n"),
            body.clone(),
        ]
        .map(String::into_bytes)
        .into();
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
        let sources = vec![b"export * from './a';\n".to_vec(); 2];
        assert!(find_identical_files(&sources, MIN_LINES).is_empty());
    }

    #[test]
    fn invalid_utf8_bytes_still_differ() {
        // Latin-1 `é` and `è`: both would decode to `�`.
        let file = |byte: u8| {
            [
                b"x = \"".as_slice(),
                &[byte],
                b"\"\n",
                &b"# same\n".repeat(11),
            ]
            .concat()
        };
        assert!(find_identical_files(&[file(0xE9), file(0xE8)], MIN_LINES).is_empty());
        assert_eq!(
            find_identical_files(&[file(0xE9), file(0xE9)], MIN_LINES).len(),
            1
        );
    }
}
