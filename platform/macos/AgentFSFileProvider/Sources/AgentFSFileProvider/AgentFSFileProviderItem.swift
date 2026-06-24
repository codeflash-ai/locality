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
    if metadata.kind == "folder" {
      self.capabilities = [.allowsReading, .allowsContentEnumerating, .allowsAddingSubItems]
    } else if metadata.entityKind == "page" {
      self.capabilities = [.allowsReading, .allowsWriting, .allowsRenaming]
    } else {
      self.capabilities = [.allowsReading]
    }
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

extension AgentFSItemMetadata {
  func namespaced(for mountId: String) -> AgentFSItemMetadata {
    AgentFSItemMetadata(
      identifier: AgentFSSharedDomain.itemIdentifier(
        mountId: mountId,
        daemonIdentifier: identifier
      ),
      parentIdentifier: AgentFSSharedDomain.parentIdentifier(
        mountId: mountId,
        daemonParentIdentifier: parentIdentifier
      ),
      filename: filename,
      kind: kind,
      entityKind: entityKind,
      remoteId: remoteId,
      path: path,
      hydration: hydration,
      contentType: contentType,
      remoteEditedAt: remoteEditedAt,
      materializedPath: materializedPath,
      byteSize: byteSize
    )
  }
}

enum AgentFSIdentifier {
  static let root = "root"
}

struct AgentFSResolvedIdentifier {
  let mountId: String
  let daemonIdentifier: String
}

enum AgentFSSharedDomain {
  static let identifier = "afs"
  private static let prefix = "m:"

  static func itemIdentifier(mountId: String, daemonIdentifier: String) -> String {
    "\(prefix)\(encode(mountId)):\(encode(daemonIdentifier))"
  }

  static func parentIdentifier(
    mountId: String,
    daemonParentIdentifier: String?
  ) -> String? {
    guard let daemonParentIdentifier else {
      return nil
    }
    if daemonParentIdentifier == AgentFSIdentifier.root {
      return AgentFSIdentifier.root
    }
    return itemIdentifier(mountId: mountId, daemonIdentifier: daemonParentIdentifier)
  }

  static func resolve(_ identifier: NSFileProviderItemIdentifier) -> AgentFSResolvedIdentifier? {
    let raw = identifier.rawValue
    guard raw.hasPrefix(prefix) else {
      return nil
    }
    let remainder = raw.dropFirst(prefix.count)
    let parts = remainder.split(separator: ":", maxSplits: 1, omittingEmptySubsequences: false)
    guard parts.count == 2,
      let mountId = decode(String(parts[0])),
      let daemonIdentifier = decode(String(parts[1]))
    else {
      return nil
    }
    return AgentFSResolvedIdentifier(mountId: mountId, daemonIdentifier: daemonIdentifier)
  }

  private static func encode(_ value: String) -> String {
    Data(value.utf8)
      .base64EncodedString()
      .replacingOccurrences(of: "+", with: "-")
      .replacingOccurrences(of: "/", with: "_")
      .replacingOccurrences(of: "=", with: "")
  }

  private static func decode(_ value: String) -> String? {
    var padded = value
      .replacingOccurrences(of: "-", with: "+")
      .replacingOccurrences(of: "_", with: "/")
    let remainder = padded.count % 4
    if remainder != 0 {
      padded.append(String(repeating: "=", count: 4 - remainder))
    }
    guard let data = Data(base64Encoded: padded) else {
      return nil
    }
    return String(data: data, encoding: .utf8)
  }
}
