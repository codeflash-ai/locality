#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NotionBlockClass {
    Paragraph,
    Heading,
    Quote,
    Callout,
    List,
    Toggle,
    Code,
    Table,
    Equation,
    Mention,
    Media,
    Embed,
    SyncedBlock,
    ChildDatabase,
    ColumnLayout,
    Unsupported,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RoundTripStrategy {
    CleanDiff,
    AnchoredDirective,
    Structural,
    OpaqueShadow,
}

pub fn strategy_for(block: &NotionBlockClass) -> RoundTripStrategy {
    use NotionBlockClass::*;

    match block {
        Paragraph | Heading | Quote | Callout | List | Toggle | Code | Table | Equation
        | Mention => RoundTripStrategy::CleanDiff,
        Media | Embed | SyncedBlock | ColumnLayout => RoundTripStrategy::AnchoredDirective,
        ChildDatabase => RoundTripStrategy::Structural,
        Unsupported => RoundTripStrategy::OpaqueShadow,
    }
}

pub fn directive(id: &str, directive_type: &str, title: Option<&str>) -> String {
    match title {
        Some(title) => format!(
            "::loc{{id={id} type={directive_type} title=\"{}\"}}",
            escape_directive_value(title)
        ),
        None => format!("::loc{{id={id} type={directive_type}}}"),
    }
}

fn escape_directive_value(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    #[test]
    fn directive_escapes_quoted_attribute_values() {
        assert_eq!(
            super::directive("media-1", "image", Some(r#"Quote: "hello" and slash \"#)),
            r#"::loc{id=media-1 type=image title="Quote: \"hello\" and slash \\"}"#
        );
    }
}
