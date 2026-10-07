import AppKit
import SwiftUI

/// Settings sheet: sidebar of panes, grouped Form on the right. Every control writes straight
/// through to the engine (`set_prefs` / `set_route` with the full object), so there is no Save.
enum SettingsPane: String, CaseIterable, Identifiable {
    case general, appearance, trading, keys, about
    var id: String { rawValue }
    init(from s: String?) { self = s.flatMap(SettingsPane.init(rawValue:)) ?? .general }
    var title: String {
        switch self {
        case .general: "General"
        case .appearance: "Appearance"
        case .trading: "Trading"
        case .keys: "API Keys"
        case .about: "About"
        }
    }
    var icon: (String, Color) {
        switch self {
        case .general: ("gearshape.fill", .gray)
        case .appearance: ("paintpalette.fill", .indigo)
        case .trading: ("chart.line.uptrend.xyaxis", .green)
        case .keys: ("key.fill", .orange)
        case .about: ("info.circle.fill", .blue)
        }
    }
}

struct SettingsView: View {
    @Environment(Store.self) private var store
    @Environment(\.dismiss) private var dismiss
    @State private var pane: SettingsPane?

    init(initial: SettingsPane) { _pane = State(initialValue: initial) }

    var body: some View {
        NavigationSplitView {
            List(SettingsPane.allCases, selection: $pane) { p in
                Label {
                    Text(L(p.title))
                } icon: {
                    Image(systemName: p.icon.0)
                        .font(.system(size: 11, weight: .semibold)).foregroundStyle(.white)
                        .frame(width: 22, height: 22)
                        .background(RoundedRectangle(cornerRadius: 6).fill(p.icon.1.gradient))
                }
                .tag(p)
            }
            .navigationSplitViewColumnWidth(190)
        } detail: {
            Group {
                switch pane ?? .general {
                case .general: SettingsGeneral()
                case .appearance: SettingsAppearance()
                case .trading: SettingsTrading()
                case .keys: SettingsKeys()
                case .about: SettingsAbout()
                }
            }
            .formStyle(.grouped)
            .navigationTitle(L((pane ?? .general).title))
        }
        .toolbar {
            ToolbarItem(placement: .confirmationAction) { Button(L("Done")) { dismiss() } }
        }
        .frame(width: 800, height: 580)
    }
}

// MARK: helpers

@MainActor enum SettingsIO {
    /// Codable -> JSON object for `Store.call` args.
    static func json<T: Encodable>(_ v: T) -> Any {
        (try? JSONSerialization.jsonObject(with: JSONEncoder().encode(v))) ?? [:]
    }
    static func setPrefs(_ p: Prefs) { Store.shared.call("set_prefs", ["prefs": json(p)]) }
    static func setRoute(_ r: RoutePolicy) { Store.shared.call("set_route", ["route": json(r)]) }

    /// Binding into one Prefs / RoutePolicy field, written through with the whole object.
    static func pref<V>(_ kp: WritableKeyPath<Prefs, V>) -> Binding<V> {
        Binding(get: { Store.shared.state.prefs[keyPath: kp] }, set: { var p = Store.shared.state.prefs; p[keyPath: kp] = $0; setPrefs(p) })
    }
    static func route<V>(_ kp: WritableKeyPath<RoutePolicy, V>) -> Binding<V> {
        Binding(get: { Store.shared.state.route[keyPath: kp] }, set: { var r = Store.shared.state.route; r[keyPath: kp] = $0; setRoute(r) })
    }
}

/// Section footer text (explanations live here, not inline under controls).
struct SettingsFooter: View {
    let text: String
    init(_ text: String) { self.text = text }
    var body: some View { Text(text).font(.footnote).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true) }
}

/// Number field with a unit suffix, committed on Return / focus loss.
struct SettingsNumber: View {
    let title: String
    @Binding var value: Double
    let unit: String
    var digits = 0...2
    var body: some View {
        LabeledContent(title) {
            HStack(spacing: 6) {
                TextField(title, value: $value, format: .number.precision(.fractionLength(digits)))
                    .labelsHidden().multilineTextAlignment(.trailing).monospacedDigit().frame(width: 80)
                Text(unit).foregroundStyle(.secondary).frame(width: 36, alignment: .leading)
            }
        }
    }
}

// MARK: General

struct SettingsGeneral: View {
    @Environment(Store.self) private var store

    var body: some View {
        Form {
            Section {
                Picker(L("Language"), selection: Binding(get: { store.state.lang_zh }, set: { store.call("set_lang", ["zh": $0]) })) {
                    Text("English").tag(false)
                    Text(L("Simplified Chinese")).tag(true)
                }
            } footer: { SettingsFooter(L("Interface language. Numbers, symbols and codes are not translated.")) }
        }
    }
}

// MARK: Appearance

struct SettingsAppearance: View {
    @Environment(Store.self) private var store
    static let accents: [(String, [Int])] = [("Blue", [76, 158, 235]), ("Gold", [240, 185, 11]), ("Violet", [155, 140, 240]),
                                             ("Teal", [46, 196, 182]), ("Orange", [245, 138, 61]), ("Pink", [224, 91, 196])]

    var body: some View {
        let p = store.state.prefs
        Form {
            Section {
                LabeledContent(L("Accent color")) {
                    HStack(spacing: 6) {
                        ForEach(Self.accents, id: \.0) { name, rgb in
                            Button { var q = store.state.prefs; q.accent = rgb; SettingsIO.setPrefs(q) } label: {
                                Circle().fill(rgbColor(rgb)).frame(width: 14, height: 14)
                                    .overlay(Circle().strokeBorder(.primary, lineWidth: p.accent == rgb ? 2 : 0).padding(-3))
                                    .padding(3)
                            }
                            .buttonStyle(.plain).help(L(name)).accessibilityLabel(L(name))
                        }
                        ColorPicker(L("Custom"), selection: accentBinding, supportsOpacity: false).labelsHidden().help(L("Custom"))
                    }
                }
                Picker(L("Price colors"), selection: SettingsIO.pref(\.red_up)) {
                    Text(L("Green up, red down")).tag(false)
                    Text(L("Red up, green down")).tag(true)
                }
            } header: { Text(L("Colors")) } footer: {
                SettingsFooter(L("The accent marks selection and highlights in the chart and book. Price colors apply to candles, the order book, PnL and buy/sell buttons."))
            }
            Section {
                LabeledContent(L("Interface size")) {
                    Slider(value: SettingsIO.pref(\.zoom), in: 0.85...1.3, step: 0.05) {
                        Text(L("Interface size"))
                    } minimumValueLabel: { Image(systemName: "textformat.size.smaller") } maximumValueLabel: { Image(systemName: "textformat.size.larger") }
                    .labelsHidden().frame(width: 220)
                    Text("\(Int((p.zoom * 100).rounded()))%").monospacedDigit().foregroundStyle(.secondary).frame(width: 44, alignment: .trailing)
                }
                LabeledContent(L("Corner radius")) {
                    Slider(value: Binding(get: { Double(store.state.prefs.radius) }, set: { var q = store.state.prefs; q.radius = Int($0.rounded()); if q != store.state.prefs { SettingsIO.setPrefs(q) } }),
                           in: 0...12, step: 1) { Text(L("Corner radius")) }
                        .labelsHidden().frame(width: 220)
                    Text("\(p.radius) pt").monospacedDigit().foregroundStyle(.secondary).frame(width: 44, alignment: .trailing)
                }
            } header: { Text(L("Chart and book")) } footer: {
                SettingsFooter(L("Scales the chart, order book and option chain. The rest of the window follows the system text size."))
            }
            Section {
                Button(L("Restore Default Appearance")) {
                    let d = Prefs()
                    var q = store.state.prefs
                    q.accent = d.accent; q.red_up = d.red_up; q.zoom = d.zoom; q.radius = d.radius
                    SettingsIO.setPrefs(q)
                }
                .disabled(p.accent == Prefs().accent && !p.red_up && p.zoom == 1 && p.radius == Prefs().radius)
            }
        }
    }

    private func rgbColor(_ c: [Int]) -> Color {
        c.count == 3 ? Color(red: Double(c[0]) / 255, green: Double(c[1]) / 255, blue: Double(c[2]) / 255) : .accentColor
    }

    private var accentBinding: Binding<Color> {
        Binding(get: { rgbColor(store.state.prefs.accent) }, set: { c in
            guard let n = NSColor(c).usingColorSpace(.sRGB) else { return }
            var q = store.state.prefs
            q.accent = [n.redComponent, n.greenComponent, n.blueComponent].map { Swift.min(255, Swift.max(0, Int(($0 * 255).rounded()))) }
            if q != store.state.prefs { SettingsIO.setPrefs(q) }
        })
    }
}

// MARK: Trading

struct SettingsTrading: View {
    @Environment(Store.self) private var store
    static let defaultFees = [0.055, 0.02]

    var body: some View {
        let s = store.state
        let r = s.route
        Form {
            Section {
                Picker(L("Routing"), selection: SettingsIO.route(\.smart)) {
                    Text(L("Smart")).tag(true)
                    Text(L("Fixed venue")).tag(false)
                }
                .pickerStyle(.segmented)
                Picker(L(r.smart ? "Preferred venue" : "Trading venue"), selection: Binding(get: { s.trade.venue }, set: { store.call("set_trade_venue", ["ex": $0]) })) {
                    ForEach(s.tradable, id: \.self) { ex in
                        let verified = s.keys.first { $0.ex == ex }?.verified ?? true
                        Text(verified ? ex : "\(ex) (\(L("untested")))").tag(ex)
                    }
                }
            } header: { Text(L("Order routing")) } footer: {
                SettingsFooter(r.smart
                    ? L("Smart routing picks venues by fee-adjusted price from each venue's own book and can split large orders. The preferred venue drives the order panel's book and limits.")
                    : L("Every order goes to the trading venue."))
            }

            Section {
                Toggle(L("Allow split orders"), isOn: SettingsIO.route(\.allow_split))
                Stepper(value: SettingsIO.route(\.max_legs), in: 1...4) {
                    LabeledContent(L("Maximum legs"), value: "\(r.max_legs)")
                }
                .disabled(!r.allow_split)
                SettingsNumber(title: L("Split when saving at least"), value: SettingsIO.route(\.split_bps), unit: "bp").disabled(!r.allow_split)
                SettingsNumber(title: L("Minimum notional to split"), value: SettingsIO.route(\.min_split_notional), unit: "USDT").disabled(!r.allow_split)
                SettingsNumber(title: L("Maximum slippage"), value: SettingsIO.route(\.max_slip_bps), unit: "bp")
                SettingsNumber(title: L("Maximum venue dispersion"), value: SettingsIO.route(\.max_disp_bps), unit: "bp")
                SettingsNumber(title: L("Ignore books older than"), value: Binding(get: { Double(r.max_stale_ms) }, set: { v in
                    var q = store.state.route; q.max_stale_ms = Int(v.rounded()); SettingsIO.setRoute(q)
                }), unit: "ms", digits: 0...0)
            } header: { Text(L("Smart routing")) } footer: {
                SettingsFooter(L("Each leg is priced on its venue's real bid/ask, never the composite index. Venues whose book is stale or too far from the others are skipped."))
            }
            .disabled(!r.smart)

            Section {
                Toggle(L("Confirm before sending orders"), isOn: SettingsIO.pref(\.confirm))
            } header: { Text(L("Orders")) } footer: { SettingsFooter(L("Shows an order summary before it is sent.")) }

            Section {
                ForEach(s.tradable, id: \.self) { ex in feeRow(ex) }
            } header: { Text(L("Fees (your tier)")) } footer: {
                SettingsFooter(L("Taker / maker in percent per fill. Used for fee estimates and routing; enter your VIP tier's rates."))
            }
        }
    }

    private func feeRow(_ ex: String) -> some View {
        func bind(_ i: Int) -> Binding<Double> {
            Binding(get: { (store.state.prefs.fees[ex] ?? Self.defaultFees)[safe: i] }, set: { v in
                var p = store.state.prefs
                var cur = p.fees[ex] ?? Self.defaultFees
                while cur.count < 2 { cur.append(0) }
                cur[i] = Swift.min(0.2, Swift.max(-0.05, v))
                p.fees[ex] = cur
                SettingsIO.setPrefs(p)
            })
        }
        return LabeledContent {
            HStack(spacing: 6) {
                feeField(L("Taker"), bind(0))
                Text("/").foregroundStyle(.tertiary)
                feeField(L("Maker"), bind(1))
                Text("%").foregroundStyle(.secondary)
            }
        } label: {
            Label { Text(ex) } icon: { VenueIcon(ex: ex, size: 16) }
        }
    }

    private func feeField(_ title: String, _ v: Binding<Double>) -> some View {
        TextField(title, value: v, format: .number.precision(.fractionLength(0...4)))
            .labelsHidden().multilineTextAlignment(.trailing).monospacedDigit().frame(width: 64).help(title)
    }
}

extension Array where Element == Double {
    subscript(safe i: Int) -> Double { indices.contains(i) ? self[i] : 0 }
}

// MARK: About

struct SettingsAbout: View {
    var body: some View {
        let home = FileManager.default.homeDirectoryForCurrentUser.path
        let version = Bundle.main.infoDictionary?["CFBundleShortVersionString"] as? String ?? "0.1.0"
        Form {
            Section {
                HStack(spacing: 14) {
                    Image(nsImage: NSApp.applicationIconImage).resizable().frame(width: 56, height: 56)
                    VStack(alignment: .leading, spacing: 3) {
                        Text("Depth").font(.title3.weight(.semibold))
                        Text("\(L("Version")) \(version)").monospacedDigit().foregroundStyle(.secondary)
                    }
                }
                .padding(.vertical, 4)
            } footer: { SettingsFooter(L("Local multi-exchange trading terminal. Runs entirely on this Mac: no relay, no server.")) }
            Section {
                pathRow(L("Settings"), "\(home)/Library/Application Support/TerminalOne")
                pathRow(L("Cache"), "\(home)/Library/Caches/TerminalOne")
                LabeledContent(L("API keys"), value: L("macOS Keychain (terminal-one)"))
            } header: { Text(L("Data")) }
        }
    }

    private func pathRow(_ title: String, _ path: String) -> some View {
        LabeledContent(title) {
            HStack(spacing: 6) {
                Text(path.replacingOccurrences(of: FileManager.default.homeDirectoryForCurrentUser.path, with: "~"))
                    .textSelection(.enabled).lineLimit(1).truncationMode(.middle).foregroundStyle(.secondary)
                Button { NSWorkspace.shared.selectFile(nil, inFileViewerRootedAtPath: path) } label: { Image(systemName: "arrow.right.circle.fill") }
                    .buttonStyle(.borderless).help(L("Show in Finder"))
                    .disabled(!FileManager.default.fileExists(atPath: path))
            }
        }
    }
}
