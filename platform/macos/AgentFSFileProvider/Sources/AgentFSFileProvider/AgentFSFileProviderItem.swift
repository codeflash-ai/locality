import FileProvider
import Foundation
import UniformTypeIdentifiers

final class AgentFSFileProviderItem: NSObject, NSFileProviderItem {
  private static let metadataSchemaVersion = "metadata-v2"

  let itemIdentifier: NSFileProviderItemIdentifier
  let parentItemIdentifier: NSFileProviderItemIdentifier
  let filename: String
  let contentType: UTType
  let capabilities: NSFileProviderItemCapabilities
  let documentSize: NSNumber?
  let childItemCount: NSNumber?
  let itemVersion: NSFileProviderItemVersion

  init(metadata: AgentFSItemMetadata) {
    self.itemIdentifier = Self.appleIdentifier(metadata.identifier)
    self.parentItemIdentifier = Self.appleIdentifier(
      metadata.parentIdentifier ?? AgentFSIdentifier.root
    )
    self.filename = metadata.filename
    self.contentType = UTType(metadata.contentType) ?? .data
    self.capabilities =
      metadata.kind == "folder"
      ? [.allowsReading, .allowsContentEnumerating]
      : [.allowsReading]
    self.documentSize =
      metadata.kind == "folder"
      ? nil
      : NSNumber(value: metadata.byteSize ?? 1)
    self.childItemCount = nil
    self.itemVersion = NSFileProviderItemVersion(
      contentVersion: Self.versionComponent([
        "content",
        metadata.identifier,
        metadata.remoteEditedAt ?? "",
        metadata.hydration ?? "",
        metadata.byteSize.map(String.init) ?? "",
      ]),
      metadataVersion: Self.versionComponent([
        Self.metadataSchemaVersion,
        metadata.identifier,
        metadata.parentIdentifier ?? "",
        metadata.filename,
        metadata.kind,
      ])
    )
    super.init()
  }

  static func daemonIdentifier(_ identifier: NSFileProviderItemIdentifier) -> String {
    if identifier == .rootContainer {
      return AgentFSIdentifier.root
    }
    return identifier.rawValue
  }

  static func appleIdentifier(_ identifier: String) -> NSFileProviderItemIdentifier {
    if identifier == AgentFSIdentifier.root {
      return .rootContainer
    }
    return NSFileProviderItemIdentifier(identifier)
  }

  private static func versionComponent(_ parts: [String]) -> Data {
    let bytes = parts.joined(separator: "|").utf8.prefix(128)
    return Data(bytes)
  }
}

enum AgentFSIdentifier {
  static let root = "root"
}
