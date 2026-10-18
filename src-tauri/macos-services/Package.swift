// swift-tools-version: 6.2
// openapp-macos: on-device macOS services for the OpenApp kernel (contract §18).
// JSON-RPC 2.0 over stdio, newline-delimited. Built by src-tauri/build.rs (`swift build -c release`).
import PackageDescription

let package = Package(
    name: "openapp-macos",
    platforms: [.macOS("26.0")],
    products: [.executable(name: "openapp-macos", targets: ["OpenAppMacOS"])],
    targets: [
        .executableTarget(name: "OpenAppMacOS", path: "Sources/OpenAppMacOS")
    ]
)
