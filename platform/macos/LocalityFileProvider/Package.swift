// swift-tools-version: 6.0

import PackageDescription

let package = Package(
    name: "LocalityFileProvider",
    platforms: [
        .macOS(.v14),
    ],
    products: [
        .library(
            name: "LocalityFileProvider",
            targets: ["LocalityFileProvider"]
        ),
        .executable(
            name: "locality-file-providerctl",
            targets: ["LocalityFileProviderCtl"]
        ),
    ],
    targets: [
        .target(
            name: "LocalityFileProvider",
            path: "Sources/LocalityFileProvider"
        ),
        .executableTarget(
            name: "LocalityFileProviderCtl",
            path: "Sources/LocalityFileProviderCtl"
        ),
    ]
)
