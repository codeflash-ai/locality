use locality_protocol::{SlackChannelSharingClassification, SlackInstallationId};
use locality_slack::portable::hosted::{
    HostedSlackConversationKindV1, HostedSlackInstallationBinding, HostedSlackNativeSnapshot,
    HostedSlackObservedInstallationIdentity, HostedSlackPortableError,
    MAX_HOSTED_SLACK_MESSAGE_FILES, MAX_HOSTED_SLACK_MESSAGE_TEXT_BYTES,
    MAX_HOSTED_SLACK_RAW_JSON_BYTES, MAX_HOSTED_SLACK_SNAPSHOT_REFERENCES,
    MAX_HOSTED_SLACK_SNAPSHOT_STRING_BYTES, RawHostedSlackChannel, RawHostedSlackNativeSnapshot,
    decode_and_sanitize_hosted_slack_native_snapshot,
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::json;

const INSTALLATION_BINDING: &[u8] =
    include_bytes!("../fixtures/hosted-v1/installation-binding.json");
const NATIVE_RAW: &[u8] = include_bytes!("../fixtures/hosted-v1/native-raw.json");
const NATIVE_SANITIZED: &[u8] = include_bytes!("../fixtures/hosted-v1/native-sanitized.json");
const NATIVE_MALICIOUS: &[u8] = include_bytes!("../fixtures/hosted-v1/native-malicious.json");

fn exact_pretty_json(value: &impl Serialize) -> Vec<u8> {
    let mut bytes = serde_json::to_vec_pretty(value).expect("serialize fixture");
    bytes.push(b'\n');
    bytes
}

fn decode_fixture<T: DeserializeOwned>(bytes: &[u8]) -> T {
    serde_json::from_slice(bytes).expect("decode fixture")
}

fn raw_snapshot() -> RawHostedSlackNativeSnapshot {
    decode_fixture(NATIVE_RAW)
}

fn message_mut<'a>(
    raw: &'a mut RawHostedSlackNativeSnapshot,
    timestamp: &str,
) -> &'a mut locality_slack::portable::hosted::RawHostedSlackMessage {
    raw.messages
        .iter_mut()
        .find(|message| message.ts == timestamp)
        .expect("fixture message")
}

fn binding() -> HostedSlackInstallationBinding {
    decode_fixture(INSTALLATION_BINDING)
}

fn observed(binding: &HostedSlackInstallationBinding) -> HostedSlackObservedInstallationIdentity {
    HostedSlackObservedInstallationIdentity {
        api_app_id: binding.api_app_id.clone(),
        team_id: binding.team_id.clone(),
        enterprise_id: binding.enterprise_id.clone(),
        enterprise_install: binding.enterprise_install,
        bot_user_id: binding.bot_user_id.clone(),
        oauth_subject_id: binding.oauth_subject_id.clone(),
    }
}

#[test]
fn installation_binding_is_exact_and_debug_redacts_oauth_subject() {
    let binding = binding();
    binding.validate().expect("valid installation binding");
    assert_eq!(exact_pretty_json(&binding), INSTALLATION_BINDING);
    assert_eq!(
        serde_json::from_slice::<HostedSlackInstallationBinding>(&exact_pretty_json(&binding))
            .expect("round trip binding"),
        binding
    );

    let debug = format!("{binding:?}");
    assert!(debug.contains("<redacted>"));
    assert!(!debug.contains("U08INSTALLER1"));
    assert!(!debug.to_ascii_lowercase().contains("token"));
    assert!(!debug.contains("xox"));

    let mut forbidden = serde_json::to_value(&binding).expect("binding JSON");
    forbidden["access_token"] = json!("xoxb-secret");
    assert!(serde_json::from_value::<HostedSlackInstallationBinding>(forbidden).is_err());
}

#[test]
fn installation_binding_rejects_workspace_app_and_subject_swaps() {
    let binding = binding();
    binding
        .verify_observed_identity(&observed(&binding))
        .expect("matching provider identity");

    let mismatches: [(
        &'static str,
        fn(&mut HostedSlackObservedInstallationIdentity),
    ); 4] = [
        (
            "api_app_id",
            |identity: &mut HostedSlackObservedInstallationIdentity| {
                identity.api_app_id = "A08OTHERAPP1".to_string();
            },
        ),
        (
            "team_id",
            |identity: &mut HostedSlackObservedInstallationIdentity| {
                identity.team_id = "T08OTHERTEAM".to_string();
            },
        ),
        (
            "bot_user_id",
            |identity: &mut HostedSlackObservedInstallationIdentity| {
                identity.bot_user_id = "U08OTHERBOT1".to_string();
            },
        ),
        (
            "oauth_subject_id",
            |identity: &mut HostedSlackObservedInstallationIdentity| {
                identity.oauth_subject_id = "U08OTHERSUB1".to_string();
            },
        ),
    ];
    for (field, mutate) in mismatches {
        let mut identity = observed(&binding);
        mutate(&mut identity);
        assert_eq!(
            binding.verify_observed_identity(&identity),
            Err(HostedSlackPortableError::IdentityMismatch(field))
        );
    }
}

#[test]
fn installation_binding_enforces_enterprise_rules() {
    let workspace_install_in_grid = binding();
    workspace_install_in_grid
        .validate()
        .expect("workspace install may retain its enterprise identity");

    let mut enterprise_install = binding();
    enterprise_install.enterprise_install = true;
    assert_eq!(
        enterprise_install.validate(),
        Err(HostedSlackPortableError::EnterpriseInstallUnsupported)
    );

    enterprise_install.enterprise_id = None;
    assert_eq!(
        enterprise_install.validate(),
        Err(HostedSlackPortableError::EnterpriseInstallUnsupported)
    );

    enterprise_install.enterprise_id = Some("not-an-enterprise-id".to_string());
    assert_eq!(
        enterprise_install.validate(),
        Err(HostedSlackPortableError::EnterpriseInstallUnsupported)
    );

    let mut observed_enterprise_install = observed(&binding());
    observed_enterprise_install.enterprise_install = true;
    assert_eq!(
        observed_enterprise_install.validate(),
        Err(HostedSlackPortableError::EnterpriseInstallUnsupported)
    );

    let mut ordinary_workspace_install = binding();
    ordinary_workspace_install.enterprise_id = None;
    ordinary_workspace_install
        .validate()
        .expect("ordinary workspace install");
}

#[test]
fn sanitized_native_snapshot_is_exact_deterministic_order() {
    let raw = decode_fixture::<RawHostedSlackNativeSnapshot>(NATIVE_RAW);
    let snapshot = decode_and_sanitize_hosted_slack_native_snapshot(NATIVE_RAW)
        .expect("bounded decode and sanitize native data");
    let expected = exact_pretty_json(&snapshot);
    assert_eq!(expected, NATIVE_SANITIZED);

    let sanitized_json = serde_json::to_value(&snapshot).expect("sanitized JSON");
    for file in sanitized_json["files"].as_array().expect("files array") {
        assert_eq!(
            file["capture_receipt"]["status"],
            json!("bytes_not_captured")
        );
    }

    let mut reordered = raw;
    reordered.users.reverse();
    reordered.messages.reverse();
    reordered.threads.reverse();
    reordered.files.reverse();
    for message in &mut reordered.messages {
        message.file_ids.reverse();
    }
    for thread in &mut reordered.threads {
        thread.reply_ts.reverse();
    }
    let reordered =
        HostedSlackNativeSnapshot::try_from(reordered).expect("sanitize reordered native data");
    assert_eq!(exact_pretty_json(&reordered), expected);
}

#[test]
fn self_referential_slack_root_is_normalized_with_replies() {
    let mut raw = raw_snapshot();
    let root_timestamp = "1780000000.000100";
    message_mut(&mut raw, root_timestamp).thread_ts = Some(root_timestamp.to_string());

    let snapshot = HostedSlackNativeSnapshot::try_from(raw).expect("self-referential root");
    assert_eq!(exact_pretty_json(&snapshot), NATIVE_SANITIZED);
}

#[test]
fn root_without_replies_retains_one_empty_thread_record() {
    let mut raw = raw_snapshot();
    let root_timestamp = "1780000000.000100";
    raw.messages.retain(|message| message.ts == root_timestamp);
    raw.threads[0].reply_ts.clear();

    let snapshot = HostedSlackNativeSnapshot::try_from(raw).expect("root with empty thread");
    let json = serde_json::to_value(snapshot).expect("sanitized JSON");
    assert_eq!(json["messages"].as_array().expect("messages").len(), 1);
    assert_eq!(json["threads"].as_array().expect("threads").len(), 1);
    assert_eq!(json["threads"][0]["root_message_id"], json!(root_timestamp));
    assert_eq!(json["threads"][0]["reply_message_ids"], json!([]));
}

#[test]
fn channel_rename_does_not_change_stable_identity() {
    let raw = decode_fixture::<RawHostedSlackNativeSnapshot>(NATIVE_RAW);
    let before = HostedSlackNativeSnapshot::try_from(raw.clone()).expect("snapshot");
    let mut renamed_raw = raw.clone();
    renamed_raw.channel.name = "renamed-engineering".to_string();
    let after = HostedSlackNativeSnapshot::try_from(renamed_raw).expect("renamed snapshot");

    assert_eq!(before.channel_identity(), after.channel_identity());
    assert_ne!(before.channel().name(), after.channel().name());

    let mut other_installation_raw = raw;
    other_installation_raw.installation_id =
        SlackInstallationId::new("0198f3c2-7d4e-7b72-9c36-5a0d9e8f7b21")
            .expect("other installation ID");
    let other_installation = HostedSlackNativeSnapshot::try_from(other_installation_raw)
        .expect("other installation snapshot");
    assert_ne!(
        before.channel_identity(),
        other_installation.channel_identity()
    );
}

#[test]
fn hosted_conversation_kind_defaults_for_legacy_raw_and_validates_channel_ids() {
    assert_eq!(
        HostedSlackConversationKindV1::PublicChannel.root_folder(),
        "channels"
    );
    assert_eq!(
        HostedSlackConversationKindV1::PrivateChannel.root_folder(),
        "private-channels"
    );
    assert_eq!(HostedSlackConversationKindV1::Im.root_folder(), "dms");
    assert_eq!(
        HostedSlackConversationKindV1::Mpim.root_folder(),
        "group-dms"
    );
    assert_eq!(
        HostedSlackConversationKindV1::PublicChannel.source_scope_kind(),
        "slack_channel"
    );
    assert_eq!(
        HostedSlackConversationKindV1::PrivateChannel.source_scope_kind(),
        "slack_channel"
    );
    assert_eq!(
        HostedSlackConversationKindV1::Im.source_scope_kind(),
        "slack_dm"
    );
    assert_eq!(
        HostedSlackConversationKindV1::Mpim.source_scope_kind(),
        "slack_group_dm"
    );

    let legacy = decode_fixture::<RawHostedSlackNativeSnapshot>(NATIVE_RAW);
    assert_eq!(
        legacy.channel.conversation_kind,
        HostedSlackConversationKindV1::PublicChannel
    );
    let legacy_snapshot =
        HostedSlackNativeSnapshot::try_from(legacy).expect("legacy public channel fixture");
    assert_eq!(
        legacy_snapshot.channel().conversation_kind(),
        HostedSlackConversationKindV1::PublicChannel
    );

    for (kind, valid_id, sharing) in [
        (
            HostedSlackConversationKindV1::PublicChannel,
            "C08PUBLIC01",
            SlackChannelSharingClassification::Public,
        ),
        (
            HostedSlackConversationKindV1::PrivateChannel,
            "G08PRIVATE1",
            SlackChannelSharingClassification::Private,
        ),
        (
            HostedSlackConversationKindV1::Im,
            "D08DIRECT01",
            SlackChannelSharingClassification::Private,
        ),
        (
            HostedSlackConversationKindV1::Mpim,
            "G08GROUPDM1",
            SlackChannelSharingClassification::Private,
        ),
    ] {
        let mut raw = raw_snapshot();
        raw.channel.conversation_kind = kind;
        raw.channel.id = valid_id.to_string();
        raw.channel.sharing = sharing;
        for message in &mut raw.messages {
            message.channel_id = valid_id.to_string();
        }
        for thread in &mut raw.threads {
            thread.channel_id = valid_id.to_string();
        }
        for file in &mut raw.files {
            file.channel_id = valid_id.to_string();
        }
        let snapshot = HostedSlackNativeSnapshot::try_from(raw).expect("valid conversation kind");
        assert_eq!(snapshot.channel().conversation_kind(), kind);
    }

    let mut dm_with_channel_id = raw_snapshot();
    dm_with_channel_id.channel.conversation_kind = HostedSlackConversationKindV1::Im;
    assert_eq!(
        HostedSlackNativeSnapshot::try_from(dm_with_channel_id),
        Err(HostedSlackPortableError::InvalidSlackId("channel.id"))
    );

    let mut channel_with_dm_id = raw_snapshot();
    channel_with_dm_id.channel.id = "D08DIRECT01".to_string();
    assert_eq!(
        HostedSlackNativeSnapshot::try_from(channel_with_dm_id),
        Err(HostedSlackPortableError::InvalidSlackId("channel.id"))
    );
}

#[test]
fn raw_boundary_rejects_forbidden_and_unknown_provider_fields() {
    assert_eq!(
        decode_and_sanitize_hosted_slack_native_snapshot(NATIVE_MALICIOUS),
        Err(HostedSlackPortableError::InvalidRawJson)
    );

    let mut unknown_enum =
        serde_json::from_slice::<serde_json::Value>(NATIVE_RAW).expect("raw JSON value");
    unknown_enum["channel"]["sharing"] = json!("future_sharing_classification");
    assert_eq!(
        decode_and_sanitize_hosted_slack_native_snapshot(
            &serde_json::to_vec(&unknown_enum).expect("encode unknown enum")
        ),
        Err(HostedSlackPortableError::InvalidRawJson),
        "unknown sharing classification must fail closed",
    );
}

#[test]
fn sanitized_json_cannot_contain_forbidden_native_keys_or_secrets() {
    let raw = decode_fixture::<RawHostedSlackNativeSnapshot>(NATIVE_RAW);
    let snapshot = HostedSlackNativeSnapshot::try_from(raw).expect("sanitize native data");
    let json = String::from_utf8(exact_pretty_json(&snapshot))
        .expect("sanitized JSON is UTF-8")
        .to_ascii_lowercase();

    for forbidden in [
        "authorization",
        "cookie",
        "headers",
        "url_private",
        "download_url",
        "permalink",
        "raw_response",
        "file_bytes",
        "files-pri",
        "xoxb-",
        "bearer ",
    ] {
        assert!(
            !json.contains(forbidden),
            "sanitized JSON leaked {forbidden}"
        );
    }
}

#[test]
fn raw_conversion_enforces_string_collection_and_timestamp_bounds() {
    let raw = decode_fixture::<RawHostedSlackNativeSnapshot>(NATIVE_RAW);

    let mut oversized_text = raw.clone();
    oversized_text.messages[0].text = "x".repeat(MAX_HOSTED_SLACK_MESSAGE_TEXT_BYTES + 1);
    assert!(matches!(
        HostedSlackNativeSnapshot::try_from(oversized_text),
        Err(HostedSlackPortableError::ValueTooLong {
            field: "message.text",
            ..
        })
    ));

    let mut oversized_files = raw.clone();
    oversized_files.messages[0].file_ids = (0..=MAX_HOSTED_SLACK_MESSAGE_FILES)
        .map(|index| format!("F{index:04}"))
        .collect();
    assert!(matches!(
        HostedSlackNativeSnapshot::try_from(oversized_files),
        Err(HostedSlackPortableError::CollectionTooLarge {
            field: "message.file_ids",
            ..
        })
    ));

    let mut invalid_timestamp = raw;
    invalid_timestamp.channel.created_ts = "1780000000".to_string();
    assert_eq!(
        HostedSlackNativeSnapshot::try_from(invalid_timestamp),
        Err(HostedSlackPortableError::InvalidTimestamp(
            "channel.created_ts"
        ))
    );
}

#[test]
fn bounded_decoder_rejects_raw_and_aggregate_limits() {
    let oversized_raw = vec![b' '; MAX_HOSTED_SLACK_RAW_JSON_BYTES + 1];
    assert_eq!(
        decode_and_sanitize_hosted_slack_native_snapshot(&oversized_raw),
        Err(HostedSlackPortableError::RawInputTooLarge {
            maximum_bytes: MAX_HOSTED_SLACK_RAW_JSON_BYTES,
            actual_bytes: MAX_HOSTED_SLACK_RAW_JSON_BYTES + 1,
        })
    );

    let mut aggregate_text = raw_snapshot();
    let mut prototype = aggregate_text.messages[0].clone();
    prototype.thread_ts = None;
    prototype.file_ids.clear();
    prototype.text = "x".repeat(MAX_HOSTED_SLACK_MESSAGE_TEXT_BYTES);
    aggregate_text.messages = (0..=(MAX_HOSTED_SLACK_SNAPSHOT_STRING_BYTES
        / MAX_HOSTED_SLACK_MESSAGE_TEXT_BYTES))
        .map(|offset| {
            let mut message = prototype.clone();
            message.ts = format!("{}.000100", 1_780_001_000 + offset);
            message
        })
        .collect();
    aggregate_text.threads.clear();
    let aggregate_text_json = serde_json::to_vec(&aggregate_text).expect("encode aggregate text");
    assert!(aggregate_text_json.len() <= MAX_HOSTED_SLACK_RAW_JSON_BYTES);
    assert!(matches!(
        decode_and_sanitize_hosted_slack_native_snapshot(&aggregate_text_json),
        Err(HostedSlackPortableError::ValueTooLong {
            field: "snapshot.string_bytes",
            ..
        })
    ));

    let mut aggregate_replies = raw_snapshot();
    let root_timestamp = "1780000000.000100".to_string();
    let mut root = message_mut(&mut aggregate_replies, &root_timestamp).clone();
    root.file_ids.clear();
    root.text.clear();
    let mut reply_prototype = aggregate_replies.messages[0].clone();
    reply_prototype.thread_ts = Some(root_timestamp.clone());
    reply_prototype.file_ids.clear();
    reply_prototype.text.clear();
    let replies = (1..=MAX_HOSTED_SLACK_SNAPSHOT_REFERENCES + 1)
        .map(|offset| format!("{}.000100", 1_780_010_000 + offset))
        .collect::<Vec<_>>();
    aggregate_replies.messages = std::iter::once(root)
        .chain(replies.iter().map(|timestamp| {
            let mut reply = reply_prototype.clone();
            reply.ts = timestamp.clone();
            reply
        }))
        .collect();
    aggregate_replies.threads[0].reply_ts = replies;
    let aggregate_replies_json =
        serde_json::to_vec(&aggregate_replies).expect("encode aggregate references");
    assert!(aggregate_replies_json.len() <= MAX_HOSTED_SLACK_RAW_JSON_BYTES);
    assert_eq!(
        decode_and_sanitize_hosted_slack_native_snapshot(&aggregate_replies_json),
        Err(HostedSlackPortableError::CollectionTooLarge {
            field: "snapshot.references",
            maximum: MAX_HOSTED_SLACK_SNAPSHOT_REFERENCES,
            actual: MAX_HOSTED_SLACK_SNAPSHOT_REFERENCES + 1,
        })
    );
}

#[test]
fn snapshot_rejects_duplicate_entity_and_nested_reference_ids() {
    let mut duplicate_user = raw_snapshot();
    duplicate_user.users.push(duplicate_user.users[0].clone());
    assert_eq!(
        HostedSlackNativeSnapshot::try_from(duplicate_user),
        Err(HostedSlackPortableError::DuplicateValue("users.user_id"))
    );

    let mut duplicate_message = raw_snapshot();
    duplicate_message
        .messages
        .push(duplicate_message.messages[0].clone());
    assert_eq!(
        HostedSlackNativeSnapshot::try_from(duplicate_message),
        Err(HostedSlackPortableError::DuplicateValue(
            "messages.message_id"
        ))
    );

    let mut duplicate_thread = raw_snapshot();
    duplicate_thread
        .threads
        .push(duplicate_thread.threads[0].clone());
    assert_eq!(
        HostedSlackNativeSnapshot::try_from(duplicate_thread),
        Err(HostedSlackPortableError::DuplicateValue(
            "threads.root_message_id"
        ))
    );

    let mut duplicate_file = raw_snapshot();
    duplicate_file.files.push(duplicate_file.files[0].clone());
    assert_eq!(
        HostedSlackNativeSnapshot::try_from(duplicate_file),
        Err(HostedSlackPortableError::DuplicateValue("files.file_id"))
    );

    let mut duplicate_file_reference = raw_snapshot();
    let file_id = duplicate_file_reference.messages[1].file_ids[0].clone();
    duplicate_file_reference.messages[1].file_ids = vec![file_id.clone(), file_id];
    assert_eq!(
        HostedSlackNativeSnapshot::try_from(duplicate_file_reference),
        Err(HostedSlackPortableError::DuplicateValue("message.file_ids"))
    );

    let mut duplicate_reply_reference = raw_snapshot();
    let reply_id = duplicate_reply_reference.threads[0].reply_ts[0].clone();
    duplicate_reply_reference.threads[0].reply_ts = vec![reply_id.clone(), reply_id];
    assert_eq!(
        HostedSlackNativeSnapshot::try_from(duplicate_reply_reference),
        Err(HostedSlackPortableError::DuplicateValue("thread.reply_ts"))
    );
}

#[test]
fn snapshot_rejects_broken_thread_roots_and_replies() {
    let mut root_without_thread_record = raw_snapshot();
    root_without_thread_record.threads.clear();
    assert_eq!(
        HostedSlackNativeSnapshot::try_from(root_without_thread_record),
        Err(HostedSlackPortableError::MissingReference(
            "messages.thread_record"
        ))
    );

    let mut missing_root = raw_snapshot();
    missing_root.threads[0].root_ts = "1780000999.000100".to_string();
    assert_eq!(
        HostedSlackNativeSnapshot::try_from(missing_root),
        Err(HostedSlackPortableError::MissingReference(
            "threads.root_message_id"
        ))
    );

    let mut non_root = raw_snapshot();
    message_mut(&mut non_root, "1780000000.000100").thread_ts =
        Some("1780000999.000100".to_string());
    assert_eq!(
        HostedSlackNativeSnapshot::try_from(non_root),
        Err(HostedSlackPortableError::InvalidRelationship(
            "threads.root_message_id"
        ))
    );

    let mut missing_reply = raw_snapshot();
    missing_reply.threads[0].reply_ts[0] = "1780000999.000100".to_string();
    assert_eq!(
        HostedSlackNativeSnapshot::try_from(missing_reply),
        Err(HostedSlackPortableError::MissingReference(
            "threads.reply_message_ids"
        ))
    );

    let mut wrong_root = raw_snapshot();
    let mut second_root = message_mut(&mut wrong_root, "1780000000.000100").clone();
    second_root.ts = "1780000100.000100".to_string();
    second_root.file_ids.clear();
    wrong_root.messages.push(second_root);
    message_mut(&mut wrong_root, "1780000001.000200").thread_ts =
        Some("1780000100.000100".to_string());
    assert_eq!(
        HostedSlackNativeSnapshot::try_from(wrong_root),
        Err(HostedSlackPortableError::InvalidRelationship(
            "threads.reply_message_ids"
        ))
    );

    let mut reply_is_root = raw_snapshot();
    message_mut(&mut reply_is_root, "1780000001.000200").thread_ts = None;
    assert_eq!(
        HostedSlackNativeSnapshot::try_from(reply_is_root),
        Err(HostedSlackPortableError::InvalidRelationship(
            "threads.reply_message_ids"
        ))
    );

    let mut orphan_reply = raw_snapshot();
    orphan_reply.threads[0]
        .reply_ts
        .retain(|timestamp| timestamp != "1780000001.000200");
    assert_eq!(
        HostedSlackNativeSnapshot::try_from(orphan_reply),
        Err(HostedSlackPortableError::MissingReference(
            "messages.thread_membership"
        ))
    );

    let mut repeated_reply = raw_snapshot();
    let mut second_root = message_mut(&mut repeated_reply, "1780000000.000100").clone();
    second_root.ts = "1780000100.000100".to_string();
    second_root.file_ids.clear();
    repeated_reply.messages.push(second_root);
    let mut second_thread = repeated_reply.threads[0].clone();
    second_thread.root_ts = "1780000100.000100".to_string();
    second_thread.reply_ts = vec!["1780000001.000200".to_string()];
    repeated_reply.threads.push(second_thread);
    assert_eq!(
        HostedSlackNativeSnapshot::try_from(repeated_reply),
        Err(HostedSlackPortableError::DuplicateReference(
            "threads.reply_message_ids"
        ))
    );
}

#[test]
fn snapshot_rejects_orphan_files_and_cross_channel_relations() {
    let mut missing_message_user = raw_snapshot();
    missing_message_user.messages[0].user_id = Some("U08MISSING01".to_string());
    assert_eq!(
        HostedSlackNativeSnapshot::try_from(missing_message_user),
        Err(HostedSlackPortableError::MissingReference(
            "messages.user_id"
        ))
    );

    let mut missing_file_user = raw_snapshot();
    missing_file_user.files[0].user_id = Some("U08MISSING01".to_string());
    assert_eq!(
        HostedSlackNativeSnapshot::try_from(missing_file_user),
        Err(HostedSlackPortableError::MissingReference("files.user_id"))
    );

    let mut missing_file = raw_snapshot();
    missing_file.messages[1]
        .file_ids
        .push("F08MISSING1".to_string());
    assert_eq!(
        HostedSlackNativeSnapshot::try_from(missing_file),
        Err(HostedSlackPortableError::MissingReference(
            "messages.file_ids"
        ))
    );

    let mut cross_channel_message = raw_snapshot();
    cross_channel_message.messages[0].channel_id = "C08OTHER001".to_string();
    assert_eq!(
        HostedSlackNativeSnapshot::try_from(cross_channel_message),
        Err(HostedSlackPortableError::IdentityMismatch(
            "message.channel_id"
        ))
    );

    let mut cross_channel_thread = raw_snapshot();
    cross_channel_thread.threads[0].channel_id = "C08OTHER001".to_string();
    assert_eq!(
        HostedSlackNativeSnapshot::try_from(cross_channel_thread),
        Err(HostedSlackPortableError::IdentityMismatch(
            "thread.channel_id"
        ))
    );

    let mut cross_channel_file = raw_snapshot();
    cross_channel_file.files[0].channel_id = "C08OTHER001".to_string();
    assert_eq!(
        HostedSlackNativeSnapshot::try_from(cross_channel_file),
        Err(HostedSlackPortableError::IdentityMismatch(
            "file.channel_id"
        ))
    );
}

#[test]
fn installation_binding_reuses_public_protocol_uuid_type() {
    let binding = binding();
    let installation_id: &SlackInstallationId = &binding.installation_id;
    assert_eq!(
        installation_id.as_str(),
        "0198f3c2-7d4e-7a61-8b25-4f9c8d7e6a10"
    );
}

#[allow(dead_code)]
fn raw_channel_type_is_public_for_narrow_provider_boundaries(_: RawHostedSlackChannel) {}
