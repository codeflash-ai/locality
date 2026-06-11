import FileProvider
import Foundation

final class AgentFSEnumerator: NSObject, NSFileProviderEnumerator {
    private let client: AgentFSDaemonClient
    private let mountId: String
    private let containerIdentifier: String

    init(client: AgentFSDaemonClient, mountId: String, containerIdentifier: String) {
        self.client = client
        self.mountId = mountId
        self.containerIdentifier = containerIdentifier
        super.init()
    }

    func invalidate() {}

    func enumerateItems(
        for observer: NSFileProviderEnumerationObserver,
        startingAt page: NSFileProviderPage
    ) {
        do {
            let response = try client.children(
                mountId: mountId,
                containerIdentifier: containerIdentifier
            )
            let items = response.children.map(AgentFSFileProviderItem.init(metadata:))
            observer.didEnumerate(items)
            observer.finishEnumerating(upTo: nil)
        } catch {
            observer.finishEnumeratingWithError(error)
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
