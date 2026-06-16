@preconcurrency import Dispatch
@preconcurrency import FileProvider
@preconcurrency import Foundation

@objc(AgentFSFileProviderExtension)
final class AgentFSFileProviderExtension: NSObject, NSFileProviderReplicatedExtension,
  NSFileProviderEnumerating
{
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
    let progress = Progress(totalUnitCount: 1)
    completionHandler(nil, [], false, unsupportedWriteError())
    progress.completedUnitCount = 1
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
    do {
      client = try daemonClient()
    } catch {
      completionHandler(nil, [], false, error)
      progress.completedUnitCount = 2
      return progress
    }
    DispatchQueue.global(qos: .userInitiated).async {
      do {
        guard let newContents else {
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
        _ = try client.write(
          mountId: mountId,
          identifier: daemonIdentifier,
          contentsBase64: data.base64EncodedString()
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
        progressHandle.value.completedUnitCount = 2
      } catch {
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
      "AgentFS currently supports editing existing page files. Create, rename, and delete support will be added through the daemon write pipeline."
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
