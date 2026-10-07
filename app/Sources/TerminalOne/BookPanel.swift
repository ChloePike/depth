import SwiftUI

/// Book column header (native) laid over the Rust-drawn rows; the Venues and Quant pages cover
/// the rows with their own opaque view. The rows themselves render at display rate in egui:
/// polling them into SwiftUI 20 times a second cost a third of the main thread.
struct BookPanel: View {
    @Environment(Store.self) private var store
    /// 0 book, 1 trades, 2 venues, 3 quant (T1_BOOK_PANE preselects, for screenshots)
    @State private var pane = Int(ProcessInfo.processInfo.environment["T1_BOOK_PANE"] ?? "") ?? 0
    @AppStorage("t1.venueWindow") private var window = 60

    var body: some View {
        let b = store.state.book
        VStack(spacing: 0) {
            VStack(spacing: 0) {
                Picker(L("Book"), selection: Binding(get: { pane > 1 ? pane : (b.trades ? 1 : 0) }, set: { v in
                    pane = v
                    if v < 2 { store.call("book", ["trades": v == 1]) }
                })) {
                    Text(L("Book")).tag(0)
                    Text(L("Trades")).tag(1)
                    Text(L("Venues")).tag(2)
                    Text(L("Quant")).tag(3)
                }
                .pickerStyle(.segmented).labelsHidden()
                .padding(.horizontal, 10).padding(.top, 8)
                // second row: the active tab's options
                HStack(spacing: 6) {
                    Spacer(minLength: 0)
                    if pane == 2 {
                        Picker(L("Window"), selection: $window) {
                            Text("5m").tag(5); Text("1h").tag(60); Text("24h").tag(1440)
                        }
                        .pickerStyle(.menu).labelsHidden().fixedSize().help(L("Volume window"))
                    } else if pane < 2 {
                        ChartBar().bookRight(b)
                    }
                }
                .padding(.horizontal, 10).frame(height: 28)
            }
            .controlSize(.small)
            .frame(height: 62)
            .background(Color(nsColor: .textBackgroundColor))
            switch pane {
            case 2: VenueShare(window: window).background(Color(nsColor: .textBackgroundColor))
            case 3: QuantPanel().background(Color(nsColor: .textBackgroundColor))
            // the egui rows below take the clicks (click-to-fill)
            default: Color.clear.allowsHitTesting(false)
            }
        }
    }
}

extension Color {
    init(hexString: String) {
        let h = UInt32(hexString.trimmingCharacters(in: CharacterSet(charactersIn: "#")), radix: 16) ?? 0x888888
        self.init(hex: h)
    }
}

struct VenueShareRow: Decodable, Identifiable {
    var ex: String; var color: String; var bid_usd: Double; var ask_usd: Double; var vol_usd: Double; var buy_usd: Double; var sell_usd: Double
    var id: String { ex }
}
struct VenueShareReply: Decodable { var rows: [VenueShareRow] }

/// Each venue's share of resting depth (+-1% of its mid) and of traded volume over the window.
struct VenueShare: View {
    let window: Int
    @State private var rows: [VenueShareRow] = []

    var body: some View {
        let depth = rows.reduce(0) { $0 + $1.bid_usd + $1.ask_usd }
        let vol = rows.reduce(0) { $0 + $1.vol_usd }
        ScrollView {
            VStack(alignment: .leading, spacing: 14) {
                section(L("Volume share"), "\(L("Traded")) \(window >= 1440 ? "24h" : window >= 60 ? "1h" : "\(window)m") · $\(Fmt.big(vol))",
                        rows.sorted { $0.vol_usd > $1.vol_usd }, total: vol, value: \.vol_usd) { r in
                    let b = r.buy_usd + r.sell_usd
                    return b > 0 ? String(format: "%@ %.0f%%", L("buy"), r.buy_usd / b * 100) : ""
                }
                section(L("Book share"), "\(L("Depth ±1%")) · $\(Fmt.big(depth))",
                        rows.sorted { $0.bid_usd + $0.ask_usd > $1.bid_usd + $1.ask_usd }, total: depth, value: { $0.bid_usd + $0.ask_usd }) { r in
                    let d = r.bid_usd + r.ask_usd
                    return d > 0 ? String(format: "%@ %.0f%%", L("bid"), r.bid_usd / d * 100) : ""
                }
            }
            .padding(10)
        }
        .scrollIndicators(.never)
        .task(id: window) {
            while !Task.isCancelled {
                if let r = Store.shared.query("venue_share", ["minutes": window], as: VenueShareReply.self) { rows = r.rows }
                try? await Task.sleep(for: .seconds(1))
            }
        }
    }

    private func section(_ title: String, _ sub: String, _ list: [VenueShareRow], total: Double, value: @escaping (VenueShareRow) -> Double,
                         extra: @escaping (VenueShareRow) -> String) -> some View {
        VStack(alignment: .leading, spacing: 7) {
            HStack(alignment: .firstTextBaseline) {
                Text(title).font(.subheadline.weight(.semibold))
                Spacer()
                Text(sub).font(.caption.monospacedDigit()).foregroundStyle(.secondary)
            }
            // one stacked bar of every venue, then a row per venue
            GeometryReader { g in
                HStack(spacing: 1) {
                    ForEach(list) { r in
                        Rectangle().fill(Color(hexString: r.color)).frame(width: total > 0 ? max(0, g.size.width * value(r) / total - 1) : 0)
                    }
                }
            }
            .frame(height: 6).clipShape(.capsule)
            ForEach(list) { r in
                let share = total > 0 ? value(r) / total : 0
                HStack(spacing: 6) {
                    VenueIcon(ex: r.ex, size: 13)
                    Text(r.ex).font(.caption)
                    Spacer(minLength: 4)
                    Text(extra(r)).font(.caption2.monospacedDigit()).foregroundStyle(.tertiary)
                    Text(String(format: "%.1f%%", share * 100)).font(.caption.monospacedDigit()).frame(width: 46, alignment: .trailing)
                }
                .help("$\(Fmt.big(value(r)))")
            }
        }
    }
}

extension Array where Element == VenueShareRow {
    func sorted(_ f: (VenueShareRow, VenueShareRow) -> Bool) -> [VenueShareRow] { self.sorted(by: f) }
}

/// Statistical models for the current market: volatility forecast, book / flow pressure, carry.
struct QuantPanel: View {
    @Environment(Store.self) private var store

    var body: some View {
        let q = store.state.quant
        ScrollView {
            VStack(alignment: .leading, spacing: 16) {
                group(L("Volatility model"), L("EWMA of 1m returns, 60 min half-life")) {
                    if let v = q.vol {
                        row(L("Expected move 1h"), String(format: "±%.2f%%", v.sigma_1h * 100), "\(Fmt.px(v.range_1h.first)) – \(Fmt.px(v.range_1h.last))")
                        row(L("Expected move 24h"), String(format: "±%.2f%%", v.sigma_24h * 100), "\(Fmt.px(v.range_24h.first)) – \(Fmt.px(v.range_24h.last))")
                        VStack(alignment: .leading, spacing: 4) {
                            HStack {
                                Text(L("Volatility regime")).foregroundStyle(.secondary)
                                Spacer()
                                Text(v.percentile < 0.2 ? L("Compressed") : v.percentile > 0.8 ? L("Expanded") : L("Normal"))
                                    .foregroundStyle(v.percentile > 0.8 ? Color.orange : v.percentile < 0.2 ? Color.accentColor : Color.primary)
                            }
                            ProgressView(value: v.percentile).tint(v.percentile > 0.8 ? .orange : .accentColor)
                            Text(String(format: L("%.0f%% of the last day's hours were calmer"), v.percentile * 100)).font(.caption2).foregroundStyle(.tertiary)
                        }
                    } else {
                        Text(L("Needs an hour of data")).foregroundStyle(.tertiary)
                    }
                }
                group(L("Pressure"), L("Book depth ±0.5% and 5 min aggressor flow; describes now, not a forecast")) {
                    let p = q.pressure
                    HStack {
                        Text(p > 20 ? L("Buyers in control") : p < -20 ? L("Sellers in control") : L("Balanced")).foregroundStyle(p > 20 ? T1.up : p < -20 ? T1.down : .primary)
                        Spacer()
                        Text(String(format: "%+.0f", p)).font(.title3.weight(.semibold).monospacedDigit()).foregroundStyle(p >= 0 ? T1.up : T1.down)
                    }
                    GeometryReader { g in
                        ZStack(alignment: .leading) {
                            Capsule().fill(.fill.tertiary)
                            Capsule().fill(p >= 0 ? T1.up : T1.down)
                                .frame(width: g.size.width / 2 * abs(p) / 100)
                                .offset(x: p >= 0 ? g.size.width / 2 : g.size.width / 2 - g.size.width / 2 * abs(p) / 100)
                            Rectangle().fill(.secondary).frame(width: 1).offset(x: g.size.width / 2)
                        }
                    }
                    .frame(height: 6)
                    row(L("Bids / asks ±0.5%"), "$\(Fmt.big(q.book_bid_usd)) / $\(Fmt.big(q.book_ask_usd))", nil)
                }
                group(L("Carry"), L("Predicted funding, annualized; basis = perp over spot")) {
                    ForEach(q.carry.sorted { ($0.funding_apr ?? 0) > ($1.funding_apr ?? 0) }) { c in
                        HStack(spacing: 6) {
                            VenueIcon(ex: c.ex, size: 13)
                            Text(c.ex)
                            Spacer()
                            Text(c.funding_apr.map { String(format: "%+.1f%%", $0) } ?? "–").foregroundStyle((c.funding_apr ?? 0) >= 0 ? T1.up : T1.down).frame(width: 64, alignment: .trailing)
                            Text(c.basis_bps.map { String(format: "%+.1f bp", $0) } ?? "–").foregroundStyle(.secondary).frame(width: 64, alignment: .trailing)
                        }
                        .font(.caption.monospacedDigit())
                    }
                }
            }
            .padding(12)
        }
        .scrollIndicators(.never)
    }

    private func group<C: View>(_ title: String, _ note: String, @ViewBuilder _ c: () -> C) -> some View {
        VStack(alignment: .leading, spacing: 8) {
            Text(title).font(.subheadline.weight(.semibold))
            c()
            Text(note).font(.caption2).foregroundStyle(.tertiary)
        }
    }

    private func row(_ k: String, _ v: String, _ sub: String?) -> some View {
        HStack(alignment: .firstTextBaseline) {
            Text(k).foregroundStyle(.secondary)
            Spacer()
            VStack(alignment: .trailing, spacing: 1) {
                Text(v).monospacedDigit()
                if let sub { Text(sub).font(.caption2.monospacedDigit()).foregroundStyle(.tertiary) }
            }
        }
        .font(.callout)
    }
}
