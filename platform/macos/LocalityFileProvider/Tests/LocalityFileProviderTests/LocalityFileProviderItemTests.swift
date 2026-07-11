import FileProvider
@testable import LocalityFileProvider
import XCTest

final class LocalityFileProviderItemTests: XCTestCase {
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

  private func metadata(
    identifier: String,
    filename: String,
    kind: String,
    entityKind: String? = nil
  ) -> LocalityItemMetadata {
    LocalityItemMetadata(
      identifier: identifier,
      parentIdentifier: LocalityIdentifier.root,
      filename: filename,
      kind: kind,
      entityKind: entityKind,
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
