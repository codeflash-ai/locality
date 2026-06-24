use locality_core::{LocalityError, LocalityResult};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MarkdownTableShape {
    pub header: Vec<String>,
    pub width: usize,
    pub data_rows: Vec<Vec<String>>,
    pub row_widths: Vec<usize>,
}

pub fn parse_markdown_table_shape(markdown: &str) -> LocalityResult<MarkdownTableShape> {
    let lines = markdown
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<_>>();
    if lines.len() < 2 {
        return Err(malformed_table());
    }

    let header = parse_markdown_table_row(lines[0])?;
    validate_markdown_table_separator(lines[1], header.len())?;
    let data_rows = lines[2..]
        .iter()
        .map(|line| parse_markdown_table_row(line))
        .collect::<LocalityResult<Vec<_>>>()?;
    let row_widths = data_rows.iter().map(Vec::len).collect::<Vec<_>>();
    Ok(MarkdownTableShape {
        width: header.len(),
        header,
        data_rows,
        row_widths,
    })
}

pub fn parse_markdown_table_row(line: &str) -> LocalityResult<Vec<String>> {
    let trimmed = line.trim();
    if !trimmed.starts_with('|') || !trimmed.ends_with('|') || trimmed.len() < 2 {
        return Err(malformed_table());
    }

    let inner = &trimmed[1..trimmed.len() - 1];
    let mut cells = Vec::new();
    let mut current = String::new();
    let mut escaped = false;
    for ch in inner.chars() {
        if ch == '|' && !escaped {
            cells.push(unescape_markdown_table_cell(current.trim()));
            current.clear();
        } else {
            current.push(ch);
        }
        escaped = ch == '\\' && !escaped;
        if ch != '\\' {
            escaped = false;
        }
    }
    cells.push(unescape_markdown_table_cell(current.trim()));

    Ok(cells)
}

pub fn validate_markdown_table_separator(line: &str, width: usize) -> LocalityResult<()> {
    let cells = parse_markdown_table_row(line)?;
    let valid = cells.len() == width
        && cells.iter().all(|cell| {
            let trimmed = cell.trim();
            trimmed.contains('-') && trimmed.chars().all(|ch| matches!(ch, '-' | ':' | ' '))
        });
    if valid {
        Ok(())
    } else {
        Err(malformed_table())
    }
}

fn unescape_markdown_table_cell(cell: &str) -> String {
    let mut unescaped = String::with_capacity(cell.len());
    let mut rest = cell;

    while !rest.is_empty() {
        if let Some(tag) = escaped_break_tag_prefix(rest) {
            unescaped.push('\\');
            unescaped.push_str(tag);
            rest = &rest[tag.len() + 1..];
            continue;
        }
        if let Some(tag) = break_tag_prefix(rest) {
            unescaped.push('\n');
            rest = &rest[tag.len()..];
            continue;
        }
        if rest.starts_with("\\|") {
            unescaped.push('|');
            rest = &rest[2..];
            continue;
        }

        let ch = rest.chars().next().expect("non-empty rest");
        unescaped.push(ch);
        rest = &rest[ch.len_utf8()..];
    }

    unescaped
}

fn escaped_break_tag_prefix(value: &str) -> Option<&'static str> {
    ["<br />", "<br/>", "<br>"].into_iter().find(|tag| {
        value
            .strip_prefix('\\')
            .is_some_and(|rest| rest.starts_with(tag))
    })
}

fn break_tag_prefix(value: &str) -> Option<&'static str> {
    ["<br />", "<br/>", "<br>"]
        .into_iter()
        .find(|tag| value.starts_with(tag))
}

fn malformed_table() -> LocalityError {
    LocalityError::Unsupported("writing malformed Notion tables")
}

#[cfg(test)]
mod tests {
    use super::{parse_markdown_table_row, parse_markdown_table_shape};

    #[test]
    fn parses_table_shape_and_row_widths() {
        let shape = parse_markdown_table_shape("| Name | Status |\n| --- | --- |\n| Old | Todo |")
            .expect("shape");

        assert_eq!(shape.width, 2);
        assert_eq!(shape.header, vec!["Name", "Status"]);
        assert_eq!(shape.data_rows, vec![vec!["Old", "Todo"]]);
        assert_eq!(shape.row_widths, vec![2]);
    }

    #[test]
    fn parses_escaped_pipe_and_line_break_cells() {
        let row = parse_markdown_table_row("| A\\|B | hello<br>world |").expect("row");

        assert_eq!(row, vec!["A|B", "hello\nworld"]);
    }

    #[test]
    fn preserves_escaped_literal_break_tags_for_rich_text_parser() {
        let row = parse_markdown_table_row("| literal \\<br> tag |").expect("row");

        assert_eq!(row, vec!["literal \\<br> tag"]);
    }
}
