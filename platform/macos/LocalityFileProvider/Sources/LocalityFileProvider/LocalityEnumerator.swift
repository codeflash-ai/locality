import FileProvider
import Foundation

final class LocalityEnumerator: NSObject, NSFileProviderEnumerator {
    private let client: LocalityDaemonClient?
    private let mountId: String?
    private let containerIdentifier: String?
    private let domainId: String?
    private let namespaceMountId: String?
    private let includeMountRootChildren: Bool

    init(
        client: LocalityDaemonClient,
        mountId: String,
        containerIdentifier: String,
        namespaceMountId: String? = nil
    ) {
        self.client = client
        self.mountId = mountId
        self.containerIdentifier = containerIdentifier
        self.domainId = nil
        self.namespaceMountId = namespaceMountId
        self.includeMountRootChildren = false
        super.init()
    }

    init(
        client: LocalityDaemonClient,
        domainId: String,
        includeMountRootChildren: Bool = false
    ) {
        self.client = client
        self.mountId = nil
        self.containerIdentifier = nil
        self.domainId = domainId
        self.namespaceMountId = nil
        self.includeMountRootChildren = includeMountRootChildren
        super.init()
    }

    init(empty: ()) {
        self.client = nil
        self.mountId = nil
        self.containerIdentifier = nil
        self.domainId = nil
        self.namespaceMountId = nil
        self.includeMountRootChildren = false
        super.init()
    }

    func invalidate() {}

    func enumerateItems(
        for observer: NSFileProviderEnumerationObserver,
        startingAt page: NSFileProviderPage
    ) {
        do {
            let items = try currentItems()
            observer.didEnumerate(items)
            observer.finishEnumerating(upTo: nil)
        } catch {
            observer.finishEnumeratingWithError(agentFSFileProviderError(error))
        }
    }

    func currentSyncAnchor(
        completionHandler: @escaping (NSFileProviderSyncAnchor?) -> Void
    ) {
        completionHandler(try? LocalitySyncAnchor.next())
    }

    func enumerateChanges(
        for observer: NSFileProviderChangeObserver,
        from syncAnchor: NSFileProviderSyncAnchor
    ) {
        guard LocalitySyncAnchor.isCurrent(syncAnchor) else {
            observer.finishEnumeratingWithError(
                NSError(
                    domain: NSFileProviderErrorDomain,
                    code: NSFileProviderError.syncAnchorExpired.rawValue
                )
            )
            return
        }

        do {
            let items = try currentItems()
            observer.didUpdate(items)
            observer.finishEnumeratingChanges(
                upTo: try LocalitySyncAnchor.next(),
                moreComing: false
            )
        } catch {
            observer.finishEnumeratingWithError(agentFSFileProviderError(error))
        }
    }

    private func currentItems() throws -> [LocalityFileProviderItem] {
        guard let client else {
            return []
        }

        if let domainId {
            let response = try client.domainChildren(domainId: domainId)
            var items = response.children.map { child in
                LocalityFileProviderItem(metadata: child.item.namespaced(for: child.mountId))
            }
            if includeMountRootChildren {
                for child in response.children {
                    let children = try client.children(
                        mountId: child.mountId,
                        containerIdentifier: child.item.identifier
                    )
                    items.append(contentsOf: children.children.map { metadata in
                        LocalityFileProviderItem(metadata: metadata.namespaced(for: child.mountId))
                    })
                }
            }
            return items
        }
        if let mountId, let containerIdentifier {
            let response = try client.children(
                mountId: mountId,
                containerIdentifier: containerIdentifier
            )
            return response.children.map { child in
                let metadata = namespaceMountId.map { child.namespaced(for: $0) } ?? child
                return LocalityFileProviderItem(metadata: metadata)
            }
        }
        return []
    }

}

enum LocalitySyncAnchor {
    private static let schemaVersion = 1

    private struct Snapshot: Codable {
        let schemaVersion: Int
        let nonce: UUID
    }

    static func next() throws -> NSFileProviderSyncAnchor {
        let snapshot = Snapshot(
            schemaVersion: schemaVersion,
            nonce: UUID()
        )
        return NSFileProviderSyncAnchor(try JSONEncoder().encode(snapshot))
    }

    static func isCurrent(_ syncAnchor: NSFileProviderSyncAnchor) -> Bool {
        guard
            let snapshot = try? JSONDecoder().decode(Snapshot.self, from: syncAnchor.rawValue),
            snapshot.schemaVersion == schemaVersion
        else {
            return false
        }
        return true
    }
}

func agentFSFileProviderError(_ error: Error) -> NSError {
    let nsError = error as NSError
    if nsError.domain == NSCocoaErrorDomain || nsError.domain == NSFileProviderErrorDomain {
        return nsError
    }
    return NSError(
        domain: NSFileProviderErrorDomain,
        code: NSFileProviderError.serverUnreachable.rawValue,
        userInfo: [NSLocalizedDescriptionKey: nsError.localizedDescription]
    )
}
