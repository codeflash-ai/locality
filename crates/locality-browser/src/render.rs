use std::path::{Path, PathBuf};

use locality_core::model::CanonicalDocument;
use locality_core::{LocalityError, LocalityResult};
use serde::{Deserialize, Serialize};

use crate::connector::BROWSER_CONNECTOR_ID;
use crate::dto::{BrowserSession, BrowserTab, BrowserWindow};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BrowserNativeBundle {
    Session {
        capture_root: PathBuf,
        session: BrowserSession,
    },
    Tab {
        capture_root: PathBuf,
        session: BrowserSession,
        window: BrowserWindow,
        tab: BrowserTab,
        index: usize,
    },
}

pub fn render_browser_entity(bundle: &BrowserNativeBundle) -> LocalityResult<CanonicalDocument> {
    match bundle {
        BrowserNativeBundle::Session {
            capture_root,
            session,
        } => render_session(capture_root, session),
        BrowserNativeBundle::Tab {
            capture_root,
            session,
            window,
            tab,
            index,
        } => render_tab(capture_root, session, window, tab, *index),
    }
}

pub fn remote_version_for_session(session: &BrowserSession) -> String {
    format!(
        "browser:session:{}:{}",
        session.id,
        session.captured_at.as_deref().unwrap_or("unknown")
    )
}

pub fn remote_version_for_tab(session: &BrowserSession, tab: &BrowserTab) -> String {
    format!(
        "browser:tab:{}:{}:{}:{}",
        session.id,
        tab.id,
        tab.captured_at
            .as_deref()
            .or(session.captured_at.as_deref())
            .unwrap_or("unknown"),
        tab.status.as_deref().unwrap_or("captured")
    )
}

fn render_session(
    _capture_root: &Path,
    session: &BrowserSession,
) -> LocalityResult<CanonicalDocument> {
    let tab_count = session
        .windows
        .iter()
        .map(|window| window.tabs.len())
        .sum::<usize>();
    let captured_at = session.captured_at.as_deref().unwrap_or("unknown");
    let frontmatter = format!(
        "loc:\n  id: {}\n  type: asset\n  connector: {}\n  synced_at: {}\n  remote_edited_at: {}\ntitle: {}\nbrowser:\n  kind: session\n  session_id: {}\n  browser: {}\n  profile: {}\n  captured_at: {}\n  tab_count: {}\n",
        yaml_string(&format!("browser:session-summary:{}", session.id)),
        BROWSER_CONNECTOR_ID,
        yaml_string(captured_at),
        yaml_string(&remote_version_for_session(session)),
        yaml_string(&session_title(session)),
        yaml_string(&session.id),
        optional_yaml_string(session.browser.as_deref()),
        optional_yaml_string(session.profile.as_deref()),
        yaml_string(captured_at),
        tab_count,
    );

    let mut body = format!("# {}\n\n", session_title(session));
    body.push_str("This browser session is saved as local files so agents can search it without keeping every tab open.\n\n");
    body.push_str(&format!("- Captured at: `{captured_at}`\n"));
    if let Some(browser) = session.browser.as_deref().filter(|value| !value.is_empty()) {
        body.push_str(&format!("- Browser: `{browser}`\n"));
    }
    if let Some(profile) = session.profile.as_deref().filter(|value| !value.is_empty()) {
        body.push_str(&format!("- Profile: `{profile}`\n"));
    }
    body.push_str(&format!("- Tabs: `{tab_count}`\n\n"));

    for (window_index, window) in session.windows.iter().enumerate() {
        body.push_str(&format!(
            "## Window {}{}\n\n",
            window_index + 1,
            window
                .title
                .as_deref()
                .filter(|title| !title.is_empty())
                .map(|title| format!(": {title}"))
                .unwrap_or_default()
        ));
        for (index, tab) in window.tabs.iter().enumerate() {
            body.push_str(&format!(
                "{}. [{}]({})",
                index + 1,
                markdown_label(&tab.title),
                tab.url
            ));
            if tab.discarded {
                body.push_str(" `discarded`");
            }
            body.push('\n');
        }
        body.push('\n');
    }

    Ok(CanonicalDocument::new(
        frontmatter,
        ensure_trailing_newline(body),
    ))
}

fn render_tab(
    capture_root: &Path,
    session: &BrowserSession,
    window: &BrowserWindow,
    tab: &BrowserTab,
    index: usize,
) -> LocalityResult<CanonicalDocument> {
    if tab.url.trim().is_empty() {
        return Err(LocalityError::InvalidState(format!(
            "browser tab `{}` has no URL",
            tab.id
        )));
    }
    let captured_at = tab
        .captured_at
        .as_deref()
        .or(session.captured_at.as_deref())
        .unwrap_or("unknown");
    let frontmatter = format!(
        "loc:\n  id: {}\n  type: page\n  connector: {}\n  synced_at: {}\n  remote_edited_at: {}\ntitle: {}\nbrowser:\n  kind: tab\n  session_id: {}\n  tab_id: {}\n  window_id: {}\n  index: {}\n  url: {}\n  captured_at: {}\n  status: {}\n  discarded: {}\n  html_path: {}\n  screenshot_path: {}\n  favicon_url: {}\n",
        yaml_string(&format!("browser:tab:{}:{}", session.id, tab.id)),
        BROWSER_CONNECTOR_ID,
        yaml_string(captured_at),
        yaml_string(&remote_version_for_tab(session, tab)),
        yaml_string(&tab_title(tab)),
        yaml_string(&session.id),
        yaml_string(&tab.id),
        yaml_string(&window.id),
        index,
        yaml_string(&tab.url),
        yaml_string(captured_at),
        optional_yaml_string(tab.status.as_deref()),
        tab.discarded,
        optional_yaml_string(tab.html_path.as_deref()),
        optional_yaml_string(tab.screenshot_path.as_deref()),
        optional_yaml_string(tab.favicon_url.as_deref()),
    );

    let mut body = format!("# {}\n\n", tab_title(tab));
    body.push_str(&format!("- URL: {}\n", tab.url));
    body.push_str(&format!("- Session: `{}`\n", session_title(session)));
    body.push_str(&format!("- Captured at: `{captured_at}`\n"));
    if tab.discarded {
        body.push_str("- Browser state: `discarded when captured`\n");
    }
    if let Some(path) = artifact_path(capture_root, tab.html_path.as_deref()) {
        body.push_str(&format!("- Saved HTML: `{}`\n", path.display()));
    }
    if let Some(path) = artifact_path(capture_root, tab.screenshot_path.as_deref()) {
        body.push_str(&format!("- Screenshot: `{}`\n", path.display()));
    }
    body.push('\n');

    if let Some(notes) = tab
        .notes
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        body.push_str("## Notes\n\n");
        body.push_str(&ensure_trailing_newline(notes.to_string()));
        body.push('\n');
    }
    if let Some(selected) = tab
        .selected_text
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        body.push_str("## Selected Text\n\n");
        body.push_str(&ensure_trailing_newline(selected.to_string()));
        body.push('\n');
    }
    body.push_str("## Captured Content\n\n");
    if let Some(content) = tab
        .markdown
        .as_deref()
        .or(tab.text.as_deref())
        .filter(|value| !value.trim().is_empty())
    {
        body.push_str(&ensure_trailing_newline(content.to_string()));
    } else {
        body.push_str("_No readable page content was captured. Reopen the tab or recapture it while the page is loaded._\n");
    }

    Ok(CanonicalDocument::new(
        frontmatter,
        ensure_trailing_newline(body),
    ))
}

fn artifact_path(capture_root: &Path, value: Option<&str>) -> Option<PathBuf> {
    let value = value?.trim();
    if value.is_empty() {
        return None;
    }
    let path = PathBuf::from(value);
    if path.is_absolute() {
        Some(path)
    } else {
        Some(capture_root.join(path))
    }
}

fn session_title(session: &BrowserSession) -> String {
    if session.title.trim().is_empty() {
        "Untitled browser session".to_string()
    } else {
        session.title.clone()
    }
}

fn tab_title(tab: &BrowserTab) -> String {
    if tab.title.trim().is_empty() {
        tab.url.clone()
    } else {
        tab.title.clone()
    }
}

fn markdown_label(value: &str) -> String {
    value.replace('[', "\\[").replace(']', "\\]")
}

fn ensure_trailing_newline(mut value: String) -> String {
    if !value.ends_with('\n') {
        value.push('\n');
    }
    value
}

fn optional_yaml_string(value: Option<&str>) -> String {
    value
        .filter(|value| !value.trim().is_empty())
        .map(yaml_string)
        .unwrap_or_else(|| "null".to_string())
}

fn yaml_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_string())
}
