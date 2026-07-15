import Darwin
@preconcurrency import FileProvider
import Foundation

private let exitUsage: Int32 = 2

@main
struct LocalityFileProviderCtl {
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
  case open(mountId: String)
  case signal(mountId: String, identifier: String)
  case reimport(mountId: String, identifier: String)
  case unregister(mountId: String)
  case list
  case reset

  static func parse(_ arguments: [String]) throws -> Self {
    let args = arguments.filter { $0 != "--json" }
    guard let action = args.first else {
      throw UsageError(
        "usage: locality-file-providerctl register|open|unregister|list|reset [options]")
    }

    switch action {
    case "register":
      let mountId = try requiredValue(args, "--mount-id")
      let displayName = value(args, "--display-name") ?? mountId
      try validateDomainIdentifier(mountId)
      return .register(mountId: mountId, displayName: displayName)
    case "open":
      let mountId = try requiredValue(args, "--mount-id")
      try validateDomainIdentifier(mountId)
      return .open(mountId: mountId)
    case "signal":
      let mountId = try requiredValue(args, "--mount-id")
      let identifier = value(args, "--identifier") ?? "root"
      try validateDomainIdentifier(mountId)
      return .signal(mountId: mountId, identifier: identifier)
    case "reimport":
      let mountId = try requiredValue(args, "--mount-id")
      let identifier = value(args, "--identifier") ?? "root"
      try validateDomainIdentifier(mountId)
      return .reimport(mountId: mountId, identifier: identifier)
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
    case .register(let mountId, let displayName):
      let identifier = NSFileProviderDomainIdentifier(mountId)
      if let existing = try getDomains().first(where: { $0.identifier == identifier }) {
        if shouldReplaceExistingDomain(existing, displayName: displayName) {
          try waitForVoid { completion in
            NSFileProviderManager.remove(existing, completionHandler: completion)
          }
        } else {
          return FileProviderCtlReport(
            ok: true,
            action: "register",
            domain: DomainReport(existing),
            domains: nil,
            url: nil,
            message: "already registered \(mountId)"
          )
        }
      }

      let domain = NSFileProviderDomain(
        identifier: identifier,
        displayName: displayName
      )
      if #available(macOS 13.0, *) {
        domain.supportsSyncingTrash = false
      }
      do {
        try waitForVoid { completion in
          NSFileProviderManager.add(domain, completionHandler: completion)
        }
      } catch {
        guard isFileProviderDomainAlreadyExists(error) else {
          throw error
        }
        let existing = try getDomains().first(where: { $0.identifier == identifier }) ?? domain
        return FileProviderCtlReport(
          ok: true,
          action: "register",
          domain: DomainReport(existing),
          domains: nil,
          url: nil,
          message: "already registered \(mountId)"
        )
      }
      return FileProviderCtlReport(
        ok: true,
        action: "register",
        domain: DomainReport(domain),
        domains: nil,
        url: nil,
        message: "registered \(mountId)"
      )
    case .open(let mountId):
      guard let domain = try getDomains().first(where: { $0.identifier.rawValue == mountId }) else {
        throw UsageError("File Provider domain \(mountId) is not registered")
      }
      guard domain.userEnabled else {
        throw UsageError(
          "The Locality File Provider is registered but not enabled. Enable Locality in Finder or System Settings, then try again."
        )
      }
      let url = try userVisibleDomainURL(for: domain)
      guard FileManager.default.fileExists(atPath: url.path) else {
        throw UsageError(
          "File Provider domain \(mountId) exists but macOS has not created \(url.path). Enable the Locality File Provider extension in System Settings, then try again."
        )
      }
      return FileProviderCtlReport(
        ok: true,
        action: "open",
        domain: DomainReport(domain),
        domains: nil,
        url: url.path,
        message: "resolved \(mountId)"
      )
    case .signal(let mountId, let identifier):
      guard let domain = try getDomains().first(where: { $0.identifier.rawValue == mountId }) else {
        throw UsageError("File Provider domain \(mountId) is not registered")
      }
      guard let manager = NSFileProviderManager(for: domain) else {
        throw UsageError("No File Provider manager is available for domain \(mountId)")
      }
      try waitForVoid { completion in
        manager.signalEnumerator(
          for: fileProviderItemIdentifier(identifier),
          completionHandler: completion
        )
      }
      return FileProviderCtlReport(
        ok: true,
        action: "signal",
        domain: DomainReport(domain),
        domains: nil,
        url: nil,
        message: "signaled \(mountId):\(identifier)"
      )
    case .reimport(let mountId, let identifier):
      guard let domain = try getDomains().first(where: { $0.identifier.rawValue == mountId }) else {
        throw UsageError("File Provider domain \(mountId) is not registered")
      }
      guard let manager = NSFileProviderManager(for: domain) else {
        throw UsageError("No File Provider manager is available for domain \(mountId)")
      }
      try waitForVoid { completion in
        manager.reimportItems(
          below: fileProviderItemIdentifier(identifier),
          completionHandler: completion
        )
      }
      return FileProviderCtlReport(
        ok: true,
        action: "reimport",
        domain: DomainReport(domain),
        domains: nil,
        url: nil,
        message: "reimported \(mountId):\(identifier)"
      )
    case .unregister(let mountId):
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
        url: nil,
        message: "unregistered \(mountId)"
      )
    case .list:
      let domains = try getDomains()
      return FileProviderCtlReport(
        ok: true,
        action: "list",
        domain: nil,
        domains: domains.map(DomainReport.init),
        url: nil,
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
        url: nil,
        message: "removed all file provider domains"
      )
    }
  }
}

private func fileProviderItemIdentifier(_ identifier: String) -> NSFileProviderItemIdentifier {
  if identifier == "root" {
    return .rootContainer
  }
  if identifier == "working-set" {
    return .workingSet
  }
  return NSFileProviderItemIdentifier(identifier)
}

private func isFileProviderDomainAlreadyExists(_ error: Error) -> Bool {
  let nsError = error as NSError
  if nsError.domain == NSCocoaErrorDomain && nsError.code == NSFileWriteFileExistsError {
    return true
  }

  let message = nsError.localizedDescription.lowercased()
  return message.contains("already exists") || message.contains("same name already exists")
}

private struct FileProviderCtlReport: Encodable {
  let ok: Bool
  let action: String
  let domain: DomainReport?
  let domains: [DomainReport]?
  let url: String?
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

private func userVisibleDomainURL(for domain: NSFileProviderDomain) throws -> URL {
  if let url = try userVisibleDomainURLFromManager(for: domain) {
    return url
  }

  let cloudStorage = realHomeDirectoryURL()
    .appendingPathComponent("Library", isDirectory: true)
    .appendingPathComponent("CloudStorage", isDirectory: true)
  let primaryDirectoryName = fileProviderDirectoryName(for: domain.displayName)
  let candidate = cloudStorage.appendingPathComponent(primaryDirectoryName, isDirectory: true)
  if FileManager.default.fileExists(atPath: candidate.path) {
    return candidate
  }
  return candidate
}

private func shouldReplaceExistingDomain(_ domain: NSFileProviderDomain, displayName: String) -> Bool {
  let state: UserVisibleDomainURLState
  do {
    state = .available(try userVisibleDomainURLFromManager(for: domain))
  } catch {
    state = .unavailable
  }
  return existingDomainNeedsReplacement(
    displayName: domain.displayName,
    requestedDisplayName: displayName,
    visibleURLState: state
  )
}

private func realHomeDirectoryURL() -> URL {
  if let passwd = getpwuid(getuid()) {
    return URL(fileURLWithPath: String(cString: passwd.pointee.pw_dir), isDirectory: true)
  }
  return FileManager.default.homeDirectoryForCurrentUser
}

private func userVisibleDomainURLFromManager(for domain: NSFileProviderDomain) throws -> URL? {
  guard let manager = NSFileProviderManager(for: domain) else {
    return nil
  }

  let result = AsyncResultBox<URL?>()
  let semaphore = DispatchSemaphore(value: 0)
  manager.getUserVisibleURL(for: .rootContainer) { url, error in
    if let error {
      result.complete(.failure(error))
    } else if let url {
      result.complete(.success(url))
    } else {
      result.complete(.success(nil))
    }
    semaphore.signal()
  }
  semaphore.wait()
  return try result.get() ?? nil
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
    throw UsageError(
      "--mount-id cannot contain '/' or ':' because File Provider domain identifiers reject them")
  }
}

private func printJSON<T: Encodable>(_ value: T) {
  let encoder = JSONEncoder()
  encoder.outputFormatting = [.prettyPrinted, .sortedKeys]
  do {
    let data = try encoder.encode(value)
    print(String(decoding: data, as: UTF8.self))
  } catch {
    print(
      "{\"ok\":false,\"code\":\"json_encode_failed\",\"message\":\"\(escape(error.localizedDescription))\"}"
    )
  }
}

private func printPlain(_ report: FileProviderCtlReport) {
  if let domains = report.domains {
    if domains.isEmpty {
      print("no Locality File Provider domains registered")
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
    FileHandle.standardError.write(Data("locality-file-providerctl: \(message)\n".utf8))
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
