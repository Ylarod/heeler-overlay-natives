// swift-tools-version: 6.0
//
// Link probe for the three binary targets of the development manifest
// (build/Artifacts): links them into an iOS test bundle and calls exported
// entry points that need no network. Run from the repository root with
// scripts/link-probe.sh after scripts/build.sh.

import PackageDescription

let package = Package(
    name: "LinkProbe",
    platforms: [
        .iOS(.v18),
    ],
    dependencies: [
        .package(name: "heeler-overlay-natives", path: "../.."),
    ],
    targets: [
        .target(
            name: "LinkProbe",
            dependencies: [
                .product(name: "CTailscale", package: "heeler-overlay-natives"),
                .product(name: "CZeroTier", package: "heeler-overlay-natives"),
                .product(name: "CEasyTier", package: "heeler-overlay-natives"),
            ],
            linkerSettings: [
                .linkedFramework("Security"),
                .linkedFramework("CoreFoundation"),
                .linkedLibrary("resolv"),
                .linkedLibrary("c++"),
            ]
        ),
        .testTarget(
            name: "LinkProbeTests",
            dependencies: ["LinkProbe"]
        ),
    ]
)
