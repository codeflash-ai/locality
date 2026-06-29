import FileProvider
import Foundation

final class LocalityEnumerator: NSObject, NSFileProviderEnumerator {
    private let client: LocalityDaemonClient?
    private let mountId: String?
    private let containerIdentifier: String?
    private let domainId: String?
    private let namespaceMountId: String?

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
        super.init()
    }

    init(client: LocalityDaemonClient, domainId: String) {
        self.client = client
        self.mountId = nil
        self.containerIdentifier = nil
        self.domainId = domainId
        self.namespaceMountId = nil
        super.init()
    }

    init(empty: ()) {
        self.client = nil
        self.mountId = nil
        self.containerIdentifier = nil
        self.domainId = nil
        self.namespaceMountId = nil
        super.init()
    }

    func invalidate() {}

    func enumerateItems(
        for observer: NSFileProviderEnumerationObserver,
        startingAt page: NSFileProviderPage
    ) {
        guard let client else {
            observer.didEnumerate([])
            observer.finishEnumerating(upTo: nil)
            return
        }

        do {
            let items: [LocalityFileProviderItem]
            if let domainId {
                let response = try client.domainChildren(domainId: domainId)
                items = response.children.map { child in
                    LocalityFileProviderItem(metadata: child.item.namespaced(for: child.mountId))
                }
            } else if let mountId, let containerIdentifier {
                let response = try client.children(
                    mountId: mountId,
                    containerIdentifier: containerIdentifier
                )
                items = response.children.map { child in
                    let metadata = namespaceMountId.map { child.namespaced(for: $0) } ?? child
                    return LocalityFileProviderItem(metadata: metadata)
                }
            } else {
                items = []
            }
            observer.didEnumerate(items)
            observer.finishEnumerating(upTo: nil)
        } catch {
            observer.finishEnumeratingWithError(agentFSFileProviderError(error))
        }
    }

    func currentSyncAnchor(
        completionHandler: @escaping (NSFileProviderSyncAnchor?) -> Void
    ) {
        completionHandler(NSFileProviderSyncAnchor(Data()))
    }

    func enumerateChanges(
        for observer: NSFileProviderChangeObserver,
        from syncAnchor: NSFileProviderSyncAnchor
    ) {
        observer.finishEnumeratingChanges(upTo: syncAnchor, moreComing: false)
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
