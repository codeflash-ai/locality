//! Browser-safe hosted connector product metadata.
//!
//! This module contains only product vocabulary shared by Locality runtimes.
//! It does not grant OAuth authority, credential access, setup capability,
//! worker dispatch, or provider network permission.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostedConnectorProduct {
    pub provider_kind: &'static str,
    pub display_name: &'static str,
    pub content_singular: &'static str,
    pub content_plural: &'static str,
}

pub const NOTION_PRODUCT: HostedConnectorProduct = HostedConnectorProduct {
    provider_kind: "notion",
    display_name: "Notion",
    content_singular: "folder",
    content_plural: "folders",
};

pub const GOOGLE_DOCS_PRODUCT: HostedConnectorProduct = HostedConnectorProduct {
    provider_kind: "google-docs",
    display_name: "Google Docs",
    content_singular: "folder",
    content_plural: "folders",
};

pub const GOOGLE_CALENDAR_PRODUCT: HostedConnectorProduct = HostedConnectorProduct {
    provider_kind: "google-calendar",
    display_name: "Google Calendar",
    content_singular: "calendar",
    content_plural: "calendars",
};

pub const GMAIL_PRODUCT: HostedConnectorProduct = HostedConnectorProduct {
    provider_kind: "gmail",
    display_name: "Gmail",
    content_singular: "label",
    content_plural: "labels",
};

pub const SLACK_PRODUCT: HostedConnectorProduct = HostedConnectorProduct {
    provider_kind: "slack",
    display_name: "Slack",
    content_singular: "channel",
    content_plural: "channels",
};

pub const HOSTED_CONNECTOR_PRODUCTS: &[HostedConnectorProduct] = &[
    NOTION_PRODUCT,
    GOOGLE_DOCS_PRODUCT,
    GOOGLE_CALENDAR_PRODUCT,
    GMAIL_PRODUCT,
    SLACK_PRODUCT,
];

#[must_use]
pub fn hosted_connector_product(provider_kind: &str) -> Option<&'static HostedConnectorProduct> {
    HOSTED_CONNECTOR_PRODUCTS
        .iter()
        .find(|product| product.provider_kind == provider_kind)
}
