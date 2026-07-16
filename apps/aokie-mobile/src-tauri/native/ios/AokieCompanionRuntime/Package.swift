// swift-tools-version: 5.9

import PackageDescription

let package = Package(
    name: "AokieCompanionRuntime",
    platforms: [.iOS(.v15)],
    products: [
        .library(name: "AokieCompanionRuntime", targets: ["AokieCompanionRuntime"]),
    ],
    targets: [
        .target(name: "AokieCompanionRuntime"),
        .testTarget(
            name: "AokieCompanionRuntimeTests",
            dependencies: ["AokieCompanionRuntime"]
        ),
    ]
)
