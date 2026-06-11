@preconcurrency import FileProvider
@preconcurrency import Dispatch
@preconcurrency import Foundation

@objc(AgentFSFileProviderExtension)
final class AgentFSFileProviderExtension: NSObject, NSFileProviderReplicatedExtension {
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
                let materialized = try client.materialize(
                    mountId: mountId,
                    identifier: daemonIdentifier
                )
                let item = try client.item(
                    mountId: mountId,
                    identifier: daemonIdentifier
                )
                let transferURL = try copyToFileProviderTransferDirectory(
                    materializedPath: materialized.path,
                    filename: item.item.filename,
                    directory: transferDirectory
                )
                completion.value(
                    transferURL,
                    AgentFSFileProviderItem(metadata: item.item),
                    nil
                )
                progressHandle.value.completedUnitCount = 1
            } catch {
                completion.value(nil, nil, error)
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
        completionHandler: @escaping (NSFileProviderItem?, NSFileProviderItemFields, Bool, Error?) -> Void
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
        completionHandler: @escaping (NSFileProviderItem?, NSFileProviderItemFields, Bool, Error?) -> Void
    ) -> Progress {
        let progress = Progress(totalUnitCount: 1)
        completionHandler(nil, [], false, unsupportedWriteError())
        progress.completedUnitCount = 1
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
                    NSLocalizedDescriptionKey: "No File Provider manager is available for domain \(mountId).",
                ]
            )
        }
        return try manager.temporaryDirectoryURL()
    }

    private func unsupportedWriteError() -> NSError {
        NSError(
            domain: NSCocoaErrorDomain,
            code: NSFeatureUnsupportedError,
            userInfo: [
                NSLocalizedDescriptionKey: "AgentFS File Provider writes are routed through the daemon push pipeline in a later slice.",
            ]
        )
    }
}

private func copyToFileProviderTransferDirectory(
    materializedPath: String,
    filename: String,
    directory: URL
) throws -> URL {
    let sourceURL = URL(fileURLWithPath: materializedPath)
    var transferURL = directory.appendingPathComponent(UUID().uuidString, isDirectory: false)
    let pathExtension = (filename as NSString).pathExtension
    if !pathExtension.isEmpty {
        transferURL.appendPathExtension(pathExtension)
    }
    try? FileManager.default.removeItem(at: transferURL)
    try FileManager.default.copyItem(at: sourceURL, to: transferURL)
    return transferURL
}

private struct UncheckedSendable<Value>: @unchecked Sendable {
    let value: Value

    init(_ value: Value) {
        self.value = value
    }
}
