use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;
use std::time::Duration;

use locality_connector::ConnectorExecutionPolicy;
use locality_core::LocalityError;
use locality_slack::client::{HttpSlackApiClient, SlackApi};

#[test]
fn list_public_channels_sends_slack_conversations_list_query() {
    let (base_url, request, server) = spawn_response_server(
        "HTTP/1.1 200 OK",
        r#"{"ok":true,"channels":[{"id":"C123","name":"general","is_channel":true,"is_archived":false}],"response_metadata":{"next_cursor":"next"}}"#,
    );
    let client = HttpSlackApiClient::with_base_url("xoxb-token", base_url);

    let response = client
        .list_public_channels(15, Some("cursor-1"))
        .expect("channels");

    assert_eq!(response.channels[0].id, "C123");
    assert_eq!(response.next_cursor.as_deref(), Some("next"));
    let request = request.recv().expect("request");
    server.join().expect("server exits");
    assert!(request.contains("GET /api/conversations.list?"));
    assert!(request.contains("types=public_channel"));
    assert!(request.contains("exclude_archived=true"));
    assert!(request.contains("limit=15"));
    assert!(request.contains("cursor=cursor-1"));
    assert!(request.contains("authorization: Bearer xoxb-token"));
}

#[test]
fn conversation_info_sends_slack_conversations_info_query() {
    let (base_url, request, server) = spawn_response_server(
        "HTTP/1.1 200 OK",
        r#"{"ok":true,"channel":{"id":"C123","name":"general","is_channel":true,"is_archived":false}}"#,
    );
    let client = HttpSlackApiClient::with_base_url("xoxb-token", base_url);

    let response = client.conversation_info("C123").expect("channel info");

    assert_eq!(response.id, "C123");
    assert_eq!(response.name, "general");
    let request = request.recv().expect("request");
    server.join().expect("server exits");
    assert!(request.contains("GET /api/conversations.info?"));
    assert!(request.contains("channel=C123"));
    assert!(request.contains("authorization: Bearer xoxb-token"));
}

#[test]
fn deferred_execution_maps_429_retry_after_to_structured_rate_limit() {
    let (base_url, _request, server) = spawn_response_server(
        "HTTP/1.1 429 Too Many Requests\r\nretry-after: 7",
        r#"{"ok":false,"error":"ratelimited"}"#,
    );
    let client = HttpSlackApiClient::with_base_url_and_execution_policy(
        "xoxb-token",
        base_url,
        ConnectorExecutionPolicy::DeferProviderCooldown,
    );

    let error = client
        .conversation_history("C123", 15, None)
        .expect_err("rate limited");

    server.join().expect("server exits");
    assert!(matches!(
        error,
        LocalityError::RateLimited { provider, retry_after, message }
            if provider == "slack"
                && retry_after == Duration::from_secs(7)
                && message.contains("ratelimited")
    ));
}

fn spawn_response_server(
    status: &'static str,
    body: &'static str,
) -> (
    String,
    std::sync::mpsc::Receiver<String>,
    thread::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind server");
    let base_url = format!("http://{}", listener.local_addr().expect("local addr"));
    let (tx, rx) = std::sync::mpsc::channel();
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept request");
        let mut buffer = [0_u8; 8192];
        let read = stream.read(&mut buffer).expect("read request");
        tx.send(String::from_utf8_lossy(&buffer[..read]).to_string())
            .expect("send request");
        let response = format!(
            "{status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        stream
            .write_all(response.as_bytes())
            .expect("write response");
    });
    (base_url, rx, handle)
}
