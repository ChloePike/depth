// swift-tools-version:6.0
// SwiftUI host for Terminal One. The Rust side (ffi/, a staticlib) is built first by
// scripts/bundle.sh into ../target/main/release.
import PackageDescription

let package = Package(
    name: "TerminalOne",
    platforms: [.macOS("26.0")],
    targets: [
        .systemLibrary(name: "CT1", path: "Sources/CT1"),
        .executableTarget(
            name: "TerminalOne",
            dependencies: ["CT1"],
            path: "Sources/TerminalOne",
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
