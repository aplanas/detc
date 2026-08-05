//! The documentation that an object carries at the head of its own file.
//!
//! Every object of the system is a file that somebody wrote, and every one of
//! them starts by saying what it is for.  That block of comments is the
//! documentation: it is written where the thing it describes is, so it is
//! updated by whoever changes the thing, and a bundle that arrives from
//! somewhere else brings the documentation of what it carries along with it.
//! Nothing here is a catalogue that `detc` keeps, and nothing has to be
//! registered anywhere for `detc doc` to find it.
//!
//! The block is the comments the file opens with, and it ends at the first line
//! that is neither a comment nor blank — which is the line where the file stops
//! describing itself and starts being a probe, a template or a declaration.  A
//! shebang is not part of it: it says how a program is run and not what it
//! does.
//!
//! The comment sign is `#`, and only `#`.  Every format the system uses has it
//! — shell, YAML, TOML, and the configuration files that the templates write —
//! so a second sign would not be another format supported, it would be a guess
//! about which language a file is written in.

use std::path::Path;

use crate::Result;

/// The comment sign, and the whole of what makes a line part of a header.
const COMMENT: char = '#';

/// The interpreter line of a program, which is not documentation.
const SHEBANG: &str = "#!";

/// The documentation at the head of a file, or the reason there is none.
pub fn header(path: impl AsRef<Path>) -> Result<String> {
    let path = path.as_ref();

    let bytes = std::fs::read(path).map_err(|e| format!("Cannot read {}: {e}", path.display()))?;

    let body = String::from_utf8(bytes).map_err(|_| {
        format!(
            "{} is a compiled program and not text, so there is nothing written at the head of it",
            path.display()
        )
    })?;

    match extract(&body) {
        Some(doc) => Ok(doc),
        None => err!(
            "{} says nothing about itself; the documentation of an object is the block of comments it starts with",
            path.display()
        ),
    }
}

/// The block of comments that a document opens with, with the comment sign
/// taken off, or `None` when it opens with something else.
///
/// What is left is the paragraphs as they were written: the lines keep their
/// order, their indentation past the sign, and the blank lines between them,
/// because a header is prose that somebody laid out and reflowing it would
/// close up the lists and the indented examples that are in every one of them.
pub fn extract(body: &str) -> Option<String> {
    let mut lines = body.lines().peekable();

    if lines.peek().is_some_and(|line| line.starts_with(SHEBANG)) {
        lines.next();
    }

    let mut block = Vec::new();
    for line in lines {
        let line = line.trim_start();

        if line.is_empty() {
            block.push("");
            continue;
        }

        let Some(text) = line.strip_prefix(COMMENT) else {
            break;
        };

        // One space after the sign is the sign, and not indentation: it is
        // what separates `# ` from the word, and taking it off is what turns
        // `#     enabled: true` into an example indented by four
        block.push(text.strip_prefix(' ').unwrap_or(text));
    }

    // The blank line under a shebang, and the one that separates the block from
    // the file below it, belong to neither
    let start = block.iter().position(|line| !line.is_empty())?;
    let end = block
        .iter()
        .rposition(|line| !line.is_empty())
        .expect("a block with a first line has a last one");

    Some(block[start..=end].join("\n") + "\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_the_header_is_what_the_file_opens_with() {
        let body = "\
# What this is for.
#
# And the detail of it.

set -eu
# Not documentation: this one is below the code
echo hello
";

        assert_eq!(
            extract(body).expect("the file opens with a comment"),
            "What this is for.\n\nAnd the detail of it.\n"
        );
    }

    /// The line that says how a program is run says nothing about what it does.
    #[test]
    fn test_a_shebang_is_not_part_of_the_header() {
        let body = "#!/bin/sh\n\n# What this probe reports.\n\nset -eu\n";

        assert_eq!(
            extract(body).expect("the program opens with a comment"),
            "What this probe reports.\n"
        );

        // And a program that says nothing but how it is run says nothing
        assert_eq!(extract("#!/bin/sh\nset -eu\n"), None);
    }

    /// A header is prose that somebody laid out, and it comes back the way they
    /// laid it out.
    #[test]
    fn test_the_layout_of_the_header_is_kept() {
        let body = "\
# A unit.
#
# ## Restarting when a file changes
#
#     enabled: true
#     _order: 70
#
# That is the pattern.
enabled: true
";

        assert_eq!(
            extract(body).expect("the document opens with a comment"),
            "A unit.\n\
             \n\
             ## Restarting when a file changes\n\
             \n\
             \x20   enabled: true\n\
             \x20   _order: 70\n\
             \n\
             That is the pattern.\n"
        );
    }

    #[test]
    fn test_a_file_that_says_nothing_about_itself_has_no_header() {
        assert_eq!(extract(""), None);
        assert_eq!(extract("web:\n  enabled: true\n"), None);

        // A block of nothing but signs is a block of nothing
        assert_eq!(extract("#\n#\nweb: true\n"), None);
    }

    #[test]
    fn test_the_header_of_a_file_is_read_and_a_program_is_not() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let path = tmp.path().join("10-core.yaml");
        std::fs::write(&path, "# What this sets.\nweb:\n  enabled: true\n")?;

        assert_eq!(header(&path)?, "What this sets.\n");

        let bare = tmp.path().join("bare.yaml");
        std::fs::write(&bare, "web: true\n")?;
        let error = header(&bare).expect_err("a file that says nothing is reported");
        assert!(
            error.to_string().contains("says nothing about itself"),
            "{error}"
        );

        let compiled = tmp.path().join("compiled");
        std::fs::write(&compiled, [0x7f, b'E', b'L', b'F', 0xff, 0xfe])?;
        let error = header(&compiled).expect_err("a program that is not text is reported");
        assert!(error.to_string().contains("compiled program"), "{error}");

        let error = header(tmp.path().join("missing")).expect_err("a file that is not there");
        assert!(error.to_string().contains("Cannot read"), "{error}");

        Ok(())
    }
}
