import SwiftUI

/// Palette shared with the Rust side (ui/src/theme.rs); accent and up/down follow Settings.
enum T1 {
    static let bg = Color(hex: 0x0a0d11)
    static let panel = Color(hex: 0x12161c)
    static let panel2 = Color(hex: 0x1a1f27)
    static let hl = Color(hex: 0x20262f)
    static let line = Color(hex: 0x242a33)
    static let fg = Color(hex: 0xe6e9ed)
    static let mu = Color(hex: 0x8a929d)
    static let dim = Color(hex: 0x59616c)
    static let warn = Color(hex: 0xe8a33d)
    static let green = Color(nsColor: .systemGreen)
    static let red = Color(nsColor: .systemRed)

    @MainActor static var accent: Color { let a = Store.shared.state.prefs.accent; return a.count == 3 ? Color(red: Double(a[0]) / 255, green: Double(a[1]) / 255, blue: Double(a[2]) / 255) : .blue }
    @MainActor static var up: Color { Store.shared.state.prefs.red_up ? red : green }
    @MainActor static var down: Color { Store.shared.state.prefs.red_up ? green : red }

    static func mono(_ size: CGFloat, _ weight: Font.Weight = .regular) -> Font { .system(size: size, weight: weight, design: .monospaced) }
    static func ui(_ size: CGFloat, _ weight: Font.Weight = .regular) -> Font { .system(size: size, weight: weight) }
}

extension Color {
    init(hex: UInt32) { self.init(red: Double((hex >> 16) & 0xff) / 255, green: Double((hex >> 8) & 0xff) / 255, blue: Double(hex & 0xff) / 255) }
}

/// Number formatting matching the Rust side (fmt_px / fmt_qty).
enum Fmt {
    static func px(_ v: Double?) -> String {
        guard let v, v.isFinite else { return "–" }
        let a = abs(v)
        let d = a >= 10_000 ? 1 : a >= 100 ? 2 : a >= 1 ? 4 : 6
        return num(v, d)
    }
    static func qty(_ v: Double?) -> String {
        guard let v, v.isFinite else { return "–" }
        let a = abs(v)
        return num(v, a >= 1000 ? 1 : a >= 1 ? 3 : 5)
    }
    static func num(_ v: Double, _ d: Int) -> String {
        let f = NumberFormatter()
        f.numberStyle = .decimal; f.minimumFractionDigits = d; f.maximumFractionDigits = d
        return f.string(from: NSNumber(value: v)) ?? "\(v)"
    }
    static func usd(_ v: Double?, _ d: Int = 2) -> String { v.map { num($0, d) } ?? "–" }
    static func big(_ v: Double?) -> String {
        guard let v else { return "–" }
        let a = abs(v)
        if a >= 1e9 { return String(format: "%.2fB", v / 1e9) }
        if a >= 1e6 { return String(format: "%.2fM", v / 1e6) }
        if a >= 1e3 { return String(format: "%.1fK", v / 1e3) }
        return String(format: "%.2f", v)
    }
    static func signed(_ v: Double?, _ d: Int = 2) -> String {
        guard let v else { return "–" }
        // anything that rounds to zero prints as an unsigned zero, never "+-0.00"
        if abs(v) < 0.5 * pow(10, -Double(d)) { return num(0, d) }
        return (v > 0 ? "+" : "") + num(v, d)
    }
    static func time(_ ms: Int64) -> String {
        let f = DateFormatter(); f.dateFormat = "MM-dd HH:mm:ss"
        return f.string(from: Date(timeIntervalSince1970: Double(ms) / 1000))
    }
}

/// Coin logo (Binance CDN, the same source the Rust side caches); a lettered disc meanwhile.
struct CoinIcon: View {
    let base: String
    var size: CGFloat = 18
    var body: some View {
        AsyncImage(url: URL(string: "https://bin.bnbstatic.com/static/assets/logos/\(base).png")) { img in
            img.resizable().interpolation(.high)
        } placeholder: {
            ZStack { Circle().fill(T1.hl); Text(String(base.prefix(1))).font(T1.ui(size * 0.5, .semibold)).foregroundStyle(T1.mu) }
        }
        .frame(width: size, height: size).clipShape(Circle())
    }
}

/// Exchange logos: app bundle Resources when packaged, assets/icons in the source tree in development.
@MainActor enum VenueImages {
    private static var cache: [String: NSImage] = [:]
    static func image(_ ex: String) -> NSImage? {
        let key = ex.lowercased()
        if let i = cache[key] { return i }
        let dev = URL(fileURLWithPath: #filePath).deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent()
            .deletingLastPathComponent().appendingPathComponent("assets/icons/\(key).png")
        guard let url = Bundle.main.url(forResource: key, withExtension: "png") ?? (FileManager.default.fileExists(atPath: dev.path) ? dev : nil),
              let img = NSImage(contentsOf: url) else { return nil }
        cache[key] = img
        return img
    }
}

/// Exchange logo, else a dot.
struct VenueIcon: View {
    let ex: String
    var size: CGFloat = 14
    var body: some View {
        if let img = VenueImages.image(ex) {
            Image(nsImage: img).resizable().frame(width: size, height: size).clipShape(Circle())
        } else {
            Circle().fill(T1.mu).frame(width: size * 0.6, height: size * 0.6).frame(width: size, height: size)
        }
    }
}

/// UI text: English in source, Chinese from app/Resources/zh-*.txt ("English = Chinese" per line),
/// chosen by the app language (Settings / status bar, shared with the Rust side).
@MainActor func L(_ en: String) -> String {
    Store.shared.state.lang_zh ? (I18n.zh[en] ?? en) : en
}

enum I18n {
    static let zh: [String: String] = {
        var out: [String: String] = [:]
        // packaged: Contents/Resources; development: the source tree next to this file
        let bundled = Bundle.main.resourceURL.map { [$0] } ?? []
        let src = URL(fileURLWithPath: #filePath).deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent().appendingPathComponent("Resources")
        for dir in bundled + [src] {
            guard let files = try? FileManager.default.contentsOfDirectory(at: dir, includingPropertiesForKeys: nil) else { continue }
            for f in files where f.lastPathComponent.hasPrefix("zh-") && f.pathExtension == "txt" {
                guard let text = try? String(contentsOf: f, encoding: .utf8) else { continue }
                for line in text.split(separator: "\n") where !line.hasPrefix("#") {
                    let parts = line.split(separator: "=", maxSplits: 1).map { $0.trimmingCharacters(in: .whitespaces) }
                    if parts.count == 2, out[parts[0]] == nil { out[parts[0]] = parts[1] }
                }
            }
            if !out.isEmpty { break }
        }
        return out
    }()
}

/// Liquid Glass panel: a floating glass card with concentric corners, inset from the window edge.
struct GlassCard: ViewModifier {
    var radius: CGFloat = 18
    func body(content: Content) -> some View {
        content
            .clipShape(.rect(cornerRadius: radius))
            // darkened glass: the plain regular material lifts everything into a milky haze in dark mode
            .glassEffect(.regular.tint(.black.opacity(0.35)), in: .rect(cornerRadius: radius))
    }
}
extension View {
    func glassCard(_ radius: CGFloat = 18) -> some View { modifier(GlassCard(radius: radius)) }
    /// Content surface (chart, tables): opaque, never glass, so data keeps full contrast.
    func contentCard(_ radius: CGFloat = 18) -> some View {
        self.clipShape(.rect(cornerRadius: radius))
            .background(Color(nsColor: .textBackgroundColor), in: .rect(cornerRadius: radius))
            .overlay { RoundedRectangle(cornerRadius: radius).strokeBorder(Color.primary.opacity(0.07), lineWidth: 1) }
    }
}

/// A titled group inside a glass panel (replaces grouped-Form boxes).
struct PanelSection<Content: View, Header: View, Footer: View>: View {
    let content: Content, header: Header, footer: Footer
    init(@ViewBuilder content: () -> Content, @ViewBuilder header: () -> Header, @ViewBuilder footer: () -> Footer) {
        self.content = content(); self.header = header(); self.footer = footer()
    }
    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            header.font(.subheadline.weight(.semibold)).foregroundStyle(.secondary)
            content
            footer.font(.caption).foregroundStyle(.secondary)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }
}
extension PanelSection where Header == EmptyView, Footer == EmptyView {
    init(@ViewBuilder content: () -> Content) { self.init(content: content, header: { EmptyView() }, footer: { EmptyView() }) }
}
extension PanelSection where Header == Text, Footer == EmptyView {
    init(_ title: String, @ViewBuilder content: () -> Content) { self.init(content: content, header: { Text(title) }, footer: { EmptyView() }) }
}
extension PanelSection where Footer == EmptyView {
    init(@ViewBuilder content: () -> Content, @ViewBuilder header: () -> Header) { self.init(content: content, header: header, footer: { EmptyView() }) }
}
extension PanelSection where Header == EmptyView {
    init(@ViewBuilder content: () -> Content, @ViewBuilder footer: () -> Footer) { self.init(content: content, header: { EmptyView() }, footer: footer) }
}

/// Label on the left, switch on the right (what Form gives toggles), outside a Form.
struct RowSwitch: ToggleStyle {
    func makeBody(configuration: Configuration) -> some View {
        HStack {
            configuration.label
            Spacer(minLength: 8)
            Toggle("", isOn: configuration.$isOn).labelsHidden().toggleStyle(.switch).controlSize(.small)
        }
        .frame(maxWidth: .infinity)
    }
}

/// Label left in secondary, value right-aligned: a Form row outside a Form.
struct RowLabeled: LabeledContentStyle {
    func makeBody(configuration: Configuration) -> some View {
        HStack(alignment: .firstTextBaseline, spacing: 8) {
            configuration.label.foregroundStyle(.secondary)
            Spacer(minLength: 8)
            configuration.content
        }
        .frame(maxWidth: .infinity)
    }
}

/// hh:mm:ss to a timestamp, ticking every second.
struct FundingCountdown: View {
    let next: Int64
    var body: some View {
        TimelineView(.periodic(from: .now, by: 1)) { ctx in
            let s = max(0, Int(Double(next) / 1000 - ctx.date.timeIntervalSince1970))
            Text(String(format: "%02d:%02d:%02d", s / 3600, s % 3600 / 60, s % 60)).monospacedDigit()
        }
    }
}
