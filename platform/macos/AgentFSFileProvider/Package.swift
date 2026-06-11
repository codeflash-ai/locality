// swift-tools-version: 6.0

import PackageDescription

let package = Package(
    name: "AgentFSFileProvider",
    platforms: [
        .macOS(.v14),
    ],
    products: [
        .library(
            name: "AgentFSFileProvider",
            targets: ["AgentFSFileProvider"]
        ),
        .executable(
            name: "agentfs-file-providerctl",
            targets: ["AgentFSFileProviderCtl"]
        ),
    ],
    targets: [
        .target(
            name: "AgentFSFileProvider",
            path: "Sources/AgentFSFileProvider"
        ),
        .executableTarget(
            name: "AgentFSFileProviderCtl",
            path: "Sources/AgentFSFileProviderCtl"
        ),
    ]
)
