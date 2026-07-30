import FileProvider
@testable import LocalityFileProvider
import XCTest

final class LocalityFileProviderItemTests: XCTestCase {
  func testCurrentSyncAnchorIsRecognized() throws {
    let (store, directory) = makeSyncAnchorStore()
    defer { try? FileManager.default.removeItem(at: directory) }
    let anchor = try LocalitySyncAnchor.next(store: store)

    XCTAssertTrue(LocalitySyncAnchor.isCurrent(anchor))
  }

  func testSuccessiveSyncAnchorsAdvance() throws {
    let (store, directory) = makeSyncAnchorStore()
    defer { try? FileManager.default.removeItem(at: directory) }
    let first = try LocalitySyncAnchor.next(store: store)
    let second = try LocalitySyncAnchor.next(store: store)

    XCTAssertNotEqual(first, second)
  }

  func testSyncAnchorStaysWithinFileProviderSizeLimit() throws {
    let (store, directory) = makeSyncAnchorStore()
    defer { try? FileManager.default.removeItem(at: directory) }
    let items = (0..<1000).map { index in
      item(identifier: "item-\(index)", filename: "Item \(index)", kind: "file")
    }

    let anchor = try LocalitySyncAnchor.next(items: items, store: store)

    XCTAssertLessThanOrEqual(anchor.rawValue.count, 500)
  }

  func testLegacyTimestampSyncAnchorExpires() {
    let legacyAnchor = NSFileProviderSyncAnchor(Data("1784775141.0".utf8))

    XCTAssertFalse(LocalitySyncAnchor.isCurrent(legacyAnchor))
  }

  func testPreviousAndNewerSchemaSyncAnchorsExpire() {
    for schemaVersion in [1, 3] {
      let data = Data(
        """
        {"schemaVersion":\(schemaVersion),"nonce":"00000000-0000-0000-0000-000000000000","items":[]}
        """.utf8
      )

      XCTAssertFalse(LocalitySyncAnchor.isCurrent(NSFileProviderSyncAnchor(data)))
    }
  }

  func testSyncChangesOnlyReportsNewItems() throws {
    let (store, directory) = makeSyncAnchorStore()
    defer { try? FileManager.default.removeItem(at: directory) }
    let notion = item(identifier: "mount:notion-main", filename: "notion", kind: "folder")
    let anchor = try LocalitySyncAnchor.next(items: [notion], store: store)
    let unchangedNotion = item(
      identifier: "mount:notion-main",
      filename: "notion",
      kind: "folder"
    )
    let calendar = item(
      identifier: "mount:google-calendar-main",
      filename: "google-calendar-main",
      kind: "folder"
    )

    let changes = try XCTUnwrap(
      LocalitySyncAnchor.changes(
        since: anchor,
        currentItems: [unchangedNotion, calendar],
        store: store
      )
    )

    XCTAssertEqual(changes.updatedItems.map(\.itemIdentifier.rawValue), ["mount:google-calendar-main"])
    XCTAssertTrue(changes.deletedIdentifiers.isEmpty)
  }

  func testSyncChangesReportsChangedAndDeletedItems() throws {
    let (store, directory) = makeSyncAnchorStore()
    defer { try? FileManager.default.removeItem(at: directory) }
    let notion = item(identifier: "mount:notion-main", filename: "notion", kind: "folder")
    let removed = item(identifier: "mount:removed", filename: "removed", kind: "folder")
    let anchor = try LocalitySyncAnchor.next(items: [notion, removed], store: store)
    let renamedNotion = item(
      identifier: "mount:notion-main",
      filename: "notion-renamed",
      kind: "folder"
    )

    let changes = try XCTUnwrap(
      LocalitySyncAnchor.changes(
        since: anchor,
        currentItems: [renamedNotion],
        store: store
      )
    )

    XCTAssertEqual(changes.updatedItems.map(\.itemIdentifier.rawValue), ["mount:notion-main"])
    XCTAssertEqual(changes.deletedIdentifiers.map(\.rawValue), ["mount:removed"])
  }

  func testMissingSyncSnapshotExpiresAnchor() throws {
    let (store, directory) = makeSyncAnchorStore()
    let notion = item(identifier: "mount:notion-main", filename: "notion", kind: "folder")
    let anchor = try LocalitySyncAnchor.next(items: [notion], store: store)
    try FileManager.default.removeItem(at: directory)

    XCTAssertNil(
      LocalitySyncAnchor.changes(
        since: anchor,
        currentItems: [notion],
        store: store
      )
    )
  }

  func testWorkingSetUsesRecursiveCachedDomainItemsWithoutPerFolderRequests() throws {
    let client = RecordingEnumerationClient(
      workingSet: LocalityDomainChildrenPayload(
        domainId: "loc",
        children: [
          LocalityDomainChild(
            mountId: "notion-main",
            item: metadata(
              identifier: "mount:notion-main",
              filename: "notion",
              kind: "folder"
            )
          ),
          LocalityDomainChild(
            mountId: "notion-main",
            item: metadata(
              identifier: "children:company",
              parentIdentifier: "mount:notion-main",
              filename: "Company",
              kind: "folder"
            )
          ),
          LocalityDomainChild(
            mountId: "notion-main",
            item: metadata(
              identifier: "children:compliance",
              parentIdentifier: "children:company",
              filename: "Compliance",
              kind: "folder"
            )
          ),
          LocalityDomainChild(
            mountId: "notion-main",
            item: metadata(
              identifier: "compliance",
              parentIdentifier: "children:compliance",
              filename: "page.md",
              kind: "file"
            )
          ),
        ]
      )
    )
    let enumerator = LocalityEnumerator(
      client: client,
      domainId: "loc",
      includeDomainWorkingSet: true
    )

    let items = try enumerator.currentItems()

    XCTAssertEqual(items.map(\.filename), ["notion", "Company", "Compliance", "page.md"])
    XCTAssertEqual(client.workingSetRequests, ["loc"])
    XCTAssertTrue(client.domainChildrenRequests.isEmpty)
    XCTAssertTrue(client.childRequests.isEmpty)
    XCTAssertEqual(items[1].parentItemIdentifier, items[0].itemIdentifier)
    XCTAssertEqual(items[2].parentItemIdentifier, items[1].itemIdentifier)
    XCTAssertEqual(items[3].parentItemIdentifier, items[2].itemIdentifier)
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
    parentIdentifier: String = LocalityIdentifier.root,
    filename: String,
    kind: String,
    entityKind: String? = nil,
    readOnly: Bool = false
  ) -> LocalityItemMetadata {
    LocalityItemMetadata(
      identifier: identifier,
      parentIdentifier: parentIdentifier,
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

  private func item(identifier: String, filename: String, kind: String) -> LocalityFileProviderItem {
    LocalityFileProviderItem(
      metadata: metadata(identifier: identifier, filename: filename, kind: kind)
    )
  }

  private func makeSyncAnchorStore() -> (LocalitySyncAnchorStore, URL) {
    let directory = FileManager.default.temporaryDirectory
      .appendingPathComponent("LocalitySyncAnchorTests-\(UUID().uuidString)", isDirectory: true)
    return (LocalitySyncAnchorStore(directory: directory), directory)
  }
}

private final class RecordingEnumerationClient: LocalityEnumerationClient {
  private let workingSet: LocalityDomainChildrenPayload
  private(set) var childRequests: [(String, String)] = []
  private(set) var domainChildrenRequests: [String] = []
  private(set) var workingSetRequests: [String] = []

  init(workingSet: LocalityDomainChildrenPayload) {
    self.workingSet = workingSet
  }

  func children(mountId: String, containerIdentifier: String) throws -> LocalityChildrenPayload {
    childRequests.append((mountId, containerIdentifier))
    return LocalityChildrenPayload(
      mountId: mountId,
      containerIdentifier: containerIdentifier,
      children: []
    )
  }

  func domainChildren(domainId: String) throws -> LocalityDomainChildrenPayload {
    domainChildrenRequests.append(domainId)
    return LocalityDomainChildrenPayload(domainId: domainId, children: [])
  }

  func domainWorkingSet(domainId: String) throws -> LocalityDomainChildrenPayload {
    workingSetRequests.append(domainId)
    return workingSet
  }
}
