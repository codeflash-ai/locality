@preconcurrency import Dispatch
@preconcurrency import FileProvider
@preconcurrency import Foundation
import OSLog
@preconcurrency import UniformTypeIdentifiers

@objc(AgentFSFileProviderExtension)
final class AgentFSFileProviderExtension: NSObject, NSFileProviderReplicatedExtension,
  NSFileProviderEnumerating
{
  private static let writeLog = Logger(
    subsystem: "ai.codeflash.afs.AgentFS.FileProvider",
    category: "writes"
  )

  private let domain: NSFileProviderDomain
  private let client: Result<AgentFSDaemonClient, Error>

  required init(domain: NSFileProviderDomain) {
    self.domain = domain
    self.client = Result {
      try AgentFSDaemonClient()
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
    let mountId = self.mountId
    let daemonIdentifier = AgentFSFileProviderItem.daemonIdentifier(identifier)
    let client: AgentFSDaemonClient
    do {
      client = try daemonClient()
    } catch {
      completionHandler(nil, error)
      progress.completedUnitCount = 1
      return progress
    }
    DispatchQueue.global(qos: .userInitiated).async {
      do {
        let response = try client.item(
          mountId: mountId,
          identifier: daemonIdentifier
        )
        completion.value(AgentFSFileProviderItem(metadata: response.item), nil)
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
    if containerItemIdentifier == .workingSet || containerItemIdentifier == .trashContainer {
      return AgentFSEnumerator(empty: ())
    }
    let client = try daemonClient()
    let daemonIdentifier = AgentFSFileProviderItem.daemonIdentifier(containerItemIdentifier)
    return AgentFSEnumerator(
      client: client,
      mountId: mountId,
      containerIdentifier: daemonIdentifier
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
    let mountId = self.mountId
    let daemonIdentifier = AgentFSFileProviderItem.daemonIdentifier(itemIdentifier)
    let client: AgentFSDaemonClient
    let transferDirectory: URL
    do {
      client = try daemonClient()
      transferDirectory = try temporaryDirectoryURL()
    } catch {
      completionHandler(nil, nil, error)
      progress.completedUnitCount = 1
      return progress
    }
    DispatchQueue.global(qos: .userInitiated).async {
      do {
        let read = try client.read(
          mountId: mountId,
          identifier: daemonIdentifier
        )
        let transferURL = try writeToFileProviderTransferDirectory(
          contentsBase64: read.contentsBase64,
          filename: read.item.filename,
          directory: transferDirectory
        )
        completion.value(
          transferURL,
          AgentFSFileProviderItem(metadata: read.item),
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
    let mountId = self.mountId
    let parentIdentifier = AgentFSFileProviderItem.daemonIdentifier(itemTemplate.parentItemIdentifier)
    let filename = itemTemplate.filename
    let isDirectory = itemTemplate.contentType?.conforms(to: .folder) ?? false
    let client: AgentFSDaemonClient
    do {
      client = try daemonClient()
    } catch {
      completionHandler(nil, [], false, error)
      progress.completedUnitCount = 2
      return progress
    }
    DispatchQueue.global(qos: .userInitiated).async {
      do {
        let created: AgentFSMutationPayload
        if isDirectory {
          created = try client.createDirectory(
            mountId: mountId,
            parentIdentifier: parentIdentifier,
            dirname: filename
          )
        } else {
          created = try client.createFile(
            mountId: mountId,
            parentIdentifier: parentIdentifier,
            filename: filename
          )
          if let url {
            let data = try Data(contentsOf: url)
            _ = try client.write(
              mountId: mountId,
              identifier: created.identifier,
              contentsBase64: data.base64EncodedString()
            )
          }
        }
        progressHandle.value.completedUnitCount = 1
        completion.value(
          AgentFSFileProviderItem(metadata: created.item),
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
    let mountId = self.mountId
    let daemonIdentifier = AgentFSFileProviderItem.daemonIdentifier(item.itemIdentifier)
    let client: AgentFSDaemonClient
    Self.writeLog.info(
      "modifyItem started mount=\(mountId, privacy: .public) id=\(daemonIdentifier, privacy: .public) filename=\(item.filename, privacy: .public) changedFields=\(changedFields.rawValue, privacy: .public) hasContents=\((newContents != nil), privacy: .public)"
    )
    do {
      client = try daemonClient()
    } catch {
      Self.writeLog.error(
        "modifyItem daemon client failed mount=\(mountId, privacy: .public) id=\(daemonIdentifier, privacy: .public) error=\(String(describing: error), privacy: .public)"
      )
      completionHandler(nil, [], false, error)
      progress.completedUnitCount = 2
      return progress
    }
    DispatchQueue.global(qos: .userInitiated).async {
      do {
        if changedFields.contains(.filename) || changedFields.contains(.parentItemIdentifier) {
          Self.writeLog.info(
            "modifyItem rename started mount=\(mountId, privacy: .public) id=\(daemonIdentifier, privacy: .public) filename=\(item.filename, privacy: .public)"
          )
          let renamed = try client.rename(
            mountId: mountId,
            identifier: daemonIdentifier,
            newParentIdentifier: AgentFSFileProviderItem.daemonIdentifier(item.parentItemIdentifier),
            newFilename: item.filename
          )
          completion.value(
            AgentFSFileProviderItem(metadata: renamed.item),
            [],
            false,
            nil
          )
          Self.writeLog.info(
            "modifyItem rename succeeded mount=\(mountId, privacy: .public) id=\(daemonIdentifier, privacy: .public)"
          )
          progressHandle.value.completedUnitCount = 2
          return
        }

        guard let newContents else {
          Self.writeLog.info(
            "modifyItem content missing mount=\(mountId, privacy: .public) id=\(daemonIdentifier, privacy: .public) changedFields=\(changedFields.rawValue, privacy: .public)"
          )
          let refreshed = try client.item(
            mountId: mountId,
            identifier: daemonIdentifier
          )
          completion.value(
            AgentFSFileProviderItem(metadata: refreshed.item),
            [],
            false,
            nil
          )
          progressHandle.value.completedUnitCount = 2
          return
        }

        let data = try Data(contentsOf: newContents)
        Self.writeLog.info(
          "modifyItem content read mount=\(mountId, privacy: .public) id=\(daemonIdentifier, privacy: .public) bytes=\(data.count, privacy: .public)"
        )
        _ = try client.write(
          mountId: mountId,
          identifier: daemonIdentifier,
          contentsBase64: data.base64EncodedString()
        )
        Self.writeLog.info(
          "modifyItem daemon write succeeded mount=\(mountId, privacy: .public) id=\(daemonIdentifier, privacy: .public) bytes=\(data.count, privacy: .public)"
        )
        progressHandle.value.completedUnitCount = 1
        let refreshed = try client.item(
          mountId: mountId,
          identifier: daemonIdentifier
        )
        completion.value(
          AgentFSFileProviderItem(metadata: refreshed.item),
          [],
          false,
          nil
        )
        Self.writeLog.info(
          "modifyItem completed mount=\(mountId, privacy: .public) id=\(daemonIdentifier, privacy: .public)"
        )
        progressHandle.value.completedUnitCount = 2
      } catch {
        Self.writeLog.error(
          "modifyItem failed mount=\(mountId, privacy: .public) id=\(daemonIdentifier, privacy: .public) error=\(String(describing: error), privacy: .public)"
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

  private func daemonClient() throws -> AgentFSDaemonClient {
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
      "AgentFS currently supports editing existing page.md files. Create, rename, and delete support will be added through the daemon write pipeline."
    )
  }

  private func unsupportedWriteError(_ message: String) -> NSError {
    NSError(
      domain: NSCocoaErrorDomain,
      code: NSFeatureUnsupportedError,
      userInfo: [
        NSLocalizedDescriptionKey: message
      ]
    )
  }
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
        NSLocalizedDescriptionKey: "AgentFS daemon returned invalid base64 file contents."
      ]
    )
  }
  try? FileManager.default.removeItem(at: transferURL)
  try contents.write(to: transferURL, options: .atomic)
  return transferURL
}

private struct UncheckedSendable<Value>: @unchecked Sendable {
  let value: Value

  init(_ value: Value) {
    self.value = value
  }
}
