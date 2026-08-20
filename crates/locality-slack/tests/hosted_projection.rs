use locality_core::portable::LogicalPath;
use locality_protocol::{
    HostedSlackChannelSelector, ProviderSourceScopeSelector, ReplicaFreshnessState,
};
use locality_slack::portable::hosted::{
    HostedSlackConversationKindV1, HostedSlackDocumentKindV1, HostedSlackNativeSnapshot,
    HostedSlackOperationalStatusV1, HostedSlackPortableError, HostedSlackRenderError,
    MAX_HOSTED_SLACK_RENDERED_DOCUMENT_BYTES_V1, MAX_HOSTED_SLACK_RENDERED_PROJECTION_BYTES_V1,
    RawHostedSlackNativeSnapshot, build_hosted_slack_logical_paths_v1,
    decode_and_sanitize_hosted_slack_native_snapshot, render_hosted_slack_projection_v1,
    render_hosted_slack_users_v1,
};
use serde::Serialize;

const SELECTOR: &[u8] =
    include_bytes!("../../locality-protocol/fixtures/hosted-slack-channel-selector-v1.json");
const NATIVE_RAW: &[u8] = include_bytes!("../fixtures/hosted-v1/native-raw.json");
const OPERATIONAL_STATUS: &[u8] =
    include_bytes!("../fixtures/hosted-v1/operational-status-v1.json");
const PATH_MANIFEST: &[u8] =
    include_bytes!("../fixtures/hosted-v1/projection-v1/path-manifest.json");
const CHANNEL_MARKDOWN: &[u8] = include_bytes!(
    "../fixtures/hosted-v1/projection-v1/channels/engineering-C08ENGINEER1/channel.md"
);
const THREAD_MARKDOWN: &[u8] = include_bytes!(
    "../fixtures/hosted-v1/projection-v1/channels/engineering-C08ENGINEER1/threads/2026/05/2026-05-28-1780000000-000100-root-message.md"
);
const PLAN_FILE_MARKDOWN: &[u8] = include_bytes!(
    "../fixtures/hosted-v1/projection-v1/channels/engineering-C08ENGINEER1/files/plan-pdf-F08PLAN0001/metadata.md"
);
const DIAGRAM_FILE_MARKDOWN: &[u8] = include_bytes!(
    "../fixtures/hosted-v1/projection-v1/channels/engineering-C08ENGINEER1/files/diagram-png-F08PLAN0002/metadata.md"
);

fn selector() -> HostedSlackChannelSelector {
    match serde_json::from_slice::<ProviderSourceScopeSelector>(SELECTOR)
        .expect("hosted selector fixture")
    {
        ProviderSourceScopeSelector::HostedSlackChannel(selector) => selector,
        other => panic!("unexpected selector: {other:?}"),
    }
}

fn raw_snapshot() -> RawHostedSlackNativeSnapshot {
    serde_json::from_slice(NATIVE_RAW).expect("raw fixture")
}

fn snapshot() -> HostedSlackNativeSnapshot {
    decode_and_sanitize_hosted_slack_native_snapshot(NATIVE_RAW).expect("sanitized fixture")
}

fn operational_status() -> HostedSlackOperationalStatusV1 {
    serde_json::from_slice(OPERATIONAL_STATUS).expect("operational status fixture")
}

fn exact_pretty_json(value: &impl Serialize) -> Vec<u8> {
    let mut bytes = serde_json::to_vec_pretty(value).expect("serialize JSON");
    bytes.push(b'\n');
    bytes
}

#[test]
fn hosted_projection_paths_and_markdown_are_exact_v1_bytes() {
    let snapshot = snapshot();
    let status = operational_status();
    status
        .validate(&selector())
        .expect("valid operational status");
    assert_eq!(exact_pretty_json(&status), OPERATIONAL_STATUS);
    let projection = render_hosted_slack_projection_v1(&selector(), &status, &snapshot)
        .expect("render projection");
    assert_eq!(exact_pretty_json(projection.paths()), PATH_MANIFEST);

    let expected = [
        (
            "channels/engineering-C08ENGINEER1/channel.md",
            HostedSlackDocumentKindV1::Channel,
            CHANNEL_MARKDOWN,
        ),
        (
            "channels/engineering-C08ENGINEER1/files/diagram-png-F08PLAN0002/metadata.md",
            HostedSlackDocumentKindV1::FileMetadata,
            DIAGRAM_FILE_MARKDOWN,
        ),
        (
            "channels/engineering-C08ENGINEER1/files/plan-pdf-F08PLAN0001/metadata.md",
            HostedSlackDocumentKindV1::FileMetadata,
            PLAN_FILE_MARKDOWN,
        ),
        (
            "channels/engineering-C08ENGINEER1/threads/2026/05/2026-05-28-1780000000-000100-root-message.md",
            HostedSlackDocumentKindV1::Thread,
            THREAD_MARKDOWN,
        ),
    ];
    assert_eq!(projection.documents().len(), expected.len());
    for (document, (logical_path, kind, bytes)) in projection.documents().iter().zip(expected) {
        assert_eq!(document.logical_path().as_str(), logical_path);
        assert_eq!(document.kind(), kind);
        assert_eq!(document.bytes(), bytes);
        assert!(!document.bytes().contains(&b'\r'));
    }
}

#[test]
fn hosted_users_markdown_matches_desktop_users_shape() {
    let snapshot = snapshot();
    let document = render_hosted_slack_users_v1(snapshot.users()).expect("render hosted users.md");
    assert_eq!(document.kind(), HostedSlackDocumentKindV1::Users);
    assert_eq!(document.logical_path().as_str(), "users.md");

    let markdown = String::from_utf8(document.bytes().to_vec()).expect("users Markdown UTF-8");
    assert!(markdown.contains("connector: slack"));
    assert!(markdown.contains("rendered_kind: users"));
    assert!(markdown.contains("| User ID | Name | Display Name | Bot | Deleted |"));

    let ada = markdown.find("| U08ADA00001 |").expect("Ada row");
    let grace = markdown.find("| U08GRACE001 |").expect("Grace row");
    assert!(ada < grace, "{markdown}");
}

#[test]
fn generated_paths_are_portable_bounded_and_authority_suffixed() {
    let paths = build_hosted_slack_logical_paths_v1(&snapshot()).expect("logical paths");
    assert_eq!(
        paths.channel.as_str(),
        "channels/engineering-C08ENGINEER1/channel.md"
    );
    assert_eq!(paths.threads.len(), 1);
    assert_eq!(paths.files.len(), 2);

    for path in std::iter::once(&paths.channel)
        .chain(paths.threads.iter().map(|entry| &entry.logical_path))
        .chain(paths.files.iter().map(|entry| &entry.logical_path))
    {
        assert_eq!(
            LogicalPath::new(path.as_str()).expect("portable path"),
            *path
        );
        assert!(path.as_str().is_ascii());
        assert!(path.as_str().len() <= 1024);
    }
    assert!(paths.channel.as_str().contains("C08ENGINEER1"));
    assert!(
        paths.threads[0]
            .logical_path
            .as_str()
            .contains("1780000000-000100")
    );
    for file in &paths.files {
        assert!(file.logical_path.as_str().contains(&file.file_id));
    }
}

#[test]
fn hosted_paths_use_desktop_roots_for_each_conversation_kind() {
    for (conversation_kind, channel_id, root_folder) in [
        (
            HostedSlackConversationKindV1::PublicChannel,
            "C08PUBLIC01",
            "channels",
        ),
        (
            HostedSlackConversationKindV1::PrivateChannel,
            "G08PRIVATE1",
            "private-channels",
        ),
        (HostedSlackConversationKindV1::Im, "D08DIRECT01", "dms"),
        (
            HostedSlackConversationKindV1::Mpim,
            "G08GROUPDM1",
            "group-dms",
        ),
    ] {
        let mut raw = raw_snapshot();
        raw.channel.conversation_kind = conversation_kind;
        raw.channel.id = channel_id.to_string();
        for message in &mut raw.messages {
            message.channel_id = channel_id.to_string();
        }
        for thread in &mut raw.threads {
            thread.channel_id = channel_id.to_string();
        }
        for file in &mut raw.files {
            file.channel_id = channel_id.to_string();
        }

        let snapshot = HostedSlackNativeSnapshot::try_from(raw).expect("snapshot");
        let paths = build_hosted_slack_logical_paths_v1(&snapshot).expect("logical paths");
        let expected_prefix = format!("{root_folder}/engineering-{channel_id}/");

        assert!(
            paths.channel.as_str().starts_with(&expected_prefix),
            "{}",
            paths.channel.as_str()
        );
        assert_eq!(
            paths.channel_directory(),
            format!("{root_folder}/engineering-{channel_id}")
        );
        for thread in &paths.threads {
            assert!(
                thread.logical_path.as_str().starts_with(&expected_prefix),
                "{}",
                thread.logical_path.as_str()
            );
        }
        for file in &paths.files {
            assert!(
                file.logical_path.as_str().starts_with(&expected_prefix),
                "{}",
                file.logical_path.as_str()
            );
        }
    }
}

#[test]
fn mutable_names_change_only_presentation_slugs_and_id_suffixes_disambiguate() {
    let original = snapshot();
    let original_paths =
        build_hosted_slack_logical_paths_v1(&original).expect("original logical paths");

    let mut renamed = raw_snapshot();
    renamed.channel.name = "Engineering / Renamed?".to_string();
    for file in &mut renamed.files {
        file.name = "same name".to_string();
    }
    let renamed = HostedSlackNativeSnapshot::try_from(renamed).expect("renamed snapshot");
    let renamed_paths =
        build_hosted_slack_logical_paths_v1(&renamed).expect("renamed logical paths");

    assert_eq!(original.channel_identity(), renamed.channel_identity());
    assert_ne!(original_paths.channel, renamed_paths.channel);
    assert_eq!(
        renamed_paths.channel.as_str(),
        "channels/engineering-renamed-C08ENGINEER1/channel.md"
    );
    assert_eq!(renamed_paths.files.len(), 2);
    assert_ne!(
        renamed_paths.files[0].logical_path,
        renamed_paths.files[1].logical_path
    );
    for file in &renamed_paths.files {
        assert!(
            file.logical_path
                .as_str()
                .contains(&format!("same-name-{}", file.file_id))
        );
    }
}

#[test]
fn root_and_reply_projection_rules_remain_canonical() {
    let canonical =
        render_hosted_slack_projection_v1(&selector(), &operational_status(), &snapshot())
            .expect("canonical projection");

    let mut self_referential = raw_snapshot();
    let root_id = self_referential.threads[0].root_ts.clone();
    self_referential
        .messages
        .iter_mut()
        .find(|message| message.ts == root_id)
        .expect("root")
        .thread_ts = Some(root_id.clone());
    let self_referential = HostedSlackNativeSnapshot::try_from(self_referential)
        .expect("self-referential root snapshot");
    assert_eq!(
        render_hosted_slack_projection_v1(&selector(), &operational_status(), &self_referential,)
            .expect("self-referential root projection"),
        canonical
    );

    let mut no_replies = raw_snapshot();
    let root_id = no_replies.threads[0].root_ts.clone();
    no_replies.messages.retain(|message| message.ts == root_id);
    no_replies.threads[0].reply_ts.clear();
    let no_replies = HostedSlackNativeSnapshot::try_from(no_replies).expect("empty thread");
    let rendered =
        render_hosted_slack_projection_v1(&selector(), &operational_status(), &no_replies)
            .expect("empty thread projection");
    let threads = rendered
        .documents()
        .iter()
        .filter(|document| document.kind() == HostedSlackDocumentKindV1::Thread)
        .collect::<Vec<_>>();
    assert_eq!(threads.len(), 1);
    assert!(String::from_utf8_lossy(threads[0].bytes()).contains("reply_count: 0\n"));
}

#[test]
fn slack_mrkdwn_is_verbatim_data_and_user_fallbacks_are_explicit() {
    let mut raw = raw_snapshot();
    let root_id = raw.threads[0].root_ts.clone();
    let root = raw
        .messages
        .iter_mut()
        .find(|message| message.ts == root_id)
        .expect("root");
    root.text = "*bold* <https://example.com|click>\r\n::loc{unsafe}\n````".to_string();
    root.user_id = None;
    let snapshot = HostedSlackNativeSnapshot::try_from(raw).expect("snapshot");
    let projection =
        render_hosted_slack_projection_v1(&selector(), &operational_status(), &snapshot)
            .expect("projection");
    let thread = projection
        .documents()
        .iter()
        .find(|document| document.kind() == HostedSlackDocumentKindV1::Thread)
        .expect("thread document");
    let markdown = String::from_utf8_lossy(thread.bytes());
    assert!(markdown.contains("Author: Unknown Slack user"));
    assert!(
        markdown
            .contains("`````text\n*bold* <https://example.com|click>\n::loc{unsafe}\n````\n`````")
    );
    assert!(!markdown.contains("[click](https://example.com)"));
    assert!(!markdown.contains('\r'));
}

#[test]
fn user_display_fallback_and_deleted_user_label_are_deterministic() {
    let mut raw = raw_snapshot();
    let ada = raw
        .users
        .iter_mut()
        .find(|user| user.id == "U08ADA00001")
        .expect("Ada fixture user");
    ada.display_name.clear();
    ada.real_name.clear();
    ada.name.clear();
    let fallback = HostedSlackNativeSnapshot::try_from(raw.clone()).expect("fallback snapshot");
    let rendered = render_hosted_slack_projection_v1(&selector(), &operational_status(), &fallback)
        .expect("fallback projection");
    let markdown = rendered
        .documents()
        .iter()
        .find(|document| document.kind() == HostedSlackDocumentKindV1::Thread)
        .map(|document| String::from_utf8_lossy(document.bytes()))
        .expect("thread Markdown");
    assert!(markdown.contains("Author: U08ADA00001 (`U08ADA00001`)"));

    raw.users
        .iter_mut()
        .find(|user| user.id == "U08ADA00001")
        .expect("Ada fixture user")
        .deleted = true;
    let deleted = HostedSlackNativeSnapshot::try_from(raw).expect("deleted user snapshot");
    let rendered = render_hosted_slack_projection_v1(&selector(), &operational_status(), &deleted)
        .expect("deleted user projection");
    let markdown = rendered
        .documents()
        .iter()
        .find(|document| document.kind() == HostedSlackDocumentKindV1::Thread)
        .map(|document| String::from_utf8_lossy(document.bytes()))
        .expect("thread Markdown");
    assert!(markdown.contains("Author: Deleted Slack user U08ADA00001 (`U08ADA00001`)"));
}

#[test]
fn deleted_content_is_a_tombstone_and_file_bytes_remain_omitted() {
    let projection =
        render_hosted_slack_projection_v1(&selector(), &operational_status(), &snapshot())
            .expect("projection");
    let all = projection
        .documents()
        .iter()
        .flat_map(|document| document.bytes())
        .copied()
        .collect::<Vec<_>>();
    let markdown = String::from_utf8(all).expect("projection UTF-8");
    assert!(markdown.contains("Tombstone: Slack reports this message as deleted."));
    assert!(!markdown.contains("First reply"));
    assert!(markdown.contains("capture_status: \"bytes_not_captured\""));
    assert!(markdown.contains("Tombstone: Slack reports this file metadata as deleted."));
    for forbidden in [
        "url_private",
        "download_url",
        "permalink",
        "authorization",
        "cookie",
        "raw_response",
        "file_bytes",
        "xoxb-",
        "files-pri",
    ] {
        assert!(!markdown.to_ascii_lowercase().contains(forbidden));
    }
}

#[test]
fn selector_identity_and_sharing_must_match_snapshot() {
    let snapshot = snapshot();
    let mut selector = selector();
    selector.channel_id = "C08OTHER001".to_string();
    assert_eq!(
        render_hosted_slack_projection_v1(&selector, &operational_status(), &snapshot),
        Err(HostedSlackRenderError::ScopeMismatch("channel_id"))
    );
}

#[test]
fn operational_status_is_strict_complete_and_selector_bound() {
    let selector = selector();

    let mut unsupported = operational_status();
    unsupported.status_format_version = 2;
    assert_eq!(
        unsupported.validate(&selector),
        Err(HostedSlackRenderError::UnsupportedOperationalStatusVersion { version: 2 })
    );

    let mut wrong_horizon = operational_status();
    wrong_horizon.authorized_history_start_at = "2026-02-01T00:00:00Z".to_string();
    assert_eq!(
        wrong_horizon.validate(&selector),
        Err(HostedSlackRenderError::OperationalStatusScopeMismatch(
            "authorized_history_start_at"
        ))
    );

    let mut noncanonical = operational_status();
    noncanonical.coverage_end_at = "2026-06-01T00:00:00+00:00".to_string();
    assert_eq!(
        noncanonical.validate(&selector),
        Err(HostedSlackRenderError::InvalidOperationalStatusTimestamp(
            "coverage_end_at"
        ))
    );

    let mut reversed = operational_status();
    reversed.coverage_end_at = reversed.coverage_start_at.clone();
    assert_eq!(
        reversed.validate(&selector),
        Err(HostedSlackRenderError::InvalidOperationalStatusOrder(
            "coverage_start_at must precede coverage_end_at"
        ))
    );

    let mut coverage_after_observation = operational_status();
    coverage_after_observation.freshness_observed_through = "2026-05-31T23:59:59Z".to_string();
    assert_eq!(
        coverage_after_observation.validate(&selector),
        Err(HostedSlackRenderError::InvalidOperationalStatusOrder(
            "coverage_end_at must not follow freshness_observed_through"
        ))
    );

    let mut observation_after_sync = operational_status();
    observation_after_sync.last_successful_sync_at = "2026-05-31T23:59:59Z".to_string();
    assert_eq!(
        observation_after_sync.validate(&selector),
        Err(HostedSlackRenderError::InvalidOperationalStatusOrder(
            "freshness_observed_through must not follow last_successful_sync_at"
        ))
    );

    let mut incomplete = operational_status();
    incomplete.coverage_complete = false;
    assert_eq!(
        incomplete.validate(&selector),
        Err(HostedSlackRenderError::IncompleteCoverage)
    );

    let mut bootstrapping = operational_status();
    bootstrapping.freshness_state = ReplicaFreshnessState::Bootstrapping;
    assert_eq!(
        bootstrapping.validate(&selector),
        Err(HostedSlackRenderError::UnrenderableFreshnessState(
            ReplicaFreshnessState::Bootstrapping
        ))
    );

    let mut unknown = serde_json::to_value(operational_status()).expect("status JSON");
    unknown["provider_cursor"] = serde_json::json!("unsafe");
    assert!(serde_json::from_value::<HostedSlackOperationalStatusV1>(unknown).is_err());
}

#[test]
fn authorized_history_horizon_is_an_exact_lower_root_boundary() {
    let mut raw = raw_snapshot();
    let old_root = raw.threads[0].root_ts.clone();
    let boundary_root = "1780000000.000000";
    raw.threads[0].root_ts = boundary_root.to_string();
    for message in &mut raw.messages {
        if message.ts == old_root {
            message.ts = boundary_root.to_string();
        }
        if message.thread_ts.as_deref() == Some(old_root.as_str()) {
            message.thread_ts = Some(boundary_root.to_string());
        }
    }
    let boundary = HostedSlackNativeSnapshot::try_from(raw).expect("boundary snapshot");
    let mut scoped_selector = selector();
    scoped_selector.authorized_history_start_at = "2026-05-28T20:26:40Z".to_string();
    let mut status = operational_status();
    status.authorized_history_start_at = scoped_selector.authorized_history_start_at.clone();
    status.coverage_start_at = scoped_selector.authorized_history_start_at.clone();
    render_hosted_slack_projection_v1(&scoped_selector, &status, &boundary)
        .expect("root exactly at authorized horizon");

    scoped_selector.authorized_history_start_at = "2026-05-28T20:26:41Z".to_string();
    status.authorized_history_start_at = scoped_selector.authorized_history_start_at.clone();
    status.coverage_start_at = scoped_selector.authorized_history_start_at.clone();
    assert_eq!(
        render_hosted_slack_projection_v1(&scoped_selector, &status, &snapshot()),
        Err(HostedSlackRenderError::RootOutsideCoverage {
            root_message_id: "1780000000.000100".to_string(),
        })
    );

    let mut at_upper_cut = raw_snapshot();
    let old_root = at_upper_cut.threads[0].root_ts.clone();
    let upper_cut_root = "1780272000.000000";
    at_upper_cut
        .messages
        .retain(|message| message.ts == old_root);
    at_upper_cut.messages[0].ts = upper_cut_root.to_string();
    at_upper_cut.messages[0].thread_ts = None;
    at_upper_cut.threads[0].root_ts = upper_cut_root.to_string();
    at_upper_cut.threads[0].reply_ts.clear();
    let at_upper_cut = HostedSlackNativeSnapshot::try_from(at_upper_cut).expect("upper cut root");
    assert_eq!(
        render_hosted_slack_projection_v1(&selector(), &operational_status(), &at_upper_cut),
        Err(HostedSlackRenderError::RootOutsideCoverage {
            root_message_id: upper_cut_root.to_string(),
        })
    );
}

#[test]
fn hostile_mimetype_and_metadata_controls_fail_before_rendering() {
    let mut hostile_mimetype = raw_snapshot();
    hostile_mimetype.files[0].mimetype = "text/`x`](https://evil.example)".to_string();
    assert_eq!(
        HostedSlackNativeSnapshot::try_from(hostile_mimetype),
        Err(HostedSlackPortableError::InvalidMimetype)
    );

    let mut hostile_channel = raw_snapshot();
    hostile_channel.channel.name = "engineering\u{007f}injected".to_string();
    assert_eq!(
        HostedSlackNativeSnapshot::try_from(hostile_channel),
        Err(HostedSlackPortableError::MetadataContainsControl(
            "channel.name"
        ))
    );

    let mut hostile_user = raw_snapshot();
    hostile_user.users[0].display_name = "Grace\u{0085}injected".to_string();
    assert_eq!(
        HostedSlackNativeSnapshot::try_from(hostile_user),
        Err(HostedSlackPortableError::MetadataContainsControl(
            "user.display_name"
        ))
    );

    let mut hostile_file = raw_snapshot();
    hostile_file.files[0].title = "Diagram\u{009f}injected".to_string();
    assert_eq!(
        HostedSlackNativeSnapshot::try_from(hostile_file),
        Err(HostedSlackPortableError::MetadataContainsControl(
            "file.title"
        ))
    );
}

#[test]
fn message_body_controls_remain_canonical_data() {
    let mut raw = raw_snapshot();
    let root_id = raw.threads[0].root_ts.clone();
    raw.messages
        .iter_mut()
        .find(|message| message.ts == root_id)
        .expect("root")
        .text = "body\u{007f}\u{0085}".to_string();
    let snapshot = HostedSlackNativeSnapshot::try_from(raw).expect("message controls are data");
    let rendered = render_hosted_slack_projection_v1(&selector(), &operational_status(), &snapshot)
        .expect("render message controls");
    let markdown = rendered
        .documents()
        .iter()
        .map(|document| String::from_utf8_lossy(document.bytes()))
        .collect::<String>();
    assert!(markdown.contains("body\\u{007f}\\u{0085}"));
    assert!(!markdown.contains('\u{007f}'));
    assert!(!markdown.contains('\u{0085}'));
}

#[test]
fn rendered_output_limits_fail_closed() {
    let mut oversized_document = raw_snapshot();
    for message in &mut oversized_document.messages {
        message.deleted = false;
        message.text = "\0".repeat(40_000);
    }
    let oversized_document =
        HostedSlackNativeSnapshot::try_from(oversized_document).expect("bounded native snapshot");
    assert!(matches!(
        render_hosted_slack_projection_v1(&selector(), &operational_status(), &oversized_document),
        Err(HostedSlackRenderError::DocumentTooLarge {
            maximum_bytes: MAX_HOSTED_SLACK_RENDERED_DOCUMENT_BYTES_V1,
            ..
        })
    ));

    let mut oversized_projection = raw_snapshot();
    let mut root = oversized_projection
        .messages
        .iter()
        .find(|message| message.thread_ts.is_none())
        .expect("root")
        .clone();
    root.file_ids.clear();
    root.text = "\0".repeat(12_000);
    oversized_projection.files.clear();
    oversized_projection.messages.clear();
    oversized_projection.threads.clear();
    for offset in 0..10 {
        let timestamp = format!("{}.000100", 1_780_100_000 + offset);
        let mut message = root.clone();
        message.ts = timestamp.clone();
        message.thread_ts = None;
        oversized_projection.messages.push(message);
        let mut thread = raw_snapshot().threads[0].clone();
        thread.root_ts = timestamp;
        thread.reply_ts.clear();
        oversized_projection.threads.push(thread);
    }
    let oversized_projection = HostedSlackNativeSnapshot::try_from(oversized_projection)
        .expect("aggregate-bounded native snapshot");
    assert!(matches!(
        render_hosted_slack_projection_v1(
            &selector(),
            &operational_status(),
            &oversized_projection
        ),
        Err(HostedSlackRenderError::ProjectionTooLarge {
            maximum_bytes: MAX_HOSTED_SLACK_RENDERED_PROJECTION_BYTES_V1,
            ..
        })
    ));
}
