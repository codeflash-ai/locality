use similar::TextDiff;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ReadableDiffOutput {
    pub files: Vec<ReadableDiffFileOutput>,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ReadableDiffFileOutput {
    pub path: String,
    pub old_label: String,
    pub new_label: String,
    pub status: ReadableDiffFileStatus,
    pub patch: String,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadableDiffFileStatus {
    Modified,
    Added,
    Deleted,
}

pub fn readable_diff_for_file(
    path: impl Into<String>,
    old_text: Option<&str>,
    new_text: Option<&str>,
) -> Option<ReadableDiffOutput> {
    let path = path.into();
    if old_text == new_text {
        return None;
    }

    let status = match (old_text.is_none(), new_text.is_none()) {
        (true, false) => ReadableDiffFileStatus::Added,
        (false, true) => ReadableDiffFileStatus::Deleted,
        _ => ReadableDiffFileStatus::Modified,
    };
    let old_label = match status {
        ReadableDiffFileStatus::Added => "/dev/null".to_string(),
        ReadableDiffFileStatus::Modified | ReadableDiffFileStatus::Deleted => format!("a/{path}"),
    };
    let new_label = match status {
        ReadableDiffFileStatus::Deleted => "/dev/null".to_string(),
        ReadableDiffFileStatus::Modified | ReadableDiffFileStatus::Added => format!("b/{path}"),
    };
    let old_text = old_text.unwrap_or("");
    let new_text = new_text.unwrap_or("");
    let mut patch_body = TextDiff::from_lines(old_text, new_text)
        .unified_diff()
        .header(&old_label, &new_label)
        .context_radius(3)
        .to_string();
    if patch_body.is_empty() {
        patch_body = format!("--- {old_label}\n+++ {new_label}\n");
    }
    let patch = format!("diff --locality {old_label} {new_label}\n{patch_body}");

    Some(ReadableDiffOutput {
        text: patch.clone(),
        files: vec![ReadableDiffFileOutput {
            path,
            old_label,
            new_label,
            status,
            patch,
        }],
    })
}

pub fn join_readable_diffs(
    diffs: impl IntoIterator<Item = ReadableDiffOutput>,
) -> Option<ReadableDiffOutput> {
    let mut files = Vec::new();
    let mut text = String::new();
    for diff in diffs {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&diff.text);
        files.extend(diff.files);
    }

    if files.is_empty() {
        None
    } else {
        Some(ReadableDiffOutput { files, text })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_modified_file_as_unified_diff() {
        let diff = readable_diff_for_file(
            "Roadmap.md",
            Some("Old paragraph.\n"),
            Some("Changed paragraph.\n"),
        )
        .expect("diff");

        assert_eq!(diff.files.len(), 1);
        assert_eq!(diff.files[0].status, ReadableDiffFileStatus::Modified);
        assert!(
            diff.text
                .contains("diff --locality a/Roadmap.md b/Roadmap.md"),
            "{}",
            diff.text
        );
        assert!(diff.text.contains("--- a/Roadmap.md"), "{}", diff.text);
        assert!(diff.text.contains("+++ b/Roadmap.md"), "{}", diff.text);
        assert!(diff.text.contains("-Old paragraph."), "{}", diff.text);
        assert!(diff.text.contains("+Changed paragraph."), "{}", diff.text);
    }

    #[test]
    fn returns_none_when_text_is_unchanged() {
        let diff = readable_diff_for_file("Roadmap.md", Some("Same.\n"), Some("Same.\n"));

        assert_eq!(diff, None);
    }

    #[test]
    fn renders_added_file_against_dev_null() {
        let diff = readable_diff_for_file("Tasks/new.md", None, Some("# New\n")).expect("diff");

        assert_eq!(diff.files[0].status, ReadableDiffFileStatus::Added);
        assert!(diff.text.contains("--- /dev/null"), "{}", diff.text);
        assert!(diff.text.contains("+++ b/Tasks/new.md"), "{}", diff.text);
        assert!(diff.text.contains("+# New"), "{}", diff.text);
    }

    #[test]
    fn treats_existing_empty_file_with_new_content_as_modified() {
        let diff = readable_diff_for_file("empty.md", Some(""), Some("content\n")).expect("diff");

        assert_eq!(diff.files[0].status, ReadableDiffFileStatus::Modified);
        assert_eq!(diff.files[0].old_label, "a/empty.md");
        assert_eq!(diff.files[0].new_label, "b/empty.md");
        assert!(diff.text.contains("--- a/empty.md"), "{}", diff.text);
        assert!(diff.text.contains("+++ b/empty.md"), "{}", diff.text);
        assert!(diff.text.contains("+content"), "{}", diff.text);
    }

    #[test]
    fn treats_existing_content_cleared_to_empty_as_modified() {
        let diff = readable_diff_for_file("empty.md", Some("content\n"), Some("")).expect("diff");

        assert_eq!(diff.files[0].status, ReadableDiffFileStatus::Modified);
        assert_eq!(diff.files[0].old_label, "a/empty.md");
        assert_eq!(diff.files[0].new_label, "b/empty.md");
        assert!(diff.text.contains("--- a/empty.md"), "{}", diff.text);
        assert!(diff.text.contains("+++ b/empty.md"), "{}", diff.text);
        assert!(diff.text.contains("-content"), "{}", diff.text);
    }

    #[test]
    fn renders_added_empty_file() {
        let diff = readable_diff_for_file("empty.md", None, Some("")).expect("diff");

        assert_eq!(diff.files[0].status, ReadableDiffFileStatus::Added);
        assert_eq!(diff.files[0].old_label, "/dev/null");
        assert_eq!(diff.files[0].new_label, "b/empty.md");
        assert!(diff.text.contains("--- /dev/null"), "{}", diff.text);
        assert!(diff.text.contains("+++ b/empty.md"), "{}", diff.text);
    }

    #[test]
    fn renders_deleted_empty_file() {
        let diff = readable_diff_for_file("empty.md", Some(""), None).expect("diff");

        assert_eq!(diff.files[0].status, ReadableDiffFileStatus::Deleted);
        assert_eq!(diff.files[0].old_label, "a/empty.md");
        assert_eq!(diff.files[0].new_label, "/dev/null");
        assert!(diff.text.contains("--- a/empty.md"), "{}", diff.text);
        assert!(diff.text.contains("+++ /dev/null"), "{}", diff.text);
    }

    #[test]
    fn joins_readable_diffs_with_blank_line_separator() {
        let first = readable_diff_for_file("one.md", Some("one\n"), Some("two\n")).expect("diff");
        let second =
            readable_diff_for_file("three.md", Some("three\n"), Some("four\n")).expect("diff");
        let expected_text = format!("{}\n{}", first.text, second.text);

        let joined = join_readable_diffs(vec![first.clone(), second.clone()]).expect("diff");

        assert_eq!(joined.files, [first.files, second.files].concat());
        assert_eq!(joined.text, expected_text);
    }
}
