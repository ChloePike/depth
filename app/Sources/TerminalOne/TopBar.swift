import SwiftUI

/// Window toolbar (unified, Liquid Glass on macOS 26): pair picker + composite price, market mode,
/// settings. Applied to RootView with `.toolbar { TopToolbar() }`.
struct TopToolbar: ToolbarContent {
    @Bindable var store: Store
    @State private var picker = false


    var body: some ToolbarContent {
        let s = store.state
        ToolbarItem(placement: .navigation) {
            Button { picker.toggle() } label: {
                HStack(spacing: 6) {
                    CoinIcon(base: s.base, size: 18)
                    Text("\(s.base)/USDT").font(.headline)
                    Image(systemName: "chevron.down").font(.caption2.weight(.semibold)).foregroundStyle(.secondary)
                }
            }
            .help(L("Choose pair") + " (⌘K)")
            .onReceive(NotificationCenter.default.publisher(for: .init("T1OpenPicker"))) { _ in picker = true }
            // T1_PICKER=1: screenshot runs open the picker
            .task { if ProcessInfo.processInfo.environment["T1_PICKER"] != nil { try? await Task.sleep(for: .seconds(4)); picker = true } }
            .popover(isPresented: $picker, arrowEdge: .bottom) {
                SymbolPicker(current: s.base) { picker = false }.environment(store)
            }
        }
        ToolbarItem(placement: .navigation) {
            TopPrice(price: s.header.price)
        }
        .sharedBackgroundVisibility(.hidden)
        ToolbarItem(placement: .principal) {
            Picker(L("Market"), selection: Binding(get: { s.mode }, set: { m in store.call("set_mode", ["mode": m]) })) {
                ForEach(Pairs.modes, id: \.id) { Text(L($0.label)).tag($0.id) }
            }
            .pickerStyle(.segmented)
            .labelsHidden()
            .fixedSize()
        }
        ToolbarItem(placement: .primaryAction) { MarketClock() }
        ToolbarItem(placement: .primaryAction) {
            Button { NotificationCenter.default.post(name: .init("T1OpenSettings"), object: nil) } label: {
                Label(L("Settings"), systemImage: "gearshape")
            }
            .help(L("Settings"))
        }
    }
}

/// Composite index price with a direction arrow; flashes on every change.
private struct TopPrice: View {
    let price: Double?
    @State private var dir = 0
    @State private var flash = false

    var body: some View {
        let col: Color = dir > 0 ? T1.up : dir < 0 ? T1.down : .primary
        HStack(spacing: 3) {
            Text(Fmt.px(price)).font(.title3.weight(.semibold).monospacedDigit())
            Image(systemName: dir < 0 ? "arrow.down" : "arrow.up").font(.caption.weight(.bold)).opacity(dir == 0 ? 0 : 1)
        }
        .foregroundStyle(col)
        .padding(.horizontal, 6).padding(.vertical, 2)
        .background(col.opacity(flash ? 0.22 : 0), in: .rect(cornerRadius: 6))
        .help(L("Composite index (volume-weighted venue mids); reference only, orders use the venue's own book"))
        // fixed box: a tick must not resize the toolbar (that re-lays out the whole window)
        .frame(width: 150, alignment: .leading)
        .onChange(of: price) { old, new in
            guard let o = old, let n = new, n != o else { return }
            dir = n > o ? 1 : -1
            flash = true
            withAnimation(.easeOut(duration: 0.45)) { flash = false }
        }
    }
}

/// Market strip under the toolbar (Binance-style): the order venue's mark / index / funding with
/// countdown, 24h range and volume, open interest; then the cross-venue extras.
struct TopStats: View {
    @Environment(Store.self) private var store

    var body: some View {
        let s = store.state
        let h = s.header
        let perp = s.mode == "Perp"
        ScrollView(.horizontal) {
            HStack(spacing: 24) {
                if perp {
                    TopStat(L("Mark"), Fmt.px(h.mark), .primary).help("\(h.venue ?? "") \(L("mark price"))")
                    TopStat(L("Index"), Fmt.px(h.index), .primary).help("\(h.venue ?? "") \(L("index price"))")
                    fundingStat(h)
                }
                TopStat(L("24h Change"), h.chg24.map { String(format: "%+.2f%%", $0) } ?? "–", sign(h.chg24))
                TopStat(L("24h High"), Fmt.px(h.high24), .primary)
                TopStat(L("24h Low"), Fmt.px(h.low24), .primary)
                TopStat("\(L("24h Vol")) (\(s.base))", Fmt.big(h.vol24_base), .primary).help(L("Summed across Binance, Bybit, OKX and Hyperliquid"))
                TopStat("\(L("24h Vol")) (USD)", Fmt.big(h.vol24_usd), .primary).help(L("Summed across Binance, Bybit, OKX and Hyperliquid"))
                if perp {
                    TopStat(L("Open Interest"), h.oi_usd.map { "$" + Fmt.big($0) } ?? "–", .primary, extra: h.oi.map { "\(Fmt.big($0)) \(s.base)" })
                        .help(L("Sum of every connected perp venue"))
                    TopStat(L("Pred. funding (all)"), bph(h.funding_pred_bph), sign(h.funding_pred_bph)).help(L("Open-interest-weighted across venues, per hour") + " · " + apr(h.funding_pred_bph))
                    TopStat(L("Basis"), h.basis_bps.map { String(format: "%+.2f bp", $0) } ?? "–", sign(h.basis_bps))
                }
                if s.mode != "Option" {
                    TopStat(L("CVD"), h.cvd.map { Fmt.signed($0, 1) } ?? "–", sign(h.cvd))
                }
            }
            .padding(.horizontal, 14)
            .padding(.vertical, 6)
        }
        .scrollIndicators(.never)
    }

    @ViewBuilder private func fundingStat(_ h: Header) -> some View {
        let iv = h.funding_interval_h.map { String(format: " (%gh)", $0) } ?? ""
        VStack(alignment: .leading, spacing: 1) {
            Text("\(L("Funding"))\(iv) / \(L("Countdown"))").font(.caption2).foregroundStyle(.secondary)
            HStack(spacing: 4) {
                Text(h.funding_rate.map { String(format: "%.4f%%", $0 * 100) } ?? "–").foregroundStyle(h.funding_rate.map { $0 >= 0 ? AnyShapeStyle(Color.orange) : AnyShapeStyle(T1.down) } ?? AnyShapeStyle(.primary))
                Text("/").foregroundStyle(.secondary)
                if let n = h.next_funding_ms { FundingCountdown(next: n) } else { Text("–") }
            }
            .font(.callout.weight(.medium).monospacedDigit())
        }
        .fixedSize()
        .help("\(h.venue ?? "") · \(L("rate per settlement"))")
    }

    private func bph(_ v: Double?) -> String { v.map { String(format: "%+.4f bp/h", $0) } ?? "–" }
    private func apr(_ v: Double?) -> String { v.map { String(format: "%@ %+.2f%%", L("APR"), $0 / 1e4 * 24 * 365 * 100) } ?? "" }
    private func sign(_ v: Double?) -> AnyShapeStyle {
        guard let v, v != 0 else { return AnyShapeStyle(.primary) }
        return AnyShapeStyle(v > 0 ? T1.up : T1.down)
    }
}

private struct TopStat: View {
    let caption: String, value: String, style: AnyShapeStyle, extra: String?

    init(_ caption: String, _ value: String, _ style: some ShapeStyle, extra: String? = nil) {
        self.caption = caption; self.value = value; self.style = AnyShapeStyle(style); self.extra = extra
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 1) {
            Text(caption).font(.caption2).foregroundStyle(.secondary)
            HStack(spacing: 5) {
                Text(value).foregroundStyle(style).frame(minWidth: 70, alignment: .leading)
                if let extra { Text(extra).foregroundStyle(.secondary) }
            }
            .font(.callout.weight(.medium).monospacedDigit())
        }
        .lineLimit(1)
        .fixedSize()
    }
}
