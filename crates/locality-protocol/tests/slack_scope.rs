use std::collections::BTreeSet;

use locality_protocol::{
    HOSTED_SLACK_CHANNEL_SELECTOR_V1_GOLDEN_JSON, HostedSlackChannelSelector,
    MAX_SLACK_CHANNEL_ID_BYTES, MAX_SLACK_TEAM_ID_BYTES, ProviderSourceScopeSelector,
    SLACK_SELECTOR_V1_GOLDEN_JSON, ScopeContractError, SlackChannelSharingClassification,
    SlackInstallationId, SlackInstallationIdError,
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

const INSTALLATION_ID: &str = "0198f3c2-7d4e-7a61-8b25-4f9c8d7e6a10";
const OTHER_INSTALLATION_ID: &str = "0198f3c2-7d4e-7b72-9c36-5a0d9e8f7b21";

fn exact_pretty_json(value: &impl Serialize) -> Vec<u8> {
    let mut bytes = serde_json::to_vec_pretty(value).expect("serialize fixture");
    bytes.push(b'\n');
    bytes
}

fn assert_exact_round_trip<T>(golden: &[u8], expected: &T)
where
    T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug,
{
    let decoded = serde_json::from_slice::<T>(golden).expect("golden value must deserialize");
    assert_eq!(&decoded, expected);
    assert_eq!(exact_pretty_json(&decoded), golden);
}

fn hosted_selector() -> HostedSlackChannelSelector {
    HostedSlackChannelSelector {
        selector_version: 1,
        installation_id: SlackInstallationId::new(INSTALLATION_ID).expect("installation ID"),
        team_id: "T08LOCALITY1".to_string(),
        channel_id: "C08ENGINEER1".to_string(),
        authorized_history_start_at: "2026-01-01T00:00:00Z".to_string(),
        sharing: SlackChannelSharingClassification::ExternallySharedPrivate,
    }
}

fn hosted_provider_selector() -> ProviderSourceScopeSelector {
    ProviderSourceScopeSelector::HostedSlackChannel(hosted_selector())
}

#[test]
fn hosted_slack_channel_selector_is_exact_golden_bytes() {
    let selector = hosted_provider_selector();
    selector.validate().expect("valid hosted Slack selector");
    assert_exact_round_trip(HOSTED_SLACK_CHANNEL_SELECTOR_V1_GOLDEN_JSON, &selector);
}

#[test]
fn existing_direct_slack_selector_bytes_and_validation_are_unchanged() {
    let selector = ProviderSourceScopeSelector::Slack {
        selector_version: 1,
        conversation_id: "C08DIRECT1".to_string(),
    };
    selector.validate().expect("valid direct Slack selector");
    assert_exact_round_trip(SLACK_SELECTOR_V1_GOLDEN_JSON, &selector);

    ProviderSourceScopeSelector::Slack {
        selector_version: 1,
        conversation_id: "legacy direct selector remains nonempty-only".to_string(),
    }
    .validate()
    .expect("hosted validation must not reinterpret the direct Slack selector");
    assert_eq!(
        ProviderSourceScopeSelector::Slack {
            selector_version: 1,
            conversation_id: String::new(),
        }
        .validate(),
        Err(ScopeContractError::EmptyField("conversation_id"))
    );
}

#[test]
fn sharing_classification_has_only_the_four_v1_wire_values() {
    for (classification, wire_value) in [
        (SlackChannelSharingClassification::Public, "public"),
        (SlackChannelSharingClassification::Private, "private"),
        (
            SlackChannelSharingClassification::ExternallySharedPublic,
            "externally_shared_public",
        ),
        (
            SlackChannelSharingClassification::ExternallySharedPrivate,
            "externally_shared_private",
        ),
    ] {
        assert_eq!(
            serde_json::to_value(classification).expect("serialize sharing"),
            json!(wire_value)
        );
        assert_eq!(
            serde_json::from_value::<SlackChannelSharingClassification>(json!(wire_value))
                .expect("deserialize sharing"),
            classification
        );
    }

    let mut unknown = serde_json::to_value(hosted_provider_selector()).expect("selector JSON");
    unknown["sharing"] = json!("future_shared_classification");
    assert!(
        serde_json::from_value::<ProviderSourceScopeSelector>(unknown).is_err(),
        "unknown sharing must fail closed before it can authorize"
    );
}

#[test]
fn hosted_slack_ids_are_bounded_and_canonical() {
    let mut selector = hosted_selector();
    selector.team_id.clear();
    assert_eq!(
        selector.validate(),
        Err(ScopeContractError::EmptyField("team_id"))
    );

    let mut selector = hosted_selector();
    selector.channel_id.clear();
    assert_eq!(
        selector.validate(),
        Err(ScopeContractError::EmptyField("channel_id"))
    );

    for malformed in ["E08LOCALITY1", "t08LOCALITY1", "T08-locality", "T LOCALITY"] {
        let mut selector = hosted_selector();
        selector.team_id = malformed.to_string();
        assert_eq!(
            selector.validate(),
            Err(ScopeContractError::InvalidSlackId("team_id")),
            "accepted malformed team ID {malformed}"
        );
    }
    let mut selector = hosted_selector();
    selector.channel_id = "D08DIRECT1".to_string();
    selector.sharing = SlackChannelSharingClassification::Private;
    selector.validate().expect("valid hosted Slack DM selector");

    for sharing in [
        SlackChannelSharingClassification::Public,
        SlackChannelSharingClassification::ExternallySharedPublic,
        SlackChannelSharingClassification::ExternallySharedPrivate,
    ] {
        let mut selector = hosted_selector();
        selector.channel_id = "D08DIRECT1".to_string();
        selector.sharing = sharing;
        assert_eq!(
            selector.validate(),
            Err(ScopeContractError::InvalidSlackId("channel_id")),
            "accepted DM selector with {sharing:?} sharing"
        );
    }

    for malformed in ["E08LOCALITY1", "c08ENGINEER1", "C08-engineer", "G CHANNEL"] {
        let mut selector = hosted_selector();
        selector.channel_id = malformed.to_string();
        assert_eq!(
            selector.validate(),
            Err(ScopeContractError::InvalidSlackId("channel_id")),
            "accepted malformed channel ID {malformed}"
        );
    }

    let mut selector = hosted_selector();
    selector.team_id = format!("T{}", "A".repeat(MAX_SLACK_TEAM_ID_BYTES));
    assert_eq!(
        selector.validate(),
        Err(ScopeContractError::ValueTooLong {
            field: "team_id",
            maximum_bytes: MAX_SLACK_TEAM_ID_BYTES,
            actual_bytes: MAX_SLACK_TEAM_ID_BYTES + 1,
        })
    );

    let mut selector = hosted_selector();
    selector.channel_id = format!("C{}", "A".repeat(MAX_SLACK_CHANNEL_ID_BYTES));
    assert_eq!(
        selector.validate(),
        Err(ScopeContractError::ValueTooLong {
            field: "channel_id",
            maximum_bytes: MAX_SLACK_CHANNEL_ID_BYTES,
            actual_bytes: MAX_SLACK_CHANNEL_ID_BYTES + 1,
        })
    );

    let mut selector = hosted_selector();
    selector.team_id = format!("T{}", "A".repeat(MAX_SLACK_TEAM_ID_BYTES - 1));
    selector.channel_id = format!("G{}", "9".repeat(MAX_SLACK_CHANNEL_ID_BYTES - 1));
    selector.validate().expect("maximum-length Slack IDs");
}

#[test]
fn installation_identity_is_a_non_nil_canonical_locality_uuid() {
    assert_eq!(
        SlackInstallationId::new("00000000-0000-0000-0000-000000000000"),
        Err(SlackInstallationIdError::Nil)
    );
    for invalid in [
        "0198F3C2-7D4E-7A61-8B25-4F9C8D7E6A10",
        "0198f3c27d4e7a618b254f9c8d7e6a10",
        "{0198f3c2-7d4e-7a61-8b25-4f9c8d7e6a10}",
        " 0198f3c2-7d4e-7a61-8b25-4f9c8d7e6a10",
        "0198f3c2-7d4e-7a61-8b25-4f9c8d7e6a10 ",
        "0198f3c2-7d4e-7a61-8b25-4f9c8d7e6a1g",
    ] {
        assert_eq!(
            SlackInstallationId::new(invalid),
            Err(SlackInstallationIdError::NonCanonical),
            "accepted noncanonical installation UUID {invalid:?}"
        );
    }

    let installation_id = SlackInstallationId::new(INSTALLATION_ID).expect("installation ID");
    assert_eq!(installation_id.as_str(), INSTALLATION_ID);
    assert_eq!(
        serde_json::to_value(&installation_id).unwrap(),
        json!(INSTALLATION_ID)
    );
    assert_eq!(
        format!("{installation_id:?}"),
        format!("SlackInstallationId({INSTALLATION_ID})")
    );

    for invalid in [
        json!("00000000-0000-0000-0000-000000000000"),
        json!("0198F3C2-7D4E-7A61-8B25-4F9C8D7E6A10"),
        json!("0198f3c27d4e7a618b254f9c8d7e6a10"),
        json!("{0198f3c2-7d4e-7a61-8b25-4f9c8d7e6a10}"),
        json!(" 0198f3c2-7d4e-7a61-8b25-4f9c8d7e6a10"),
        json!(42),
    ] {
        let mut value = serde_json::to_value(hosted_provider_selector()).expect("selector JSON");
        value["installation_id"] = invalid;
        assert!(
            serde_json::from_value::<ProviderSourceScopeSelector>(value).is_err(),
            "accepted invalid installation identity"
        );
    }
}

#[test]
fn history_horizon_must_be_a_real_canonical_utc_second() {
    for invalid in [
        "",
        "2026-01-01T00:00:00+00:00",
        "2026-01-01T00:00:00.000Z",
        "2026-01-01t00:00:00z",
        "2026-1-01T00:00:00Z",
        "0000-01-01T00:00:00Z",
        "2026-02-29T00:00:00Z",
        "2024-02-30T00:00:00Z",
        "2026-13-01T00:00:00Z",
        "2026-01-01T24:00:00Z",
        "2026-01-01T00:60:00Z",
        "2026-01-01T00:00:60Z",
    ] {
        let mut selector = hosted_selector();
        selector.authorized_history_start_at = invalid.to_string();
        assert_eq!(
            selector.validate(),
            Err(ScopeContractError::InvalidCanonicalUtcTimestamp(
                "authorized_history_start_at"
            )),
            "accepted invalid history horizon {invalid:?}"
        );
    }

    let mut leap_day = hosted_selector();
    leap_day.authorized_history_start_at = "2024-02-29T23:59:59Z".to_string();
    leap_day.validate().expect("canonical leap-day horizon");
}

#[test]
fn hosted_scope_identity_pins_installation_team_horizon_and_sharing() {
    let original = hosted_provider_selector();

    let mut other_installation = hosted_selector();
    other_installation.installation_id =
        SlackInstallationId::new(OTHER_INSTALLATION_ID).expect("installation ID");
    assert_ne!(
        original,
        ProviderSourceScopeSelector::HostedSlackChannel(other_installation)
    );

    let mut other_team = hosted_selector();
    other_team.team_id = "T08OTHERTEAM".to_string();
    assert_ne!(
        original,
        ProviderSourceScopeSelector::HostedSlackChannel(other_team)
    );

    let mut other_horizon = hosted_selector();
    other_horizon.authorized_history_start_at = "2026-02-01T00:00:00Z".to_string();
    assert_ne!(
        original,
        ProviderSourceScopeSelector::HostedSlackChannel(other_horizon)
    );

    let mut other_sharing = hosted_selector();
    other_sharing.sharing = SlackChannelSharingClassification::Private;
    assert_ne!(
        original,
        ProviderSourceScopeSelector::HostedSlackChannel(other_sharing)
    );
}

#[test]
fn hosted_selector_contains_no_secret_or_mutable_provider_fields() {
    let selector = hosted_provider_selector();
    let value = serde_json::to_value(&selector).expect("selector JSON");
    let keys = value
        .as_object()
        .expect("selector object")
        .keys()
        .cloned()
        .collect::<BTreeSet<_>>();
    assert_eq!(
        keys,
        BTreeSet::from([
            "authorized_history_start_at".to_string(),
            "channel_id".to_string(),
            "installation_id".to_string(),
            "provider".to_string(),
            "selector_version".to_string(),
            "sharing".to_string(),
            "team_id".to_string(),
        ])
    );

    for forbidden_field in ["access_token", "url", "provider_cursor", "display_name"] {
        let mut forbidden = value.clone();
        forbidden[forbidden_field] = Value::String("must-not-appear".to_string());
        assert!(
            serde_json::from_value::<ProviderSourceScopeSelector>(forbidden).is_err(),
            "hosted selector accepted forbidden field {forbidden_field}"
        );
    }

    let debug = format!("{selector:?}");
    assert!(debug.contains(&format!("SlackInstallationId({INSTALLATION_ID})")));
    assert!(debug.contains("T08LOCALITY1"));
    assert!(debug.contains("C08ENGINEER1"));
    for forbidden in [
        "access_token",
        "http://",
        "https://",
        "provider_cursor",
        "display_name",
    ] {
        assert!(!debug.contains(forbidden), "unsafe debug output: {debug}");
    }
}
