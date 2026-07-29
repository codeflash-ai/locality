import FileProvider
import Foundation

final class LocalityEnumerator: NSObject, NSFileProviderEnumerator {
    private let client: LocalityDaemonClient?
    private let mountId: String?
    private let containerIdentifier: String?
    private let domainId: String?
    private let namespaceMountId: String?
    private let includeMountRootChildren: Bool
    private let syncAnchorStore: LocalitySyncAnchorStore

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
        self.syncAnchorStore = .shared
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
        self.syncAnchorStore = includeMountRootChildren ? .workingSet : .shared
        super.init()
    }

    init(empty: ()) {
        self.client = nil
        self.mountId = nil
        self.containerIdentifier = nil
        self.domainId = nil
        self.namespaceMountId = nil
        self.includeMountRootChildren = false
        self.syncAnchorStore = .shared
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
        completionHandler(
            try? LocalitySyncAnchor.next(items: currentItems(), store: syncAnchorStore)
        )
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
            guard
                let changes = LocalitySyncAnchor.changes(
                    since: syncAnchor,
                    currentItems: items,
                    store: syncAnchorStore
                )
            else {
                observer.finishEnumeratingWithError(
                    NSError(
                        domain: NSFileProviderErrorDomain,
                        code: NSFileProviderError.syncAnchorExpired.rawValue
                    )
                )
                return
            }
            if !changes.updatedItems.isEmpty {
                observer.didUpdate(changes.updatedItems)
            }
            if !changes.deletedIdentifiers.isEmpty {
                observer.didDeleteItems(withIdentifiers: changes.deletedIdentifiers)
            }
            observer.finishEnumeratingChanges(
                upTo: try LocalitySyncAnchor.next(items: items, store: syncAnchorStore),
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

struct LocalitySyncChanges {
    let updatedItems: [LocalityFileProviderItem]
    let deletedIdentifiers: [NSFileProviderItemIdentifier]
}

private let localitySyncSchemaVersion = 2

private struct LocalitySyncItemSnapshot: Codable, Equatable {
    let identifier: String
    let contentVersion: Data
    let metadataVersion: Data

    init(_ item: LocalityFileProviderItem) {
        self.identifier = item.itemIdentifier.rawValue
        self.contentVersion = item.itemVersion.contentVersion
        self.metadataVersion = item.itemVersion.metadataVersion
    }
}

final class LocalitySyncAnchorStore: @unchecked Sendable {
    static let shared = LocalitySyncAnchorStore(directory: defaultDirectory(scope: "Enumerators"))
    static let workingSet = LocalitySyncAnchorStore(
        directory: defaultDirectory(scope: "WorkingSet")
    )
    private static let retainedSnapshotCount = 64

    private struct StoredSnapshot: Codable {
        let schemaVersion: Int
        let items: [LocalitySyncItemSnapshot]
    }

    private let directory: URL

    init(directory: URL) {
        self.directory = directory
    }

    func save(items: [LocalityFileProviderItem]) throws -> UUID {
        try FileManager.default.createDirectory(
            at: directory,
            withIntermediateDirectories: true
        )
        let identifier = UUID()
        let snapshot = StoredSnapshot(
            schemaVersion: localitySyncSchemaVersion,
            items: items.map(LocalitySyncItemSnapshot.init).sorted {
                $0.identifier < $1.identifier
            }
        )
        try JSONEncoder().encode(snapshot).write(
            to: snapshotURL(identifier),
            options: .atomic
        )
        prune()
        return identifier
    }

    fileprivate func load(_ identifier: UUID) -> [LocalitySyncItemSnapshot]? {
        guard
            let data = try? Data(contentsOf: snapshotURL(identifier)),
            let snapshot = try? JSONDecoder().decode(StoredSnapshot.self, from: data),
            snapshot.schemaVersion == localitySyncSchemaVersion
        else {
            return nil
        }
        return snapshot.items
    }

    private func snapshotURL(_ identifier: UUID) -> URL {
        directory.appendingPathComponent("\(identifier.uuidString).json", isDirectory: false)
    }

    private func prune() {
        let modificationDateKey = URLResourceKey.contentModificationDateKey
        guard
            let urls = try? FileManager.default.contentsOfDirectory(
                at: directory,
                includingPropertiesForKeys: [modificationDateKey],
                options: [.skipsHiddenFiles]
            )
        else {
            return
        }
        let snapshots = urls.filter { $0.pathExtension == "json" }.sorted { left, right in
            let leftDate = try? left.resourceValues(forKeys: [modificationDateKey])
                .contentModificationDate
            let rightDate = try? right.resourceValues(forKeys: [modificationDateKey])
                .contentModificationDate
            return (leftDate ?? .distantPast) > (rightDate ?? .distantPast)
        }
        for url in snapshots.dropFirst(Self.retainedSnapshotCount) {
            try? FileManager.default.removeItem(at: url)
        }
    }

    private static func defaultDirectory(scope: String) -> URL {
        let base = FileManager.default.containerURL(
            forSecurityApplicationGroupIdentifier: "C484HB7Q6S.group.ai.codeflash.locality"
        ) ?? FileManager.default.temporaryDirectory
        return base
            .appendingPathComponent("Library", isDirectory: true)
            .appendingPathComponent("Caches", isDirectory: true)
            .appendingPathComponent("Locality", isDirectory: true)
            .appendingPathComponent("FileProviderSyncAnchors", isDirectory: true)
            .appendingPathComponent(scope, isDirectory: true)
    }
}

enum LocalitySyncAnchor {
    private struct Snapshot: Codable {
        let schemaVersion: Int
        let nonce: UUID
    }

    static func next(
        items: [LocalityFileProviderItem] = [],
        store: LocalitySyncAnchorStore = .shared
    ) throws -> NSFileProviderSyncAnchor {
        let snapshot = Snapshot(
            schemaVersion: localitySyncSchemaVersion,
            nonce: try store.save(items: items)
        )
        return NSFileProviderSyncAnchor(try JSONEncoder().encode(snapshot))
    }

    static func isCurrent(_ syncAnchor: NSFileProviderSyncAnchor) -> Bool {
        guard
            let snapshot = try? JSONDecoder().decode(Snapshot.self, from: syncAnchor.rawValue),
            snapshot.schemaVersion == localitySyncSchemaVersion
        else {
            return false
        }
        return true
    }

    static func changes(
        since syncAnchor: NSFileProviderSyncAnchor,
        currentItems: [LocalityFileProviderItem],
        store: LocalitySyncAnchorStore = .shared
    ) -> LocalitySyncChanges? {
        guard
            let previous = try? JSONDecoder().decode(Snapshot.self, from: syncAnchor.rawValue),
            previous.schemaVersion == localitySyncSchemaVersion,
            let previousSnapshots = store.load(previous.nonce)
        else {
            return nil
        }

        let previousItems = Dictionary(
            uniqueKeysWithValues: previousSnapshots.map { ($0.identifier, $0) }
        )
        let currentSnapshots = currentItems.map { ($0, LocalitySyncItemSnapshot($0)) }
        let currentIdentifiers = Set(currentSnapshots.map { $0.1.identifier })
        let updatedItems = currentSnapshots.compactMap { item, snapshot in
            previousItems[snapshot.identifier] == snapshot ? nil : item
        }
        let deletedIdentifiers = previousSnapshots.compactMap { snapshot in
            currentIdentifiers.contains(snapshot.identifier)
                ? nil
                : NSFileProviderItemIdentifier(snapshot.identifier)
        }
        return LocalitySyncChanges(
            updatedItems: updatedItems,
            deletedIdentifiers: deletedIdentifiers
        )
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
