import FileProvider
@testable import LocalityFileProvider
import XCTest

final class LocalityFileProviderItemTests: XCTestCase {
  func testSyncAnchorRoundTripsItemIdentifiers() throws {
    let identifiers = Set([
      NSFileProviderItemIdentifier("draft-message"),
      NSFileProviderItemIdentifier("sent-message"),
    ])

    let anchor = try LocalitySyncAnchor.encode(identifiers: identifiers)

    XCTAssertEqual(LocalitySyncAnchor.decode(anchor), identifiers)
  }

  func testSyncAnchorIdentifiesDeletedItems() throws {
    let draft = NSFileProviderItemIdentifier("draft-message")
    let retained = NSFileProviderItemIdentifier("retained-message")
    let anchor = try LocalitySyncAnchor.encode(identifiers: Set([draft, retained]))
    let current = Set([retained, NSFileProviderItemIdentifier("new-sent-message")])

    let previous = try XCTUnwrap(LocalitySyncAnchor.decode(anchor))

    XCTAssertEqual(
      LocalitySyncAnchor.deletedIdentifiers(previous: previous, current: current),
      [draft]
    )
  }

  func testLegacyTimestampSyncAnchorExpires() {
    let legacyAnchor = NSFileProviderSyncAnchor(Data("1784775141.0".utf8))

    XCTAssertNil(LocalitySyncAnchor.decode(legacyAnchor))
  }

  func testSharedDomainPageChildFolderAllowsAddingSubitems() {
    let item = LocalityFileProviderItem(
      metadata: metadata(
        identifier: LocalitySharedDomain.itemIdentifier(
          mountId: "notion-main",
          daemonIdentifier: "children:page-1"
        ),
        filename: "Home",
        kind: "folder"
      )
    )

    XCTAssertTrue(item.capabilities.contains(.allowsContentEnumerating))
    XCTAssertTrue(item.capabilities.contains(.allowsAddingSubItems))
  }

  func testPendingPageFolderAllowsAddingSubitems() {
    let item = LocalityFileProviderItem(
      metadata: metadata(
        identifier: LocalitySharedDomain.itemIdentifier(
          mountId: "notion-main",
          daemonIdentifier: "children:local:1234"
        ),
        filename: "Draft",
        kind: "folder",
        entityKind: "page"
      )
    )

    XCTAssertTrue(item.capabilities.contains(.allowsAddingSubItems))
  }

  func testPageDocumentAllowsWritingAndRenaming() {
    let item = LocalityFileProviderItem(
      metadata: metadata(
        identifier: LocalitySharedDomain.itemIdentifier(
          mountId: "notion-main",
          daemonIdentifier: "page-1"
        ),
        filename: "page.md",
        kind: "file",
        entityKind: "page"
      )
    )

    XCTAssertTrue(item.capabilities.contains(.allowsWriting))
    XCTAssertTrue(item.capabilities.contains(.allowsRenaming))
  }

  func testWritableMountRootFolderAllowsAddingSubitems() {
    let item = LocalityFileProviderItem(
      metadata: metadata(
        identifier: LocalitySharedDomain.itemIdentifier(
          mountId: "google-docs-main",
          daemonIdentifier: "mount:google-docs-main"
        ),
        filename: "google-docs-main",
        kind: "folder"
      )
    )

    XCTAssertTrue(item.capabilities.contains(.allowsReading))
    XCTAssertTrue(item.capabilities.contains(.allowsContentEnumerating))
    XCTAssertTrue(item.capabilities.contains(.allowsAddingSubItems))
  }

  func testReadOnlyFolderDoesNotAllowAddingSubitems() {
    let item = LocalityFileProviderItem(
      metadata: metadata(
        identifier: LocalitySharedDomain.itemIdentifier(
          mountId: "gmail-main",
          daemonIdentifier: "gmail-folder:inbox"
        ),
        filename: "inbox",
        kind: "folder",
        readOnly: true
      )
    )

    XCTAssertTrue(item.capabilities.contains(.allowsReading))
    XCTAssertTrue(item.capabilities.contains(.allowsContentEnumerating))
    XCTAssertFalse(item.capabilities.contains(.allowsAddingSubItems))
  }

  func testReadOnlyPageDocumentDoesNotAllowWritingOrRenaming() {
    let item = LocalityFileProviderItem(
      metadata: metadata(
        identifier: LocalitySharedDomain.itemIdentifier(
          mountId: "gmail-main",
          daemonIdentifier: "msg-inbox-1"
        ),
        filename: "Inbox.md",
        kind: "file",
        entityKind: "page",
        readOnly: true
      )
    )

    XCTAssertTrue(item.capabilities.contains(.allowsReading))
    XCTAssertFalse(item.capabilities.contains(.allowsWriting))
    XCTAssertFalse(item.capabilities.contains(.allowsRenaming))
  }

  func testMetadataDecodingDefaultsMissingReadOnlyToFalse() throws {
    let json = Data(
      """
      {
        "identifier": "page-1",
        "parent_identifier": "root",
        "filename": "page.md",
        "kind": "file",
        "entity_kind": "page",
        "remote_id": "remote-page-1",
        "path": "page.md",
        "hydration": "clean",
        "content_type": "net.daringfireball.markdown",
        "remote_edited_at": "2026-07-14T10:00:00Z",
        "materialized_path": "/tmp/page.md",
        "byte_size": 42
      }
      """.utf8
    )

    let metadata = try JSONDecoder().decode(LocalityItemMetadata.self, from: json)

    XCTAssertEqual(metadata.filename, "page.md")
    XCTAssertFalse(metadata.readOnly)
  }

  private func metadata(
    identifier: String,
    filename: String,
    kind: String,
    entityKind: String? = nil,
    readOnly: Bool = false
  ) -> LocalityItemMetadata {
    LocalityItemMetadata(
      identifier: identifier,
      parentIdentifier: LocalityIdentifier.root,
      filename: filename,
      kind: kind,
      entityKind: entityKind,
      readOnly: readOnly,
      remoteId: nil,
      path: filename,
      hydration: nil,
      contentType: kind == "folder" ? "public.folder" : "net.daringfireball.markdown",
      remoteEditedAt: nil,
      materializedPath: nil,
      byteSize: nil
    )
  }
}
