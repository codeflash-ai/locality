import FileProvider
@testable import LocalityFileProvider
import Testing

@Test func sharedDomainPageChildFolderAllowsAddingSubitems() {
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

  #expect(item.capabilities.contains(.allowsContentEnumerating))
  #expect(item.capabilities.contains(.allowsAddingSubItems))
}

@Test func pendingPageFolderAllowsAddingSubitems() {
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

  #expect(item.capabilities.contains(.allowsAddingSubItems))
}

@Test func pageDocumentAllowsWritingAndRenaming() {
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

  #expect(item.capabilities.contains(.allowsWriting))
  #expect(item.capabilities.contains(.allowsRenaming))
}

@Test func writableMountRootFolderAllowsAddingSubitems() {
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

  #expect(item.capabilities.contains(.allowsReading))
  #expect(item.capabilities.contains(.allowsContentEnumerating))
  #expect(item.capabilities.contains(.allowsAddingSubItems))
}

@Test func readOnlyFolderDoesNotAllowAddingSubitems() {
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

  #expect(item.capabilities.contains(.allowsReading))
  #expect(item.capabilities.contains(.allowsContentEnumerating))
  #expect(!item.capabilities.contains(.allowsAddingSubItems))
}

@Test func readOnlyPageDocumentDoesNotAllowWritingOrRenaming() {
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

  #expect(item.capabilities.contains(.allowsReading))
  #expect(!item.capabilities.contains(.allowsWriting))
  #expect(!item.capabilities.contains(.allowsRenaming))
}

@Test func metadataDecodingDefaultsMissingReadOnlyToFalse() throws {
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

  #expect(metadata.filename == "page.md")
  #expect(!metadata.readOnly)
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
