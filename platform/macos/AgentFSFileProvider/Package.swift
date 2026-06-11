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
    ],
    targets: [
        .target(
            name: "AgentFSFileProvider",
            path: "Sources"
        ),
    ]
)
