@preconcurrency import Dispatch
@preconcurrency import FileProvider
@preconcurrency import Foundation
import OSLog
@preconcurrency import UniformTypeIdentifiers

@objc(LocalityFileProviderExtension)
final class LocalityFileProviderExtension: NSObject, NSFileProviderReplicatedExtension,
  NSFileProviderEnumerating
{
  private static let writeLog = Logger(
    subsystem: "ai.codeflash.locality.Locality.FileProvider",
    category: "writes"
  )

  private let domain: NSFileProviderDomain
  private let client: Result<LocalityDaemonClient, Error>

  required init(domain: NSFileProviderDomain) {
    self.domain = domain
    self.client = Result {
      try LocalityDaemonClient()
    }
    super.init()
  }

  func invalidate() {}

  func item(
    for identifier: NSFileProviderItemIdentifier,
    request: NSFileProviderRequest,
    completionHandler: @escaping (NSFileProviderItem?, Error?) -> Void
  ) -> Progress {
    let progress = Progress(totalUnitCount: 1)
    let completion = UncheckedSendable(completionHandler)
    let progressHandle = UncheckedSendable(progress)
    let client: LocalityDaemonClient
    let resolved: LocalityResolvedIdentifier
    let sharedDomain = isSharedDomain
    do {
      client = try daemonClient()
      if sharedDomain && identifier == .rootContainer {
        completionHandler(LocalityFileProviderItem(metadata: sharedRootItem()), nil)
        progress.completedUnitCount = 1
        return progress
      }
      resolved = try resolveIdentifier(identifier)
    } catch {
      completionHandler(nil, error)
      progress.completedUnitCount = 1
      return progress
    }
    DispatchQueue.global(qos: .userInitiated).async {
      do {
        let response = try client.item(
          mountId: resolved.mountId,
          identifier: resolved.daemonIdentifier
        )
        completion.value(LocalityFileProviderItem(metadata: providerMetadata(response.item, mountId: response.mountId, sharedDomain: sharedDomain)), nil)
        progressHandle.value.completedUnitCount = 1
      } catch {
        completion.value(nil, error)
      }
    }
    return progress
  }

  func enumerator(
    for containerItemIdentifier: NSFileProviderItemIdentifier,
    request: NSFileProviderRequest
  ) throws -> NSFileProviderEnumerator {
    if containerItemIdentifier == .trashContainer {
      return LocalityEnumerator(empty: ())
    }
    let client = try daemonClient()
    if containerItemIdentifier == .workingSet {
      return LocalityEnumerator(
        client: client,
        domainId: domain.identifier.rawValue,
        includeMountRootChildren: true
      )
    }
    if isSharedDomain && containerItemIdentifier == .rootContainer {
      return LocalityEnumerator(client: client, domainId: domain.identifier.rawValue)
    }
    let resolved = try resolveIdentifier(containerItemIdentifier)
    return LocalityEnumerator(
      client: client,
      mountId: resolved.mountId,
      containerIdentifier: resolved.daemonIdentifier,
      namespaceMountId: isSharedDomain ? resolved.mountId : nil
    )
  }

  func fetchContents(
    for itemIdentifier: NSFileProviderItemIdentifier,
    version requestedVersion: NSFileProviderItemVersion?,
    request: NSFileProviderRequest,
    completionHandler: @escaping (URL?, NSFileProviderItem?, Error?) -> Void
  ) -> Progress {
    let progress = Progress(totalUnitCount: 1)
    let completion = UncheckedSendable(completionHandler)
    let progressHandle = UncheckedSendable(progress)
    let client: LocalityDaemonClient
    let resolved: LocalityResolvedIdentifier
    let sharedDomain = isSharedDomain
    let transferDirectory: URL
    do {
      client = try daemonClient()
      resolved = try resolveIdentifier(itemIdentifier)
      transferDirectory = try temporaryDirectoryURL()
    } catch {
      completionHandler(nil, nil, error)
      progress.completedUnitCount = 1
      return progress
    }
    DispatchQueue.global(qos: .userInitiated).async {
      do {
        let read = try client.read(
          mountId: resolved.mountId,
          identifier: resolved.daemonIdentifier
        )
        let transferURL = try writeToFileProviderTransferDirectory(
          contentsBase64: read.contentsBase64,
          filename: read.item.filename,
          directory: transferDirectory
        )
        completion.value(
          transferURL,
          LocalityFileProviderItem(metadata: providerMetadata(read.item, mountId: read.mountId, sharedDomain: sharedDomain)),
          nil
        )
        progressHandle.value.completedUnitCount = 1
      } catch {
        completion.value(nil, nil, agentFSFileProviderError(error))
      }
    }
    return progress
  }

  func createItem(
    basedOn itemTemplate: NSFileProviderItem,
    fields: NSFileProviderItemFields,
    contents url: URL?,
    options: NSFileProviderCreateItemOptions = [],
    request: NSFileProviderRequest,
    completionHandler:
      @escaping (NSFileProviderItem?, NSFileProviderItemFields, Bool, Error?) -> Void
  ) -> Progress {
    let progress = Progress(totalUnitCount: 2)
    let completion = UncheckedSendable(completionHandler)
    let progressHandle = UncheckedSendable(progress)
    let filename = itemTemplate.filename
    let isDirectory = itemTemplate.contentType?.conforms(to: .folder) ?? false
    let client: LocalityDaemonClient
    let parent: LocalityResolvedIdentifier
    let sharedDomain = isSharedDomain
    do {
      client = try daemonClient()
      parent = try resolveIdentifier(itemTemplate.parentItemIdentifier)
    } catch {
      completionHandler(nil, [], false, error)
      progress.completedUnitCount = 2
      return progress
    }
    DispatchQueue.global(qos: .userInitiated).async {
      do {
        let created: LocalityMutationPayload
        if isDirectory {
          created = try client.createDirectory(
            mountId: parent.mountId,
            parentIdentifier: parent.daemonIdentifier,
            dirname: filename
          )
        } else {
          created = try client.createFile(
            mountId: parent.mountId,
            parentIdentifier: parent.daemonIdentifier,
            filename: filename
          )
          if let url {
            let data = try Data(contentsOf: url)
            _ = try client.write(
              mountId: parent.mountId,
              identifier: created.identifier,
              contentsBase64: data.base64EncodedString()
            )
          }
        }
        progressHandle.value.completedUnitCount = 1
        completion.value(
          LocalityFileProviderItem(metadata: providerMetadata(created.item, mountId: created.mountId, sharedDomain: sharedDomain)),
          [],
          false,
          nil
        )
        progressHandle.value.completedUnitCount = 2
      } catch {
        completion.value(nil, fields, false, agentFSFileProviderError(error))
        progressHandle.value.completedUnitCount = 2
      }
    }
    return progress
  }

  func modifyItem(
    _ item: NSFileProviderItem,
    baseVersion version: NSFileProviderItemVersion,
    changedFields: NSFileProviderItemFields,
    contents newContents: URL?,
    options: NSFileProviderModifyItemOptions = [],
    request: NSFileProviderRequest,
    completionHandler:
      @escaping (NSFileProviderItem?, NSFileProviderItemFields, Bool, Error?) -> Void
  ) -> Progress {
    let progress = Progress(totalUnitCount: 2)
    let completion = UncheckedSendable(completionHandler)
    let progressHandle = UncheckedSendable(progress)
    let client: LocalityDaemonClient
    let resolved: LocalityResolvedIdentifier
    let newParent: LocalityResolvedIdentifier?
    let sharedDomain = isSharedDomain
    Self.writeLog.info(
      "modifyItem started id=\(item.itemIdentifier.rawValue, privacy: .public) filename=\(item.filename, privacy: .public) changedFields=\(changedFields.rawValue, privacy: .public) hasContents=\((newContents != nil), privacy: .public)"
    )
    do {
      client = try daemonClient()
      resolved = try resolveIdentifier(item.itemIdentifier)
      if changedFields.contains(.filename) || changedFields.contains(.parentItemIdentifier) {
        newParent = try resolveIdentifier(item.parentItemIdentifier)
      } else {
        newParent = nil
      }
    } catch {
      Self.writeLog.error(
        "modifyItem daemon client failed id=\(item.itemIdentifier.rawValue, privacy: .public) error=\(String(describing: error), privacy: .public)"
      )
      completionHandler(nil, [], false, error)
      progress.completedUnitCount = 2
      return progress
    }
    DispatchQueue.global(qos: .userInitiated).async {
      do {
        if changedFields.contains(.filename) || changedFields.contains(.parentItemIdentifier) {
          Self.writeLog.info(
            "modifyItem rename started mount=\(resolved.mountId, privacy: .public) id=\(resolved.daemonIdentifier, privacy: .public) filename=\(item.filename, privacy: .public)"
          )
          guard let newParent else {
            throw agentFSUnsupportedWriteError(
              "Could not resolve the target folder for this rename."
            )
          }
          let renamed = try client.rename(
            mountId: resolved.mountId,
            identifier: resolved.daemonIdentifier,
            newParentIdentifier: newParent.daemonIdentifier,
            newFilename: item.filename
          )
          completion.value(
            LocalityFileProviderItem(metadata: providerMetadata(renamed.item, mountId: renamed.mountId, sharedDomain: sharedDomain)),
            [],
            false,
            nil
          )
          Self.writeLog.info(
            "modifyItem rename succeeded mount=\(resolved.mountId, privacy: .public) id=\(resolved.daemonIdentifier, privacy: .public)"
          )
          progressHandle.value.completedUnitCount = 2
          return
        }

        guard let newContents else {
          Self.writeLog.info(
            "modifyItem content missing mount=\(resolved.mountId, privacy: .public) id=\(resolved.daemonIdentifier, privacy: .public) changedFields=\(changedFields.rawValue, privacy: .public)"
          )
          let refreshed = try client.item(
            mountId: resolved.mountId,
            identifier: resolved.daemonIdentifier
          )
          completion.value(
            LocalityFileProviderItem(metadata: providerMetadata(refreshed.item, mountId: refreshed.mountId, sharedDomain: sharedDomain)),
            [],
            false,
            nil
          )
          progressHandle.value.completedUnitCount = 2
          return
        }

        let data = try Data(contentsOf: newContents)
        Self.writeLog.info(
          "modifyItem content read mount=\(resolved.mountId, privacy: .public) id=\(resolved.daemonIdentifier, privacy: .public) bytes=\(data.count, privacy: .public)"
        )
        _ = try client.write(
          mountId: resolved.mountId,
          identifier: resolved.daemonIdentifier,
          contentsBase64: data.base64EncodedString()
        )
        Self.writeLog.info(
          "modifyItem daemon write succeeded mount=\(resolved.mountId, privacy: .public) id=\(resolved.daemonIdentifier, privacy: .public) bytes=\(data.count, privacy: .public)"
        )
        progressHandle.value.completedUnitCount = 1
        let refreshed = try client.item(
          mountId: resolved.mountId,
          identifier: resolved.daemonIdentifier
        )
        completion.value(
          LocalityFileProviderItem(metadata: providerMetadata(refreshed.item, mountId: refreshed.mountId, sharedDomain: sharedDomain)),
          [],
          false,
          nil
        )
        Self.writeLog.info(
          "modifyItem completed mount=\(resolved.mountId, privacy: .public) id=\(resolved.daemonIdentifier, privacy: .public)"
        )
        progressHandle.value.completedUnitCount = 2
      } catch {
        Self.writeLog.error(
          "modifyItem failed mount=\(resolved.mountId, privacy: .public) id=\(resolved.daemonIdentifier, privacy: .public) error=\(String(describing: error), privacy: .public)"
        )
        completion.value(nil, changedFields, false, agentFSFileProviderError(error))
        progressHandle.value.completedUnitCount = 2
      }
    }
    return progress
  }

  func deleteItem(
    identifier: NSFileProviderItemIdentifier,
    baseVersion version: NSFileProviderItemVersion,
    options: NSFileProviderDeleteItemOptions = [],
    request: NSFileProviderRequest,
    completionHandler: @escaping (Error?) -> Void
  ) -> Progress {
    let progress = Progress(totalUnitCount: 1)
    completionHandler(unsupportedWriteError())
    progress.completedUnitCount = 1
    return progress
  }

  private var mountId: String {
    domain.identifier.rawValue
  }

  private var isSharedDomain: Bool {
    domain.identifier.rawValue == LocalitySharedDomain.identifier
  }

  private func resolveIdentifier(_ identifier: NSFileProviderItemIdentifier) throws
    -> LocalityResolvedIdentifier
  {
    if isSharedDomain {
      guard let resolved = LocalitySharedDomain.resolve(identifier) else {
        throw unsupportedWriteError(
          "Create and edit files inside a connected source folder, such as Notion."
        )
      }
      return resolved
    }
    return LocalityResolvedIdentifier(
      mountId: mountId,
      daemonIdentifier: LocalityFileProviderItem.daemonIdentifier(identifier)
    )
  }

  private func sharedRootItem() -> LocalityItemMetadata {
    LocalityItemMetadata(
      identifier: LocalityIdentifier.root,
      parentIdentifier: nil,
      filename: domain.displayName.isEmpty ? "Locality" : domain.displayName,
      kind: "folder",
      entityKind: nil,
      remoteId: nil,
      path: "",
      hydration: nil,
      contentType: "public.folder",
      remoteEditedAt: nil,
      materializedPath: nil,
      byteSize: nil
    )
  }

  private func daemonClient() throws -> LocalityDaemonClient {
    try client.get()
  }

  private func temporaryDirectoryURL() throws -> URL {
    guard let manager = NSFileProviderManager(for: domain) else {
      throw NSError(
        domain: NSCocoaErrorDomain,
        code: NSFileNoSuchFileError,
        userInfo: [
          NSLocalizedDescriptionKey: "No File Provider manager is available for domain \(mountId)."
        ]
      )
    }
    return try manager.temporaryDirectoryURL()
  }

  private func unsupportedWriteError() -> NSError {
    unsupportedWriteError(
      "Locality currently supports editing existing page.md files. Create, rename, and delete support will be added through the daemon write pipeline."
    )
  }

  private func unsupportedWriteError(_ message: String) -> NSError {
    agentFSUnsupportedWriteError(message)
  }
}

private func agentFSUnsupportedWriteError(_ message: String) -> NSError {
  NSError(
    domain: NSCocoaErrorDomain,
    code: NSFeatureUnsupportedError,
    userInfo: [
      NSLocalizedDescriptionKey: message
    ]
  )
}

private func writeToFileProviderTransferDirectory(
  contentsBase64: String,
  filename: String,
  directory: URL
) throws -> URL {
  var transferURL = directory.appendingPathComponent(UUID().uuidString, isDirectory: false)
  let pathExtension = (filename as NSString).pathExtension
  if !pathExtension.isEmpty {
    transferURL.appendPathExtension(pathExtension)
  }
  guard let contents = Data(base64Encoded: contentsBase64) else {
    throw NSError(
      domain: NSCocoaErrorDomain,
      code: NSFileReadCorruptFileError,
      userInfo: [
        NSLocalizedDescriptionKey: "Locality daemon returned invalid base64 file contents."
      ]
    )
  }
  try? FileManager.default.removeItem(at: transferURL)
  try contents.write(to: transferURL, options: .atomic)
  return transferURL
}

private func providerMetadata(
  _ metadata: LocalityItemMetadata,
  mountId: String,
  sharedDomain: Bool
) -> LocalityItemMetadata {
  sharedDomain ? metadata.namespaced(for: mountId) : metadata
}

private struct UncheckedSendable<Value>: @unchecked Sendable {
  let value: Value

  init(_ value: Value) {
    self.value = value
  }
}
