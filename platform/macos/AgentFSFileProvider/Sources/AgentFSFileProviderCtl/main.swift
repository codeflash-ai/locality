import Darwin
@preconcurrency import FileProvider
import Foundation

private let exitUsage: Int32 = 2

@main
struct AgentFSFileProviderCtl {
    static func main() {
        let arguments = Array(CommandLine.arguments.dropFirst())
        let json = arguments.contains("--json")

        do {
            let command = try Command.parse(arguments)
            let result = try command.run()
            if json {
                printJSON(result)
            } else {
                printPlain(result)
            }
        } catch let error as UsageError {
            writeError(error.message, json: json, code: "usage")
            exit(exitUsage)
        } catch {
            writeError(error.localizedDescription, json: json, code: "file_provider_error")
            exit(1)
        }
    }
}

private enum Command {
    case register(mountId: String, displayName: String)
    case unregister(mountId: String)
    case list
    case reset

    static func parse(_ arguments: [String]) throws -> Self {
        let args = arguments.filter { $0 != "--json" }
        guard let action = args.first else {
            throw UsageError("usage: agentfs-file-providerctl register|unregister|list|reset [options]")
        }

        switch action {
        case "register":
            let mountId = try requiredValue(args, "--mount-id")
            let displayName = value(args, "--display-name") ?? mountId
            try validateDomainIdentifier(mountId)
            return .register(mountId: mountId, displayName: displayName)
        case "unregister":
            let mountId = try requiredValue(args, "--mount-id")
            try validateDomainIdentifier(mountId)
            return .unregister(mountId: mountId)
        case "list":
            return .list
        case "reset":
            return .reset
        default:
            throw UsageError("unknown file provider action: \(action)")
        }
    }

    func run() throws -> FileProviderCtlReport {
        switch self {
        case let .register(mountId, displayName):
            let domain = NSFileProviderDomain(
                identifier: NSFileProviderDomainIdentifier(mountId),
                displayName: displayName
            )
            if #available(macOS 13.0, *) {
                domain.supportsSyncingTrash = false
            }
            try waitForVoid { completion in
                NSFileProviderManager.add(domain, completionHandler: completion)
            }
            return FileProviderCtlReport(
                ok: true,
                action: "register",
                domain: DomainReport(domain),
                domains: nil,
                message: "registered \(mountId)"
            )
        case let .unregister(mountId):
            let domain = NSFileProviderDomain(
                identifier: NSFileProviderDomainIdentifier(mountId),
                displayName: mountId
            )
            try waitForVoid { completion in
                NSFileProviderManager.remove(domain, completionHandler: completion)
            }
            return FileProviderCtlReport(
                ok: true,
                action: "unregister",
                domain: DomainReport(domain),
                domains: nil,
                message: "unregistered \(mountId)"
            )
        case .list:
            let domains = try getDomains()
            return FileProviderCtlReport(
                ok: true,
                action: "list",
                domain: nil,
                domains: domains.map(DomainReport.init),
                message: "listed \(domains.count) domain(s)"
            )
        case .reset:
            try waitForVoid { completion in
                NSFileProviderManager.removeAllDomains(completionHandler: completion)
            }
            return FileProviderCtlReport(
                ok: true,
                action: "reset",
                domain: nil,
                domains: nil,
                message: "removed all file provider domains"
            )
        }
    }
}

private struct FileProviderCtlReport: Encodable {
    let ok: Bool
    let action: String
    let domain: DomainReport?
    let domains: [DomainReport]?
    let message: String
}

private struct DomainReport: Encodable {
    let identifier: String
    let displayName: String
    let userEnabled: Bool
    let disconnected: Bool
    let hidden: Bool

    init(_ domain: NSFileProviderDomain) {
        self.identifier = domain.identifier.rawValue
        self.displayName = domain.displayName
        self.userEnabled = domain.userEnabled
        self.disconnected = domain.isDisconnected
        self.hidden = domain.isHidden
    }
}

private struct ErrorReport: Encodable {
    let ok = false
    let code: String
    let message: String
}

private struct UsageError: Error {
    let message: String

    init(_ message: String) {
        self.message = message
    }
}

private func getDomains() throws -> [NSFileProviderDomain] {
    let result = AsyncResultBox<[NSFileProviderDomain]>()
    let semaphore = DispatchSemaphore(value: 0)
    NSFileProviderManager.getDomainsWithCompletionHandler { domains, error in
        if let error {
            result.complete(.failure(error))
        } else {
            result.complete(.success(domains))
        }
        semaphore.signal()
    }
    semaphore.wait()
    return try result.get() ?? []
}

private func waitForVoid(_ body: (@escaping @Sendable (Error?) -> Void) -> Void) throws {
    let result = AsyncResultBox<Void>()
    let semaphore = DispatchSemaphore(value: 0)
    body { error in
        if let error {
            result.complete(.failure(error))
        } else {
            result.complete(.success(()))
        }
        semaphore.signal()
    }
    semaphore.wait()
    _ = try result.get()
}

private func requiredValue(_ args: [String], _ flag: String) throws -> String {
    guard let value = value(args, flag) else {
        throw UsageError("\(flag) is required")
    }
    return value
}

private func value(_ args: [String], _ flag: String) -> String? {
    guard let index = args.firstIndex(of: flag), args.indices.contains(index + 1) else {
        return nil
    }
    return args[index + 1]
}

private func validateDomainIdentifier(_ identifier: String) throws {
    guard !identifier.isEmpty else {
        throw UsageError("--mount-id cannot be empty")
    }
    if identifier.contains("/") || identifier.contains(":") {
        throw UsageError("--mount-id cannot contain '/' or ':' because File Provider domain identifiers reject them")
    }
}

private func printJSON<T: Encodable>(_ value: T) {
    let encoder = JSONEncoder()
    encoder.outputFormatting = [.prettyPrinted, .sortedKeys]
    do {
        let data = try encoder.encode(value)
        print(String(decoding: data, as: UTF8.self))
    } catch {
        print("{\"ok\":false,\"code\":\"json_encode_failed\",\"message\":\"\(escape(error.localizedDescription))\"}")
    }
}

private func printPlain(_ report: FileProviderCtlReport) {
    if let domains = report.domains {
        if domains.isEmpty {
            print("no AgentFS File Provider domains registered")
        } else {
            for domain in domains {
                print("\(domain.identifier)\t\(domain.displayName)")
            }
        }
        return
    }
    print(report.message)
}

private func writeError(_ message: String, json: Bool, code: String) {
    if json {
        printJSON(ErrorReport(code: code, message: message))
    } else {
        FileHandle.standardError.write(Data("agentfs-file-providerctl: \(message)\n".utf8))
    }
}

private func escape(_ value: String) -> String {
    value.replacingOccurrences(of: "\\", with: "\\\\")
        .replacingOccurrences(of: "\"", with: "\\\"")
}

private final class AsyncResultBox<Value>: @unchecked Sendable {
    private let lock = NSLock()
    private var result: Result<Value, Error>?

    func complete(_ result: Result<Value, Error>) {
        lock.lock()
        self.result = result
        lock.unlock()
    }

    func get() throws -> Value? {
        lock.lock()
        let result = self.result
        lock.unlock()
        return try result?.get()
    }
}
