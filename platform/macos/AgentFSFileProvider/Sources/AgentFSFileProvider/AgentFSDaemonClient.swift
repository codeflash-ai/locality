import Darwin
import Foundation

enum AgentFSDaemonClientError: Error, LocalizedError {
    case homeDirectoryUnavailable
    case socketPathTooLong(String)
    case invalidDaemonAddress(String)
    case connectFailed(String)
    case writeFailed
    case readFailed
    case daemonError(String)
    case missingPayload

    var errorDescription: String? {
        switch self {
        case .homeDirectoryUnavailable:
            return "HOME is not set"
        case .socketPathTooLong(let path):
            return "daemon socket path is too long: \(path)"
        case .invalidDaemonAddress(let value):
            return "daemon TCP address must be host:port: \(value)"
        case .connectFailed(let message):
            return "failed to connect to afsd: \(message)"
        case .writeFailed:
            return "failed to write daemon request"
        case .readFailed:
            return "failed to read daemon response"
        case .daemonError(let message):
            return message
        case .missingPayload:
            return "daemon response did not include a payload"
        }
    }
}

struct AgentFSDaemonError: Decodable {
    let code: String
    let message: String
}

struct AgentFSDaemonResponse<Payload: Decodable>: Decodable {
    let ok: Bool
    let payload: Payload?
    let error: AgentFSDaemonError?
}

struct AgentFSItemPayload: Decodable {
    let mountId: String
    let item: AgentFSItemMetadata

    enum CodingKeys: String, CodingKey {
        case mountId = "mount_id"
        case item
    }
}

struct AgentFSChildrenPayload: Decodable {
    let mountId: String
    let containerIdentifier: String
    let children: [AgentFSItemMetadata]

    enum CodingKeys: String, CodingKey {
        case mountId = "mount_id"
        case containerIdentifier = "container_identifier"
        case children
    }
}

struct AgentFSMaterializePayload: Decodable {
    let mountId: String
    let identifier: String
    let remoteId: String
    let path: String
    let outcome: String
    let hydration: String

    enum CodingKeys: String, CodingKey {
        case mountId = "mount_id"
        case identifier
        case remoteId = "remote_id"
        case path
        case outcome
        case hydration
    }
}

struct AgentFSItemMetadata: Decodable {
    let identifier: String
    let parentIdentifier: String?
    let filename: String
    let kind: String
    let entityKind: String?
    let remoteId: String?
    let path: String
    let hydration: String?
    let contentType: String
    let remoteEditedAt: String?
    let materializedPath: String?

    enum CodingKeys: String, CodingKey {
        case identifier
        case parentIdentifier = "parent_identifier"
        case filename
        case kind
        case entityKind = "entity_kind"
        case remoteId = "remote_id"
        case path
        case hydration
        case contentType = "content_type"
        case remoteEditedAt = "remote_edited_at"
        case materializedPath = "materialized_path"
    }
}

final class AgentFSDaemonClient: @unchecked Sendable {
    private let transport: AgentFSDaemonTransport

    init(socketPath: String? = nil, tcpAddress: String? = nil) throws {
        if let socketPath {
            self.transport = .unixSocket(socketPath)
            return
        }
        if let socketPath = ProcessInfo.processInfo.environment["AFS_DAEMON_SOCKET"] {
            self.transport = .unixSocket(socketPath)
            return
        }
        let address = tcpAddress
            ?? ProcessInfo.processInfo.environment["AFS_DAEMON_TCP_ADDR"]
            ?? "127.0.0.1:38567"
        self.transport = try AgentFSDaemonTransport.parseTcpAddress(address)
    }

    static func unixSocketFromHome() throws -> AgentFSDaemonClient {
        guard let home = ProcessInfo.processInfo.environment["HOME"] else {
            throw AgentFSDaemonClientError.homeDirectoryUnavailable
        }
        return try AgentFSDaemonClient(socketPath: "\(home)/.afs/afsd.sock")
    }

    func item(mountId: String, identifier: String) throws -> AgentFSItemPayload {
        try request([
            "command": "file_provider_item",
            "mount_id": mountId,
            "identifier": identifier,
        ])
    }

    func children(mountId: String, containerIdentifier: String) throws -> AgentFSChildrenPayload {
        try request([
            "command": "file_provider_children",
            "mount_id": mountId,
            "container_identifier": containerIdentifier,
        ])
    }

    func materialize(mountId: String, identifier: String) throws -> AgentFSMaterializePayload {
        try request([
            "command": "file_provider_materialize",
            "mount_id": mountId,
            "identifier": identifier,
        ])
    }

    private func request<Payload: Decodable>(_ object: [String: String]) throws -> Payload {
        var payload = try JSONSerialization.data(withJSONObject: object)
        payload.append(0x0a)

        let fd = try connect()
        defer {
            close(fd)
        }

        try payload.withUnsafeBytes { rawBuffer in
            guard let baseAddress = rawBuffer.baseAddress else {
                throw AgentFSDaemonClientError.writeFailed
            }
            var written = 0
            while written < rawBuffer.count {
                let count = Darwin.write(
                    fd,
                    baseAddress.advanced(by: written),
                    rawBuffer.count - written
                )
                if count <= 0 {
                    throw AgentFSDaemonClientError.writeFailed
                }
                written += count
            }
        }

        let responseData = try readLine(fd: fd)
        let response = try JSONDecoder().decode(AgentFSDaemonResponse<Payload>.self, from: responseData)
        if response.ok, let payload = response.payload {
            return payload
        }
        throw AgentFSDaemonClientError.daemonError(
            response.error.map { "\($0.code): \($0.message)" } ?? "unknown daemon error"
        )
    }

    private func connect() throws -> Int32 {
        switch transport {
        case let .unixSocket(path):
            return try connectUnixSocket(path)
        case let .tcp(host, port):
            return try connectTcp(host: host, port: port)
        }
    }

    private func connectUnixSocket(_ socketPath: String) throws -> Int32 {
        let fd = socket(AF_UNIX, SOCK_STREAM, 0)
        if fd < 0 {
            throw AgentFSDaemonClientError.connectFailed(String(cString: strerror(errno)))
        }

        var address = sockaddr_un()
        address.sun_family = sa_family_t(AF_UNIX)
        let maxPathLength = MemoryLayout.size(ofValue: address.sun_path)
        guard socketPath.utf8.count < maxPathLength else {
            close(fd)
            throw AgentFSDaemonClientError.socketPathTooLong(socketPath)
        }

        socketPath.withCString { path in
            withUnsafeMutableBytes(of: &address.sun_path) { rawBuffer in
                guard let baseAddress = rawBuffer.baseAddress else {
                    return
                }
                memset(baseAddress, 0, rawBuffer.count)
                strncpy(baseAddress.assumingMemoryBound(to: CChar.self), path, rawBuffer.count - 1)
            }
        }

        let result = withUnsafePointer(to: &address) { pointer in
            pointer.withMemoryRebound(to: sockaddr.self, capacity: 1) { sockaddrPointer in
                Darwin.connect(fd, sockaddrPointer, socklen_t(MemoryLayout<sockaddr_un>.size))
            }
        }
        if result != 0 {
            let message = String(cString: strerror(errno))
            close(fd)
            throw AgentFSDaemonClientError.connectFailed(message)
        }

        return fd
    }

    private func connectTcp(host: String, port: UInt16) throws -> Int32 {
        let fd = socket(AF_INET, SOCK_STREAM, 0)
        if fd < 0 {
            throw AgentFSDaemonClientError.connectFailed(String(cString: strerror(errno)))
        }

        var address = sockaddr_in()
        address.sin_family = sa_family_t(AF_INET)
        address.sin_port = port.bigEndian
        guard inet_pton(AF_INET, host, &address.sin_addr) == 1 else {
            close(fd)
            throw AgentFSDaemonClientError.invalidDaemonAddress("\(host):\(port)")
        }

        let result = withUnsafePointer(to: &address) { pointer in
            pointer.withMemoryRebound(to: sockaddr.self, capacity: 1) { sockaddrPointer in
                Darwin.connect(fd, sockaddrPointer, socklen_t(MemoryLayout<sockaddr_in>.size))
            }
        }
        if result != 0 {
            let message = String(cString: strerror(errno))
            close(fd)
            throw AgentFSDaemonClientError.connectFailed(message)
        }

        return fd
    }

    private func readLine(fd: Int32) throws -> Data {
        var data = Data()
        var buffer = [UInt8](repeating: 0, count: 4096)

        while true {
            let count = Darwin.read(fd, &buffer, buffer.count)
            if count < 0 {
                throw AgentFSDaemonClientError.readFailed
            }
            if count == 0 {
                break
            }
            if let newline = buffer[..<count].firstIndex(of: 0x0a) {
                data.append(contentsOf: buffer[..<newline])
                break
            }
            data.append(contentsOf: buffer[..<count])
        }

        if data.isEmpty {
            throw AgentFSDaemonClientError.readFailed
        }
        return data
    }
}

private enum AgentFSDaemonTransport {
    case unixSocket(String)
    case tcp(host: String, port: UInt16)

    static func parseTcpAddress(_ address: String) throws -> Self {
        let parts = address.split(separator: ":", maxSplits: 1).map(String.init)
        guard parts.count == 2, let port = UInt16(parts[1]) else {
            throw AgentFSDaemonClientError.invalidDaemonAddress(address)
        }
        return .tcp(host: parts[0], port: port)
    }
}
