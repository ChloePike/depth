import SwiftUI

/// This symbol's position on the trade venue as a Form section: size, prices, uPnL / ROE, its
/// TP/SL, and actions.
struct OrderPositionSection: View {
    let p: PositionRow
    let tpsl: [TpSlRow]
    let onMarket: () -> Void
    let onLimit: () -> Void
    let onTpSl: () -> Void

    var body: some View {
        let side = p.isLong ? OrderColor.up : OrderColor.down
        let pnl = p.upnl >= 0 ? OrderColor.up : OrderColor.down
        let margin = p.margin > 0 ? p.margin : p.entry * p.qty / max(p.lev, 1)
        PanelSection {
            LabeledContent {
                VStack(alignment: .trailing, spacing: 0) {
                    Text(Fmt.signed(p.upnl)).font(.headline.monospacedDigit()).foregroundStyle(pnl)
                    Text(margin > 0 ? String(format: "%+.2f%%", p.upnl / margin * 100) : "–").font(.caption.monospacedDigit()).foregroundStyle(pnl)
                }
                .help(L("Unrealized PnL (USDT) and return on margin"))
            } label: {
                HStack(spacing: 6) {
                    Text(p.isLong ? L("Long") : L("Short")).fontWeight(.semibold).foregroundStyle(side)
                    if p.lev > 0 { Text(String(format: "%gx", p.lev)).monospacedDigit().foregroundStyle(.secondary) }
                    VenueIcon(ex: p.ex, size: 12)
                }
            }
            row(L("Size"), "\(Fmt.qty(p.qty)) \(p.base)")
            row(L("Entry / Mark"), "\(Fmt.px(p.entry)) / \(Fmt.px(p.mark))")
            row(L("Liq. price"), p.liq.map { Fmt.px($0) } ?? "–", .orange)
                .help(p.liq == nil && Store.shared.state.binance_pm ? L("Portfolio margin liquidates on the account's uniMMR; there is no per-position liquidation price.") : L("Liquidation price"))
            row(L("Margin"), "\(Fmt.usd(margin)) USDT")
            row(L("TP / SL"), summary)
            HStack(spacing: 6) {
                Button(role: .destructive, action: onMarket) { Text(L("Market")).frame(maxWidth: .infinity) }
                    .help(L("Close the whole position at market (asks for confirmation)"))
                Button(action: onLimit) { Text(L("Limit")).frame(maxWidth: .infinity) }
                    .help(L("Fill the form with a limit close at the venue's best price"))
                Button(action: onTpSl) { Text(L("TP/SL")).frame(maxWidth: .infinity) }
                    .help(L("Whole-position or staged take-profit / stop-loss"))
            }
            .buttonStyle(.bordered)
        } header: {
            Text("\(L("Position")) · \(p.symbol)")
        }
    }

    private var summary: String {
        let tp = tpsl.filter(\.take_profit).map(\.trigger_px), sl = tpsl.filter { !$0.take_profit }.map(\.trigger_px)
        func one(_ v: [Double]) -> String { v.isEmpty ? "–" : Fmt.px(v[0]) + (v.count > 1 ? " +\(v.count - 1)" : "") }
        return "\(one(tp)) / \(one(sl))"
    }

    private func row(_ k: String, _ v: String, _ c: Color? = nil) -> some View {
        LabeledContent(k) { Text(v).monospacedDigit().foregroundStyle(c ?? .primary) }
    }
}

/// TP/SL for an existing position: whole position (one call) or staged levels (one `set_tpsl`
/// with `partial_qty` per level). Lists the position's live TP/SL with cancel buttons.
struct OrderTpSlSheet: View {
    let p: PositionRow
    @Environment(\.dismiss) private var dismiss
    @FocusState private var focus: OrderFieldID?
    @State private var partial = false
    @State private var tp = ""
    @State private var sl = ""
    @State private var trigLast = false
    @State private var levels = [Level(tp: true, amount: "50"), Level(tp: false, amount: "100")]
    @State private var error: String?
    @State private var cancelRow: TpSlRow?

    struct Level: Identifiable {
        let id = UUID()
        var tp: Bool
        var price = ""
        var amount = ""
        /// amount is % of the position (else base units)
        var pct = true
    }

    private var store: Store { Store.shared }
    /// live row (mark, size move while the sheet is open)
    private var pos: PositionRow { store.state.positions.first { $0.id == p.id } ?? p }
    private var existing: [TpSlRow] { store.state.tpsl.filter { $0.ex == p.ex && $0.symbol == p.symbol && $0.pos == p.side } }
    private var step: Double? { p.symbol == store.state.trade.symbol ? store.state.trade.step : nil }
    private var sign: Double { pos.isLong ? 1 : -1 }

    var body: some View {
        VStack(spacing: 0) {
            Form {
                Section {
                    LabeledContent {
                        Text(pos.isLong ? L("Long") : L("Short")).fontWeight(.bold).foregroundStyle(pos.isLong ? OrderColor.up : OrderColor.down)
                    } label: {
                        HStack(spacing: 8) { VenueIcon(ex: p.ex, size: 16); Text(p.symbol).font(.headline.weight(.semibold)); Text(L("TP/SL")).foregroundStyle(.secondary) }
                    }
                    LabeledContent(L("Size")) { Text("\(Fmt.qty(pos.qty)) \(pos.base)").monospacedDigit() }
                    LabeledContent(L("Entry / Mark")) { Text("\(Fmt.px(pos.entry)) / \(Fmt.px(pos.mark))").monospacedDigit() }
                    LabeledContent(L("Liq. price")) { Text(Fmt.px(pos.liq)).monospacedDigit().foregroundStyle(Color.orange) }
                }
                Section {
                    Picker(L("Mode"), selection: $partial) {
                        Text(L("Entire position")).tag(false)
                        Text(L("Partial (staged)")).tag(true)
                    }
                    .pickerStyle(.segmented)
                    Picker(L("Trigger"), selection: $trigLast) {
                        Text(L("Mark")).tag(false)
                        Text(L("Last")).tag(true)
                    }
                    .pickerStyle(.segmented)
                }
                if partial { staged } else { whole }
                if !existing.isEmpty {
                    Section(L("Active")) { ForEach(existing) { row($0) } }
                }
                if let e = error {
                    Section { OrderNote(text: e, color: OrderColor.down, icon: "xmark.octagon.fill") }
                }
            }
            .formStyle(.grouped)
            HStack(spacing: 10) {
                Spacer()
                Button(L("Close")) { dismiss() }.keyboardShortcut(.cancelAction)
                Button(L("Confirm")) { apply() }.buttonStyle(.borderedProminent).keyboardShortcut(.defaultAction)
            }
            .controlSize(.large)
            .padding([.horizontal, .bottom], 20)
        }
        .frame(width: 480, height: 600)
        .confirmationDialog(L("Cancel this TP/SL?"), isPresented: Binding(get: { cancelRow != nil }, set: { if !$0 { cancelRow = nil } }), presenting: cancelRow) { r in
            Button(L("Cancel TP/SL"), role: .destructive) {
                if store.call("cancel_tpsl", ["ex": r.ex, "id": r.id])?["ok"] as? Bool != true { error = store.lastError ?? L("Cancel failed") }
            }
            Button(L("Keep"), role: .cancel) {}
        } message: { r in
            Text("\(r.take_profit ? L("TP") : L("SL")) \(Fmt.px(r.trigger_px))")
        }
    }

    // MARK: whole position

    private var whole: some View {
        Section {
            TextField(L("Take profit"), text: $tp, prompt: Text("USDT")).monospacedDigit().multilineTextAlignment(.trailing)
                .focused($focus, equals: .wholeTp).onSubmit(apply)
            pnlLine(OrderNum.parse(tp), pos.qty)
            TextField(L("Stop loss"), text: $sl, prompt: Text("USDT")).monospacedDigit().multilineTextAlignment(.trailing)
                .focused($focus, equals: .wholeSl).onSubmit(apply)
            pnlLine(OrderNum.parse(sl), pos.qty)
        } footer: {
            Text(L("Closes the whole position at market when triggered, including size added later.")).font(.caption).foregroundStyle(.secondary)
        }
    }

    @ViewBuilder private func pnlLine(_ px: Double?, _ q: Double) -> some View {
        if let px, px > 0 {
            let v = (px - pos.entry) * q * sign
            OrderKV(k: L("Est. PnL"), v: "\(Fmt.signed(v)) USDT (\(String(format: "%+.2f%%", (px / max(pos.entry, 1e-12) - 1) * sign * 100)))", color: v >= 0 ? OrderColor.up : OrderColor.down)
        }
    }

    // MARK: staged levels

    private var staged: some View {
        Section {
            ForEach($levels) { $l in
                let i = levels.firstIndex { $0.id == l.id } ?? 0
                VStack(spacing: 3) {
                    HStack(spacing: 6) {
                        Picker("", selection: $l.tp) { Text(L("TP")).tag(true); Text(L("SL")).tag(false) }
                            .pickerStyle(.segmented).labelsHidden().controlSize(.small).frame(width: 74)
                        TextField(L("Price"), text: $l.price, prompt: Text(L("Price"))).labelsHidden().textFieldStyle(.roundedBorder)
                            .monospacedDigit().multilineTextAlignment(.trailing).focused($focus, equals: .level(i * 2)).onSubmit(apply)
                        TextField(L("Size"), text: $l.amount, prompt: Text(L("Size"))).labelsHidden().textFieldStyle(.roundedBorder)
                            .monospacedDigit().multilineTextAlignment(.trailing).frame(width: 70).focused($focus, equals: .level(i * 2 + 1)).onSubmit(apply)
                        Picker("", selection: $l.pct) { Text("%").tag(true); Text(pos.base).tag(false) }
                            .labelsHidden().pickerStyle(.menu).controlSize(.small).fixedSize()
                            .onChange(of: l.pct) { l.amount = "" }
                            .help(L("Size as % of the position, or in base units"))
                        Button { levels.removeAll { $0.id == l.id } } label: { Image(systemName: "minus.circle.fill") }
                            .buttonStyle(.borderless).foregroundStyle(.secondary)
                    }
                    if let px = OrderNum.parse(l.price), let q = qtyOf(l) {
                        OrderKV(k: "\(Fmt.qty(q)) \(pos.base)", v: "\(Fmt.signed((px - pos.entry) * q * sign)) USDT", color: (px - pos.entry) * sign >= 0 ? OrderColor.up : OrderColor.down)
                            .padding(.leading, 80)
                    }
                }
            }
            if levels.count < 10 {
                HStack {
                    Button { levels.append(Level(tp: true)) } label: { Label(L("Take-profit level"), systemImage: "plus") }
                    Button { levels.append(Level(tp: false)) } label: { Label(L("Stop-loss level"), systemImage: "plus") }
                    Spacer()
                }
                .buttonStyle(.bordered).controlSize(.small)
            }
        } footer: {
            Text(L("Each level closes its size when triggered. Take-profit levels and stop levels each add up to at most the position.")).font(.caption).foregroundStyle(.secondary)
        }
    }

    private func qtyOf(_ l: Level) -> Double? {
        guard let a = OrderNum.parse(l.amount), a > 0 else { return nil }
        let q = l.pct ? pos.qty * min(a, 100) / 100 : a
        return OrderNum.parse(OrderNum.qty(q, step: step))
    }

    // MARK: existing

    private func row(_ r: TpSlRow) -> some View {
        // a long's TP triggers at or above, its SL at or below; reversed for shorts
        let up = r.take_profit == pos.isLong
        return LabeledContent {
            HStack(spacing: 8) {
                Text(r.qty.map { "\(Fmt.qty($0)) \(pos.base)" } ?? L("Entire position")).monospacedDigit().foregroundStyle(.secondary)
                Button(L("Cancel"), role: .destructive) { cancelRow = r }
                .buttonStyle(.bordered).controlSize(.small)
            }
        } label: {
            HStack(spacing: 8) {
                Text(r.take_profit ? L("TP") : L("SL")).fontWeight(.bold).foregroundStyle(r.take_profit ? OrderColor.up : OrderColor.down)
                Text("\(r.trigger == "last" ? L("Last") : L("Mark")) \(up ? "≥" : "≤") \(Fmt.px(r.trigger_px))").monospacedDigit()
            }
        }
    }

    // MARK: send

    private func apply() {
        error = nil
        let long = pos.isLong, ref = pos.mark, refName = L("the mark price")
        let base: [String: Any] = ["ex": p.ex, "symbol": p.symbol, "pos": p.side, "trigger": trigLast ? "last" : "mark"]
        var calls: [[String: Any]] = []
        if !partial {
            let tpV = OrderNum.parse(tp), slV = OrderNum.parse(sl)
            guard tpV != nil || slV != nil else { error = L("Enter a take-profit or stop-loss price"); return }
            if let e = orderTpSlCheck(long: long, ref: ref, refName: refName, tp: tpV, sl: slV) { error = e; return }
            var a = base
            if let tpV { a["tp"] = tpV }
            if let slV { a["sl"] = slV }
            calls = [a]
        } else {
            guard !levels.isEmpty else { error = L("Add at least one level"); return }
            var sum = (tp: 0.0, sl: 0.0)
            for (i, l) in levels.enumerated() {
                let tag = "\(L("Level")) \(i + 1): "
                guard let px = OrderNum.parse(l.price) else { error = tag + L("enter a price"); return }
                guard let q = qtyOf(l), q > 0 else { error = tag + L("size is zero after rounding"); return }
                if let e = orderTpSlCheck(long: long, ref: ref, refName: refName, tp: l.tp ? px : nil, sl: l.tp ? nil : px) { error = tag + e; return }
                if l.tp { sum.tp += q } else { sum.sl += q }
                var a = base
                a[l.tp ? "tp" : "sl"] = px
                a["partial_qty"] = q
                calls.append(a)
            }
            let cap = pos.qty * (1 + 1e-9)
            if sum.tp > cap { error = L("Take-profit levels add up to more than the position"); return }
            if sum.sl > cap { error = L("Stop-loss levels add up to more than the position"); return }
        }
        for (i, a) in calls.enumerated() where store.call("set_tpsl", a)?["ok"] as? Bool != true {
            error = "\(i) / \(calls.count) \(L("sent")). \(store.lastError ?? L("failed"))"
            return
        }
        dismiss()
    }
}
