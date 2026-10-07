// Renders the Depth app icon (assets/appicon/icon-1024.png): a mirrored order book: bid depth
// bars on the left (green), ask bars on the right (red), the mid price line between them, on a
// near-black squircle with a soft top light. Run: swift scripts/make-icon.swift [out.png]
import AppKit
import SwiftUI

struct Icon: View {
    // bar lengths top to bottom (fraction of the half width): uneven like a real book
    let bids: [Double] = [0.34, 0.56, 0.44, 0.82, 0.62, 1.0]
    let asks: [Double] = [0.30, 0.50, 0.72, 0.46, 0.92, 0.76]
    let green = LinearGradient(colors: [Color(red: 0.33, green: 0.93, blue: 0.55), Color(red: 0.13, green: 0.72, blue: 0.38)], startPoint: .trailing, endPoint: .leading)
    let red = LinearGradient(colors: [Color(red: 1.0, green: 0.44, blue: 0.40), Color(red: 0.86, green: 0.20, blue: 0.20)], startPoint: .leading, endPoint: .trailing)

    var body: some View {
        let shape = RoundedRectangle(cornerRadius: 185, style: .continuous)
        ZStack {
            shape.fill(LinearGradient(colors: [Color(white: 0.17), Color(white: 0.05)], startPoint: .top, endPoint: .bottom))
            HStack(spacing: 22) {
                VStack(alignment: .trailing, spacing: 22) {
                    ForEach(bids.indices, id: \.self) { i in
                        RoundedRectangle(cornerRadius: 11, style: .continuous).fill(green).frame(width: 250 * bids[i], height: 46)
                    }
                }
                .frame(width: 250, alignment: .trailing)
                Capsule().fill(.white).frame(width: 14, height: 440)
                    .shadow(color: .white.opacity(0.55), radius: 14)
                VStack(alignment: .leading, spacing: 22) {
                    ForEach(asks.indices, id: \.self) { i in
                        RoundedRectangle(cornerRadius: 11, style: .continuous).fill(red).frame(width: 250 * asks[i], height: 46)
                    }
                }
                .frame(width: 250, alignment: .leading)
            }
            // top light and rim, like the system icons
            shape.fill(LinearGradient(colors: [.white.opacity(0.14), .white.opacity(0.0)], startPoint: .top, endPoint: .center))
            shape.strokeBorder(LinearGradient(colors: [.white.opacity(0.30), .white.opacity(0.05)], startPoint: .top, endPoint: .bottom), lineWidth: 3)
        }
        .frame(width: 824, height: 824)
        .compositingGroup()
        .shadow(color: .black.opacity(0.35), radius: 22, y: 12)
        .frame(width: 1024, height: 1024)
    }
}

@MainActor func render() {
    let r = ImageRenderer(content: Icon())
    r.scale = 1
    guard let img = r.nsImage, let tiff = img.tiffRepresentation, let rep = NSBitmapImageRep(data: tiff),
          let png = rep.representation(using: .png, properties: [:]) else { fatalError("render failed") }
    let out = URL(fileURLWithPath: CommandLine.arguments.count > 1 ? CommandLine.arguments[1] : "assets/appicon/icon-1024.png")
    try! png.write(to: out)
    print("wrote \(out.path)")
}
MainActor.assumeIsolated { render() }
