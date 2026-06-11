import FileProvider
import Foundation
import UniformTypeIdentifiers

final class AgentFSFileProviderItem: NSObject, NSFileProviderItem {
    let itemIdentifier: NSFileProviderItemIdentifier
    let parentItemIdentifier: NSFileProviderItemIdentifier
    let filename: String
    let contentType: UTType
    let capabilities: NSFileProviderItemCapabilities
    let documentSize: NSNumber?
    let childItemCount: NSNumber?

    init(metadata: AgentFSItemMetadata) {
        self.itemIdentifier = Self.appleIdentifier(metadata.identifier)
        self.parentItemIdentifier = Self.appleIdentifier(
            metadata.parentIdentifier ?? AgentFSIdentifier.root
        )
        self.filename = metadata.filename
        self.contentType = UTType(metadata.contentType) ?? .data
        self.capabilities = metadata.kind == "folder"
            ? [.allowsReading, .allowsContentEnumerating]
            : [.allowsReading]
        self.documentSize = nil
        self.childItemCount = metadata.kind == "folder" ? 0 : nil
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
}

enum AgentFSIdentifier {
    static let root = "root"
}
