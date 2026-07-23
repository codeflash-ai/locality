import FileProvider
@testable import LocalityFileProvider
import XCTest

final class LocalityFileProviderItemTests: XCTestCase {
  func testCurrentSyncAnchorIsRecognized() throws {
    let anchor = try LocalitySyncAnchor.next()

    XCTAssertTrue(LocalitySyncAnchor.isCurrent(anchor))
  }

  func testSuccessiveSyncAnchorsAdvance() throws {
    let first = try LocalitySyncAnchor.next()
    let second = try LocalitySyncAnchor.next()

    XCTAssertNotEqual(first, second)
  }

  func testLegacyTimestampSyncAnchorExpires() {
    let legacyAnchor = NSFileProviderSyncAnchor(Data("1784775141.0".utf8))

    XCTAssertFalse(LocalitySyncAnchor.isCurrent(legacyAnchor))
  }

  func testMissingReconciledLocalItemCanBeDeleted() {
    let error = LocalityDaemonClientError.daemonError(
      code: "invalid_state",
      message: "invalid state: virtual filesystem item `local:1` is not present in daemon state"
    )

    XCTAssertTrue(
      shouldAcceptAlreadyReconciledLocalDeletion(
        daemonIdentifier: "local:1",
        error: error
      )
    )
  }

  func testRemoteOrUnconfirmedItemDeletionRemainsBlocked() {
    let missing = LocalityDaemonClientError.daemonError(
      code: "invalid_state",
      message: "invalid state: virtual filesystem item `page-1` is not present in daemon state"
    )
    let unavailable = LocalityDaemonClientError.connectFailed("offline")

    XCTAssertFalse(
      shouldAcceptAlreadyReconciledLocalDeletion(
        daemonIdentifier: "page-1",
        error: missing
      )
    )
    XCTAssertFalse(
      shouldAcceptAlreadyReconciledLocalDeletion(
        daemonIdentifier: "local:1",
        error: unavailable
      )
    )
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
