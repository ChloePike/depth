// swift-tools-version:6.0
// SwiftUI host for Depth. The Rust side (ffi/, a staticlib) is built first by
// scripts/bundle.sh into ../target/main/release.
import PackageDescription

let package = Package(
    name: "Depth",
    platforms: [.macOS("26.0")],
    targets: [
        .systemLibrary(name: "CT1", path: "Sources/CT1"),
        .executableTarget(
            name: "Depth",
            dependencies: ["CT1"],
            path: "Sources/Depth",
            linkerSettings: [
                .unsafeFlags(["-L../target/main/release"]),
                .linkedFramework("Metal"), .linkedFramework("QuartzCore"), .linkedFramework("AppKit"),
                .linkedFramework("Security"), .linkedFramework("SystemConfiguration"), .linkedFramework("CoreFoundation"),
                .linkedFramework("IOKit"), .linkedFramework("CoreGraphics"), .linkedFramework("CoreText"),
                .linkedLibrary("c++"), .linkedLibrary("z"),
            ]
        ),
    ]
)
