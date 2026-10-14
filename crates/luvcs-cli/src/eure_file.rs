//! Reading the CLI's own Eure files (`Luv.eure`, `.luv/remote.eure`) into
//! their types, with errors that say where in the file they are.

use std::path::Path;

use eure_document::parse::{FromEure, ParseError};
use eure_parol::{EureParseError, ParseErrorKind};

/// Reads `content`, the text of the file at `path`, as a `T`.
pub fn read<T>(content: &str, path: &Path) -> anyhow::Result<T>
where
    T: for<'doc> FromEure<'doc, Error = ParseError>,
{
    let at = |offset: Option<u32>| match offset {
        Some(offset) => {
            let (line, column) = line_column(content, offset as usize);
            format!("{}:{line}:{column}", path.display())
        }
        None => path.display().to_string(),
    };
    let cst = eure_parol::parse(content, path).map_err(|e| {
        let (offset, message) = syntax_error(content, &e);
        anyhow::anyhow!("invalid {}: {message}", at(offset))
    })?;
    let (doc, origins) = eure_reader::cst_to_document_and_origin_map(content, &cst)
        .map_err(|e| anyhow::anyhow!("invalid {}: {e}", at(None)))?;
    doc.parse::<T>(doc.get_root_id()).map_err(|e| {
        let span = origins
            .get_value_span(e.node_id, &cst)
            .or_else(|| origins.get_definition_span(e.node_id, &cst));
        anyhow::anyhow!("invalid {}: {}", at(span.map(|s| s.start)), e.kind)
    })
}

/// Where the first syntax error is, and what it is in a person's terms
/// rather than the grammar's.
fn syntax_error(content: &str, error: &EureParseError) -> (Option<u32>, String) {
    let Some(entry) = error.entries.first() else {
        return (None, "syntax error".into());
    };
    match &entry.kind {
        ParseErrorKind::SyntaxError {
            unexpected_tokens, ..
        } => match unexpected_tokens.first() {
            Some(t) => {
                let text = content
                    .get(t.token.start as usize..t.token.end as usize)
                    .unwrap_or("");
                let message = if text.is_empty() {
                    "unexpected end of file".to_owned()
                } else {
                    format!("unexpected `{}`", text.escape_debug())
                };
                (Some(t.token.start), message)
            }
            None => (entry.span.map(|s| s.start), "syntax error".into()),
        },
        ParseErrorKind::LexerError => (entry.span.map(|s| s.start), "invalid character".into()),
        ParseErrorKind::UnprocessedInput => (
            entry.span.map(|s| s.start),
            "unexpected text after the end of the document".into(),
        ),
        ParseErrorKind::Other => (
            entry.span.map(|s| s.start),
            entry
                .message
                .lines()
                .next()
                .unwrap_or("syntax error")
                .to_owned(),
        ),
    }
}

/// The 1-based line and column (in characters) of byte `offset`.
fn line_column(content: &str, offset: usize) -> (usize, usize) {
    let before = &content[..offset.min(content.len())];
    let line = before.matches('\n').count() + 1;
    let column = before.rsplit('\n').next().unwrap_or("").chars().count() + 1;
    (line, column)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::LuvConfig;

    #[test]
    fn errors_name_the_file_and_where_in_it() {
        let path = Path::new("Luv.eure");
        let missing = read::<LuvConfig>("include = []\n", path).unwrap_err();
        assert!(
            missing.to_string().starts_with("invalid Luv.eure"),
            "{missing}"
        );
        assert!(missing.to_string().contains("repo"), "{missing}");
        let mistyped = read::<LuvConfig>("repo = \"notes\"\ninclude = 3\n", path).unwrap_err();
        assert!(
            mistyped.to_string().starts_with("invalid Luv.eure:2:11:"),
            "{mistyped}"
        );
        let syntax = read::<LuvConfig>("repo = = 1\n", path).unwrap_err();
        assert_eq!(syntax.to_string(), "invalid Luv.eure:1:8: unexpected `=`");
    }

    #[test]
    fn line_and_column_count_characters() {
        assert_eq!(line_column("ab\ncdé\nf", 0), (1, 1));
        assert_eq!(line_column("ab\ncdé\nf", 3), (2, 1));
        assert_eq!(line_column("ab\ncdé\nf", 8), (3, 1));
    }
}
