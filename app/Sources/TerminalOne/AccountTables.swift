import AppKit
import SwiftUI

// Account tables (native SwiftUI Table): positions, open orders, TP/SL, history, order log.

// MARK: shared cells

@MainActor func copy(_ s: String) {
    NSPasteboard.general.clearContents()
    NSPasteboard.general.setString(s, forType: .string)
}

/// Coin logo + symbol, venue logo + "Venue · Perp" underneath (or inline when `compact`).
struct AccountSymbolCell<Badge: View>: View {
    let ex: String
    let symbol: String
    var compact = true
    @ViewBuilder var badge: Badge

    var body: some View {
        let base = symbol.hasSuffix("USDT") ? String(symbol.dropLast(4)) : symbol
        HStack(spacing: 7) {
            ZStack(alignment: .bottomTrailing) {
                CoinIcon(base: base, size: compact ? 16 : 20)
                if compact { VenueIcon(ex: ex, size: 9).offset(x: 3, y: 2) }
            }
            if compact {
                Text(symbol).font(uiFont(12, .medium)).foregroundStyle(Color.primary)
                badge
            } else {
                VStack(alignment: .leading, spacing: 2) {
                    HStack(spacing: 6) {
                        Text(symbol).font(uiFont(12.5, .semibold)).foregroundStyle(Color.primary)
                        badge
                    }
                    HStack(spacing: 4) {
                        VenueIcon(ex: ex, size: 11)
                        Text("\(ex) · \(L("Perp"))").font(uiFont(10.5)).foregroundStyle(Color.t1Dim)
                    }
                }
            }
        }
        .lineLimit(1)
        .contentShape(.rect)
        .onTapGesture { open(base) }
        .pointerStyle(.link)
        .help("\(ex) · \(L("Perp")) — \(L("click to open this market"))")
    }

    /// Jump the whole terminal to this pair's perpetual market.
    private func open(_ base: String) {
        let st = Store.shared
        Pairs.open(base)
        if st.state.mode != "Perp" { st.call("set_mode", ["mode": "Perp"]) }
    }
}

extension AccountSymbolCell where Badge == EmptyView {
    init(ex: String, symbol: String) { self.init(ex: ex, symbol: symbol, compact: true) { EmptyView() } }
}

struct AccountBadge: View {
    let text: String
    let color: Color
    var body: some View {
        Text(text).font(uiFont(10, .semibold)).foregroundStyle(color)
            .padding(.horizontal, 5).padding(.vertical, 1.5)
            .background(RoundedRectangle(cornerRadius: 3).fill(color.opacity(0.15)))
    }
}

/// Monospaced, right-aligned number (the column itself is `.alignment(.trailing)`).
struct Num: View {
    let text: String
    var color: Color = Color.primary
    var sub: String? = nil
    var subColor: Color = Color.t1Dim
    var body: some View {
        if let sub {
            HStack(spacing: 4) {
                Text(text).foregroundStyle(color)
                Text(sub).foregroundStyle(subColor)
            }
            .font(numFont(11.5))
        } else {
            Text(text).font(numFont(11.5)).foregroundStyle(color)
        }
    }
}

@MainActor private func sideText(_ side: String, pos: String? = nil) -> some View {
    let buy = side == "buy" || side == "long"
    let base = L(side == "buy" ? "Buy" : side == "sell" ? "Sell" : side == "long" ? "Long" : "Short")
    let p = pos.map { " · " + L($0 == "long" ? "Long" : "Short") } ?? ""
    return Text(base + p).font(uiFont(11.5, .medium)).foregroundStyle(buy ? Color.t1Up : Color.t1Down)
}

@MainActor private func pnlColor(_ v: Double) -> Color { v >= 0 ? Color.t1Up : Color.t1Down }

private struct TimeCell: View {
    let ts: Int64
    var body: some View { Text(Fmt.time(ts)).font(numFont(11)).foregroundStyle(Color.secondary) }
}

/// Dense dark table chrome shared by every account table.
private struct AccountTableStyle: ViewModifier {
    let empty: Bool
    let emptyText: String
    func body(content: Content) -> some View {
        content
            .tableStyle(.inset(alternatesRowBackgrounds: false))
            .alternatingRowBackgrounds(.disabled)
            .scrollContentBackground(.hidden)
            .overlay {
                if empty { Text(emptyText).font(uiFont(12)).foregroundStyle(Color.t1Dim).padding(.top, 24) }
            }
    }
}

extension View {
    fileprivate func accountTable(empty: Bool, _ text: String) -> some View { modifier(AccountTableStyle(empty: empty, emptyText: text)) }
}

// MARK: positions

extension PositionRow {
    var notional: Double { qty * mark }
    var roe: Double { margin > 0 ? upnl / margin : 0 }
    var liqSort: Double { liq ?? .infinity }
}

struct AccountPositionsTable: View {
    let state: AppState
    let onMarketClose: (PositionRow) -> Void
    let onLimitClose: (PositionRow) -> Void
    @State private var sort = [KeyPathComparator(\PositionRow.symbol)]
    @State private var selection = Set<PositionRow.ID>()
    @AppStorage("t1.hideOtherSymbols") private var hideOthers = false
    /// per-row close inputs (price / size), seeded from mark and full size
    @State private var closePx: [String: String] = [:]
    @State private var closeQty: [String: String] = [:]
    @State private var confirm: PosAction?
    @State private var share: PositionRow?
    @State private var tpsl: PositionRow?

    enum PosAction: Identifiable {
        case market(PositionRow, Double), limit(PositionRow, Double, Double), reverse(PositionRow), closeAll([PositionRow])
        var id: String {
            switch self {
            case .market(let p, _): "m\(p.id)"
            case .limit(let p, _, _): "l\(p.id)"
            case .reverse(let p): "r\(p.id)"
            case .closeAll: "all"
            }
        }
    }

    var body: some View {
        // stable order: pushes reorder the engine's vector; ties broken by venue then side
        let cur = "\(state.base)USDT"
        let rows = state.positions.filter { !hideOthers || $0.symbol == cur }
            .sorted { ($0.symbol, $0.ex, $0.side) < ($1.symbol, $1.ex, $1.side) }.sorted(using: sort)
        PositionList(rows: rows, state: state, closeAll: { confirm = .closeAll(rows) },
                     row: { p in AnyView(rowView(p)) })
        .confirmationDialog(confirmTitle, isPresented: Binding(get: { confirm != nil }, set: { if !$0 { confirm = nil } }), titleVisibility: .visible, presenting: confirm) { a in
            Button(L("Cancel"), role: .cancel) {}
            Button(confirmButton(a), role: .destructive) { run(a) }
        } message: { a in Text(confirmMessage(a)) }
        .sheet(item: $share) { PnLShareSheet(p: $0) }
        // T1_SHARE=1: open the share sheet for the first position after launch (screenshots)
        .onChange(of: rows.count, initial: true) {
            if ProcessInfo.processInfo.environment["T1_SHARE"] != nil, share == nil, let p = rows.first { share = p }
        }
        .sheet(item: $tpsl) { OrderTpSlSheet(p: $0) }
    }

    private func rowView(_ p: PositionRow) -> some View {
        let side = p.isLong ? Color.t1Up : Color.t1Down
        return HStack(spacing: 0) {
            AccountSymbolCell(ex: p.ex, symbol: p.symbol) {
                AccountBadge(text: L(p.isLong ? "Long" : "Short") + (p.lev > 0 ? " \(Int(p.lev))x" : ""), color: side)
            }
            .frame(width: PositionList.symbolW, alignment: .leading)
            PosCol(w: PositionList.w.size) {
                Text("\(Fmt.usd(p.notional)) USDT").foregroundStyle(side)
                Text("\(Fmt.qty(p.qty)) \(p.base)").foregroundStyle(.secondary).font(.caption2.monospacedDigit())
            }
            PosCol(w: PositionList.w.px) { Text(Fmt.px(p.entry)) }
            PosCol(w: PositionList.w.px) { Text(Fmt.px(p.mark)) }
            PosCol(w: PositionList.w.px) { liqCell(p) }
            PosCol(w: PositionList.w.margin) {
                Text("\(Fmt.usd(p.margin)) USDT")
                if let c = p.cross { Text(c ? L("Cross") : L("Isolated")).foregroundStyle(.secondary).font(.caption2) }
            }
            PosCol(w: PositionList.w.pnl) {
                HStack(spacing: 6) {
                    VStack(alignment: .trailing, spacing: 1) {
                        Text("\(Fmt.signed(p.upnl)) USDT")
                        Text(Fmt.signed(p.roe * 100) + "%").font(.caption2.monospacedDigit())
                    }
                    .foregroundStyle(pnlColor(p.upnl))
                    Button { share = p } label: { Image(systemName: "square.and.arrow.up") }
                        .buttonStyle(.borderless).foregroundStyle(.secondary).help(L("Share this PnL as an image"))
                }
            }
            closeCell(p).frame(minWidth: PositionList.w.close, maxWidth: .infinity, alignment: .leading).padding(.leading, 18)
            PosCol(w: PositionList.w.tpsl) {
                HStack(spacing: 4) {
                    tpslCell(p)
                    Button { tpsl = p } label: { Image(systemName: "square.and.pencil") }
                        .buttonStyle(.borderless).foregroundStyle(.secondary).help(L("Whole-position or staged TP/SL"))
                }
            }
            PosCol(w: PositionList.w.funding) { fundingCell(p) }
            Button(L("Reverse")) { confirm = .reverse(p) }
                .buttonStyle(.bordered).controlSize(.small)
                .help(L("Close at market and open the same size on the other side (asks to confirm)"))
                .frame(width: PositionList.w.reverse, alignment: .trailing)
        }
        .font(.callout.monospacedDigit())
        .padding(.horizontal, 14)
        .frame(height: 50)
        .background(alignment: .leading) {
            // the side at a glance: a bar on the leading edge and a soft fade across the symbol
            HStack(spacing: 0) {
                Rectangle().fill(side).frame(width: 3)
                LinearGradient(colors: [side.opacity(0.16), side.opacity(0)], startPoint: .leading, endPoint: .trailing).frame(width: 260)
            }
        }
        .contextMenu {
            Button(L("Close at Market…")) { confirm = .market(p, p.qty) }
            Button(L("Limit Close in Order Panel")) { onLimitClose(p) }
            Button(L("TP/SL…")) { tpsl = p }
            Button(L("Share PnL…")) { share = p }
            Divider()
            Button(L("Copy Symbol")) { copy(p.symbol) }
        }
    }

    private func table(_ rows: [PositionRow]) -> some View {
        Table(rows, selection: $selection, sortOrder: $sort) {
            TableColumn(L("Symbol"), value: \.symbol) { p in
                AccountSymbolCell(ex: p.ex, symbol: p.symbol) {
                    AccountBadge(text: L(p.isLong ? "Long" : "Short") + (p.lev > 0 ? " \(Int(p.lev))x" : ""), color: p.isLong ? Color.t1Up : Color.t1Down)
                }
                .padding(.leading, 8)
                // the side at a glance: a fade from the row's leading edge
                .background(alignment: .leading) {
                    LinearGradient(colors: [(p.isLong ? Color.t1Up : Color.t1Down).opacity(0.28), .clear], startPoint: .leading, endPoint: .trailing)
                        .frame(width: 120).padding(.vertical, -4)
                        .overlay(alignment: .leading) { Rectangle().fill(p.isLong ? Color.t1Up : Color.t1Down).frame(width: 2.5) }
                }
            }
            .width(min: 160, ideal: 180)
            TableColumn(L("Size"), value: \.notional) { p in
                VStack(alignment: .trailing, spacing: 1) {
                    Text("\(Fmt.usd(p.notional)) USDT").foregroundStyle(p.isLong ? Color.t1Up : Color.t1Down)
                    Text("\(Fmt.qty(p.qty)) \(p.base)").foregroundStyle(.secondary).font(numFont(10.5))
                }
                .font(numFont(11.5))
            }
            .width(min: 100, ideal: 120).alignment(.trailing)
            TableColumn(L("Entry"), value: \.entry) { p in Num(text: Fmt.px(p.entry)) }
                .width(min: 70, ideal: 85).alignment(.trailing)
            TableColumn(L("Mark"), value: \.mark) { p in Num(text: Fmt.px(p.mark)) }
                .width(min: 70, ideal: 85).alignment(.trailing)
            TableColumn(L("Liq. Price"), value: \.liqSort) { p in liqCell(p) }
                .width(min: 70, ideal: 85).alignment(.trailing)
            TableColumn(L("Margin"), value: \.margin) { p in
                VStack(alignment: .trailing, spacing: 1) {
                    Text("\(Fmt.usd(p.margin)) USDT")
                    if let c = p.cross { Text(c ? L("Cross") : L("Isolated")).foregroundStyle(.secondary).font(numFont(10.5)) }
                }
                .font(numFont(11.5))
            }
            .width(min: 90, ideal: 105).alignment(.trailing)
            TableColumn(L("PnL (ROE%)"), value: \.upnl) { p in
                HStack(spacing: 6) {
                    VStack(alignment: .trailing, spacing: 1) {
                        Text("\(Fmt.signed(p.upnl)) USDT")
                        Text(Fmt.signed(p.roe * 100) + "%").font(numFont(10.5))
                    }
                    .font(numFont(11.5)).foregroundStyle(pnlColor(p.upnl))
                    Button { share = p } label: { Image(systemName: "square.and.arrow.up") }
                        .buttonStyle(.borderless).help(L("Share this PnL as an image"))
                }
            }
            .width(min: 110, ideal: 120).alignment(.trailing)
            Group {
            TableColumn(L("Close Position")) { p in closeCell(p) }
                .width(min: 240, ideal: 250)
            TableColumn(L("TP / SL")) { p in
                HStack(spacing: 4) {
                    tpslCell(p)
                    Button { tpsl = p } label: { Image(systemName: "square.and.pencil") }
                        .buttonStyle(.borderless).help(L("Whole-position or staged TP/SL"))
                }
            }
            .width(min: 110, ideal: 130).alignment(.trailing)
            TableColumn(L("Est. Funding")) { p in fundingCell(p) }
                .width(min: 110, ideal: 130).alignment(.trailing)
            TableColumn(L("Reverse")) { p in
                Button(L("Reverse")) { confirm = .reverse(p) }.buttonStyle(.bordered).controlSize(.small)
                    .help(L("Close at market and open the same size on the other side (asks to confirm)"))
            }
            .width(min: 70, ideal: 80)
            }
        }
        .contextMenu(forSelectionType: PositionRow.ID.self) { ids in
            if let p = rows.first(where: { ids.contains($0.id) }), ids.count == 1 {
                Button(L("Close at Market…")) { confirm = .market(p, p.qty) }
                Button(L("Limit Close in Order Panel")) { onLimitClose(p) }
                Button(L("TP/SL…")) { tpsl = p }
                Button(L("Share PnL…")) { share = p }
                Divider()
                Button(L("Copy Symbol")) { copy(p.symbol) }
            }
        }
        .accountTable(empty: rows.isEmpty, L("No open positions"))
    }

    /// Market | Limit, then the limit price and the size to close (defaults: mark, whole position).
    private func closeCell(_ p: PositionRow) -> some View {
        let px = Binding(get: { closePx[p.id] ?? Fmt.num(p.mark, priceDp(p.mark)) }, set: { closePx[p.id] = $0 })
        let qty = Binding(get: { closeQty[p.id] ?? Fmt.num(p.qty, qtyDp(p.qty)) }, set: { closeQty[p.id] = $0 })
        let q = min(OrderNum.parse(qty.wrappedValue) ?? 0, p.qty)
        return HStack(spacing: 6) {
            Button(L("Market")) { if q > 0 { confirm = .market(p, q) } }.buttonStyle(.borderless).foregroundStyle(.orange)
            Text("|").foregroundStyle(.tertiary)
            Button(L("Limit")) { if q > 0, let x = OrderNum.parse(px.wrappedValue), x > 0 { confirm = .limit(p, x, q) } }.buttonStyle(.borderless).foregroundStyle(.orange)
            PosField(text: px, width: 82).help(L("Limit close price"))
            PosField(text: qty, width: 72).help("\(L("Size to close")) (\(p.base))")
        }
        .font(.caption)
    }

    private func priceDp(_ v: Double) -> Int { v >= 10_000 ? 1 : v >= 100 ? 2 : v >= 1 ? 4 : 6 }
    private func qtyDp(_ v: Double) -> Int { v >= 1000 ? 1 : v >= 1 ? 3 : 5 }

    private var confirmTitle: String {
        switch confirm {
        case .reverse: L("Reverse position?")
        case .closeAll: L("Close all positions at market?")
        case .limit: L("Place limit close?")
        default: L("Close position at market?")
        }
    }
    private func confirmButton(_ a: PosAction) -> String {
        switch a {
        case .reverse: L("Reverse")
        case .closeAll: L("Close All")
        case .limit: L("Place Limit Close")
        case .market: L("Close at Market")
        }
    }
    private func confirmMessage(_ a: PosAction) -> String {
        switch a {
        case .market(let p, let q): "\(p.ex) \(p.symbol) \(L(p.isLong ? "Long" : "Short")) \(Fmt.qty(q)) \(p.base) ≈ \(Fmt.usd(q * p.mark, 0)) USDT\n\(L("A market order fills against the book; slippage applies."))"
        case .limit(let p, let x, let q): "\(p.ex) \(p.symbol) \(L(p.isLong ? "Long" : "Short")) \(Fmt.qty(q)) \(p.base) @ \(Fmt.px(x))"
        case .reverse(let p): "\(p.ex) \(p.symbol): \(L("close")) \(L(p.isLong ? "Long" : "Short")) \(Fmt.qty(p.qty)) \(p.base), \(L("then open")) \(L(p.isLong ? "Short" : "Long")) \(Fmt.qty(p.qty)) \(p.base)\n\(L("Two market orders; if the second fails you are left flat."))"
        case .closeAll(let ps): ps.map { "\($0.ex) \($0.symbol) \(L($0.isLong ? "Long" : "Short")) \(Fmt.qty($0.qty))" }.joined(separator: "\n")
        }
    }
    private func run(_ a: PosAction) {
        let st = Store.shared
        switch a {
        case .market(let p, let q): st.call("close_position", ["ex": p.ex, "symbol": p.symbol, "side": p.side, "qty": q])
        case .limit(let p, let x, let q): st.call("close_position", ["ex": p.ex, "symbol": p.symbol, "side": p.side, "qty": q, "price": x])
        case .reverse(let p): st.call("reverse_position", ["ex": p.ex, "symbol": p.symbol, "side": p.side])
        case .closeAll(let ps): for p in ps { st.call("close_position", ["ex": p.ex, "symbol": p.symbol, "side": p.side, "qty": p.qty]) }
        }
    }

    @ViewBuilder private func liqCell(_ p: PositionRow) -> some View {
        if let liq = p.liq {
            Num(text: Fmt.px(liq), color: Color.orange)
        } else {
            let pm = state.binance_pm && p.ex == "Binance"
            Num(text: "—", color: Color.t1Dim)
                .help(pm ? L("Portfolio Margin liquidates on the whole account's uniMMR; the exchange gives no per-position liquidation price.") : L("No liquidation price reported"))
        }
    }

    /// Venue rate per settlement and countdown; underneath, the estimated payment at the next one
    /// (negative = this position pays).
    @ViewBuilder private func fundingCell(_ p: PositionRow) -> some View {
        if let r = p.funding_rate {
            VStack(alignment: .trailing, spacing: 1) {
                HStack(spacing: 4) {
                    Text(String(format: "%+.4f%%", r * 100)).foregroundStyle(r >= 0 ? Color.t1Up : Color.t1Down)
                    if let n = p.next_funding_ms { FundingCountdown(next: n).foregroundStyle(.secondary) }
                }
                .font(numFont(11.5))
                if let e = p.funding_est {
                    Text("\(L("next")) \(Fmt.signed(e)) USDT").font(numFont(10.5)).foregroundStyle(e >= 0 ? Color.t1Up : Color.t1Down)
                }
            }
            .help(p.funding_interval_h.map { String(format: "%@ %gh", L("Settles every"), $0) } ?? "")
        } else {
            Text("—").font(numFont(11.5)).foregroundStyle(Color.t1Dim).help(L("Funding is shown for Binance, Bybit and Hyperliquid positions"))
        }
    }

    @ViewBuilder private func tpslCell(_ p: PositionRow) -> some View {
        let mine = state.tpsl.filter { $0.ex == p.ex && $0.symbol == p.symbol && $0.pos == p.side }
        let tp = mine.filter(\.take_profit).map(\.trigger_px).sorted()
        let sl = mine.filter { !$0.take_profit }.map(\.trigger_px).sorted()
        if mine.isEmpty {
            Text("—").font(numFont(11.5)).foregroundStyle(Color.t1Dim)
        } else {
            HStack(spacing: 3) {
                Text(tp.isEmpty ? "—" : tp.map { Fmt.px($0) }.joined(separator: ", ")).foregroundStyle(tp.isEmpty ? Color.t1Dim : Color.t1Up)
                Text("/").foregroundStyle(Color.t1Dim)
                Text(sl.isEmpty ? "—" : sl.map { Fmt.px($0) }.joined(separator: ", ")).foregroundStyle(sl.isEmpty ? Color.t1Dim : Color.t1Down)
            }
            .font(numFont(11.5))
            .help(mine.map { "\($0.take_profit ? "TP" : "SL") \(Fmt.px($0.trigger_px)) (\($0.trigger)) · \($0.qty.map { Fmt.qty($0) } ?? L("entire position"))" }.joined(separator: "\n"))
        }
    }
}

// MARK: open orders

extension OrderRow {
    var remaining: Double { qty - filled }
}

struct AccountOrdersTable: View {
    let orders: [OrderRow]
    let onCancel: (OrderRow) -> Void
    @State private var sort = [KeyPathComparator(\OrderRow.ts, order: .reverse)]
    @State private var selection = Set<OrderRow.ID>()

    var body: some View {
        let rows = orders.sorted { ($0.ts, $0.id) > ($1.ts, $1.id) }.sorted(using: sort)
        Table(rows, selection: $selection, sortOrder: $sort) {
            TableColumn(L("Time"), value: \.ts) { o in TimeCell(ts: o.ts) }.width(min: 110, ideal: 120)
            TableColumn(L("Symbol"), value: \.symbol) { o in AccountSymbolCell(ex: o.ex, symbol: o.symbol) }.width(min: 130, ideal: 160)
            TableColumn(L("Side"), value: \.side) { o in sideText(o.side, pos: o.pos) }.width(min: 70, ideal: 90)
            TableColumn(L("Type"), value: \.kind) { o in
                HStack(spacing: 4) {
                    Text(o.kind).font(uiFont(11.5)).foregroundStyle(Color.primary)
                    if o.reduce_only { AccountBadge(text: L("Reduce"), color: Color.secondary) }
                }
            }
            .width(min: 70, ideal: 100)
            TableColumn(L("Price"), value: \.price) { o in Num(text: Fmt.px(o.price)) }.width(min: 70, ideal: 90).alignment(.trailing)
            TableColumn(L("Amount"), value: \.qty) { o in Num(text: Fmt.qty(o.qty)) }.width(min: 60, ideal: 80).alignment(.trailing)
            TableColumn(L("Filled"), value: \.filled) { o in
                Num(text: Fmt.qty(o.filled), color: o.filled > 0 ? Color.primary : Color.secondary, sub: o.qty > 0 ? "(" + Fmt.num(o.filled / o.qty * 100, 1) + "%)" : nil, subColor: .secondary)
            }
            .width(min: 60, ideal: 80).alignment(.trailing)
            TableColumn(L("Value")) { o in Num(text: Fmt.usd(o.price * o.qty), color: Color.secondary) }.width(min: 70, ideal: 90).alignment(.trailing)
            TableColumn("") { o in Button(L("Cancel")) { onCancel(o) }.buttonStyle(.bordered).controlSize(.small) }
                .width(min: 60, ideal: 70).alignment(.trailing)
        }
        .contextMenu(forSelectionType: OrderRow.ID.self) { ids in
            let sel = rows.filter { ids.contains($0.id) }
            if !sel.isEmpty {
                Button(sel.count == 1 ? L("Cancel Order") : L("Cancel Selected Orders")) { sel.forEach(onCancel) }
                Button(L("Copy Order ID")) { copy(sel.map(\.id).joined(separator: "\n")) }
            }
        }
        .accountTable(empty: rows.isEmpty, L("No open orders"))
    }
}

// MARK: TP/SL

struct AccountTpSlTable: View {
    let rows: [TpSlRow]
    let onCancel: (TpSlRow) -> Void
    @State private var sort = [KeyPathComparator(\TpSlRow.symbol)]
    @State private var selection = Set<TpSlRow.ID>()

    var body: some View {
        let rows = rows.sorted { ($0.symbol, $0.ex, $0.id) < ($1.symbol, $1.ex, $1.id) }.sorted(using: sort)
        Table(rows, selection: $selection, sortOrder: $sort) {
            TableColumn(L("Symbol"), value: \.symbol) { t in AccountSymbolCell(ex: t.ex, symbol: t.symbol) }.width(min: 130, ideal: 170)
            TableColumn(L("Position"), value: \.pos) { t in sideText(t.pos) }.width(min: 60, ideal: 80)
            TableColumn(L("Type")) { t in
                AccountBadge(text: t.take_profit ? L("Take Profit") : L("Stop Loss"), color: t.take_profit ? Color.t1Up : Color.t1Down)
            }
            .width(min: 80, ideal: 100)
            TableColumn(L("Trigger Price"), value: \.trigger_px) { t in Num(text: Fmt.px(t.trigger_px)) }.width(min: 80, ideal: 100).alignment(.trailing)
            TableColumn(L("Trigger")) { t in Text(L(t.trigger == "last" ? "Last price" : "Mark price")).font(uiFont(11.5)).foregroundStyle(Color.secondary) }
                .width(min: 70, ideal: 90)
            TableColumn(L("Amount")) { t in
                if let q = t.qty { Num(text: Fmt.qty(q)) } else { Text(L("Entire position")).font(uiFont(11.5)).foregroundStyle(Color.secondary) }
            }
            .width(min: 80, ideal: 100).alignment(.trailing)
            TableColumn("") { t in Button(L("Cancel")) { onCancel(t) }.buttonStyle(.bordered).controlSize(.small) }
                .width(min: 60, ideal: 70).alignment(.trailing)
        }
        .contextMenu(forSelectionType: TpSlRow.ID.self) { ids in
            let sel = rows.filter { ids.contains($0.id) }
            if !sel.isEmpty { Button(sel.count == 1 ? L("Cancel TP/SL") : L("Cancel Selected TP/SL")) { sel.forEach(onCancel) } }
        }
        .accountTable(empty: rows.isEmpty, L("No TP/SL orders"))
    }
}

// MARK: history

struct AccountHistoryBar: View {
    let history: HistoryState
    let refresh: () -> Void

    var body: some View {
        TimelineView(.periodic(from: .now, by: 1)) { ctx in
            HStack(spacing: 8) {
                Text(L("Last 7 days, fetched when opened")).foregroundStyle(Color.t1Dim)
                if let at = history.updated_ms {
                    Text("· \(L("updated")) \(max(0, Int(ctx.date.timeIntervalSince1970 * 1000 - Double(at)) / 1000))s \(L("ago"))").foregroundStyle(Color.t1Dim)
                }
                Spacer()
                if history.loading {
                    ProgressView().controlSize(.mini)
                    Text(L("Loading…")).foregroundStyle(Color.secondary)
                } else {
                    Button { refresh() } label: { Label(L("Refresh"), systemImage: "arrow.clockwise") }.buttonStyle(.borderless).controlSize(.small)
                }
            }
            .font(uiFont(11))
            .padding(.horizontal, 12).frame(height: 24)
        }
    }
}

extension HistOrder { var filledSort: Double { filled } }

struct AccountOrderHistoryTable: View {
    let rows: [HistOrder]
    @State private var sort = [KeyPathComparator(\HistOrder.ts, order: .reverse)]

    var body: some View {
        let rows = rows.sorted(using: sort)
        Table(rows, sortOrder: $sort) {
            TableColumn(L("Time"), value: \.ts) { o in TimeCell(ts: o.ts) }.width(min: 110, ideal: 120)
            TableColumn(L("Symbol"), value: \.symbol) { o in AccountSymbolCell(ex: o.ex, symbol: o.symbol) }.width(min: 130, ideal: 160)
            TableColumn(L("Side"), value: \.side) { o in sideText(o.side) }.width(min: 50, ideal: 60)
            TableColumn(L("Type"), value: \.kind) { o in Text(o.kind).font(uiFont(11.5)).foregroundStyle(Color.primary) }.width(min: 60, ideal: 80)
            TableColumn(L("Price"), value: \.price) { o in Num(text: o.price > 0 ? Fmt.px(o.price) : "—", color: o.price > 0 ? Color.primary : Color.t1Dim) }
                .width(min: 70, ideal: 90).alignment(.trailing)
            TableColumn(L("Avg. Price"), value: \.avg) { o in Num(text: o.avg > 0 ? Fmt.px(o.avg) : "—", color: o.avg > 0 ? Color.primary : Color.t1Dim) }
                .width(min: 70, ideal: 90).alignment(.trailing)
            TableColumn(L("Filled / Amount"), value: \.filledSort) { o in Num(text: "\(Fmt.qty(o.filled)) / \(Fmt.qty(o.qty))") }
                .width(min: 100, ideal: 130).alignment(.trailing)
            TableColumn(L("Status"), value: \.status) { o in
                Text(o.status).font(uiFont(11.5)).foregroundStyle(statusColor(o.status))
            }
            .width(min: 70, ideal: 100).alignment(.trailing)
        }
        .accountTable(empty: rows.isEmpty, L("No orders in the last 7 days"))
    }

    private func statusColor(_ s: String) -> Color {
        let u = s.uppercased()
        if u.contains("FILLED") && !u.contains("PARTIAL") { return Color.primary }
        if u.contains("CANCEL") || u.contains("EXPIRE") { return Color.t1Dim }
        if u.contains("REJECT") { return Color.t1Down }
        return Color.secondary
    }
}

extension Fill {
    var notional: Double { price * qty }
    var realizedSort: Double { realized ?? 0 }
}

struct AccountFillsTable: View {
    let rows: [Fill]
    @State private var sort = [KeyPathComparator(\Fill.ts, order: .reverse)]

    var body: some View {
        let rows = rows.sorted(using: sort)
        Table(rows, sortOrder: $sort) {
            TableColumn(L("Time"), value: \.ts) { f in TimeCell(ts: f.ts) }.width(min: 110, ideal: 120)
            TableColumn(L("Symbol"), value: \.symbol) { f in AccountSymbolCell(ex: f.ex, symbol: f.symbol) }.width(min: 130, ideal: 160)
            TableColumn(L("Side"), value: \.side) { f in sideText(f.side) }.width(min: 50, ideal: 60)
            TableColumn(L("Price"), value: \.price) { f in Num(text: Fmt.px(f.price)) }.width(min: 70, ideal: 90).alignment(.trailing)
            TableColumn(L("Amount"), value: \.qty) { f in Num(text: Fmt.qty(f.qty)) }.width(min: 60, ideal: 80).alignment(.trailing)
            TableColumn(L("Value"), value: \.notional) { f in Num(text: Fmt.usd(f.notional)) }.width(min: 70, ideal: 100).alignment(.trailing)
            TableColumn(L("Fee"), value: \.fee) { f in Num(text: Fmt.num(f.fee, 4), color: Color.secondary) }.width(min: 60, ideal: 80).alignment(.trailing)
            TableColumn(L("Realized PnL"), value: \.realizedSort) { f in
                if let r = f.realized, r != 0 { Num(text: Fmt.signed(r), color: pnlColor(r)) } else { Num(text: "—", color: Color.t1Dim) }
            }
            .width(min: 80, ideal: 100).alignment(.trailing)
        }
        .accountTable(empty: rows.isEmpty, L("No trades in the last 7 days"))
    }
}

extension Closed {
    var qtySort: Double { qty ?? 0 }
    var sideSort: String { long.map { $0 ? "long" : "short" } ?? "" }
}

struct AccountClosedTable: View {
    let rows: [Closed]
    @State private var sort = [KeyPathComparator(\Closed.ts, order: .reverse)]

    var body: some View {
        let rows = rows.sorted(using: sort)
        Table(rows, sortOrder: $sort) {
            TableColumn(L("Closed"), value: \.ts) { c in TimeCell(ts: c.ts) }.width(min: 110, ideal: 120)
            TableColumn(L("Symbol"), value: \.symbol) { c in AccountSymbolCell(ex: c.ex, symbol: c.symbol) }.width(min: 130, ideal: 160)
            TableColumn(L("Side"), value: \.sideSort) { c in
                if let l = c.long { sideText(l ? "long" : "short") } else { Text("—").foregroundStyle(Color.t1Dim) }
            }
            .width(min: 50, ideal: 60)
            TableColumn(L("Amount"), value: \.qtySort) { c in Num(text: Fmt.qty(c.qty)) }.width(min: 60, ideal: 90).alignment(.trailing)
            TableColumn(L("Entry")) { c in Num(text: Fmt.px(c.entry)) }.width(min: 70, ideal: 90).alignment(.trailing)
            TableColumn(L("Exit")) { c in Num(text: Fmt.px(c.exit)) }.width(min: 70, ideal: 90).alignment(.trailing)
            TableColumn(L("Realized PnL"), value: \.pnl) { c in Num(text: Fmt.signed(c.pnl), color: pnlColor(c.pnl)) }
                .width(min: 80, ideal: 110).alignment(.trailing)
        }
        .accountTable(empty: rows.isEmpty, L("No closed positions in the last 7 days"))
    }
}

// MARK: order log

struct AccountLogView: View {
    let rows: [LogRow]

    var body: some View {
        if rows.isEmpty {
            ContentUnavailableView(L("No order activity this session"), systemImage: "list.bullet.rectangle")
        } else {
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 0) {
                    ForEach(rows) { r in
                        HStack(alignment: .firstTextBaseline, spacing: 10) {
                            Image(systemName: r.ok ? "checkmark.circle" : "exclamationmark.triangle.fill")
                                .font(.system(size: 10)).foregroundStyle(r.ok ? Color.t1Dim : Color.t1Down)
                            Text(Fmt.time(r.ts)).font(numFont(11)).foregroundStyle(Color.t1Dim)
                            Text(r.msg).font(numFont(11)).foregroundStyle(r.ok ? Color.primary : Color.t1Down)
                                .textSelection(.enabled)
                            Spacer(minLength: 0)
                        }
                        .padding(.horizontal, 12).padding(.vertical, 3)
                    }
                }
                .padding(.vertical, 4)
            }
        }
    }
}

/// PnL card as an image (1200 x 675, the size social apps preview): preview, options for what to
/// reveal, copy to the clipboard or save as PNG.
struct PnLShareSheet: View {
    let p: PositionRow
    @Environment(\.dismiss) private var dismiss
    @AppStorage("t1.share.amount") private var showAmount = true
    @AppStorage("t1.share.prices") private var showPrices = true
    @State private var note: String?

    var body: some View {
        VStack(spacing: 0) {
            Form {
                Section {
                    card.frame(width: 600, height: 337.5).scaleEffect(1).clipShape(.rect(cornerRadius: 14))
                        .frame(maxWidth: .infinity)
                }
                Section {
                    Toggle(L("Show PnL amount"), isOn: $showAmount)
                    Toggle(L("Show entry and mark price"), isOn: $showPrices)
                } footer: {
                    Text(L("Size, margin and account balances are never on the card."))
                }
            }
            .formStyle(.grouped)
            HStack(spacing: 10) {
                if let note { Label(note, systemImage: "checkmark.circle.fill").foregroundStyle(.secondary) }
                Spacer()
                Button(L("Close")) { dismiss() }.keyboardShortcut(.cancelAction)
                Button(L("Save…")) { save() }
                Button(L("Copy Image")) { copyImage() }.buttonStyle(.borderedProminent).keyboardShortcut(.defaultAction)
            }
            .controlSize(.large)
            .padding([.horizontal, .bottom], 20)
        }
        .frame(width: 680, height: 600)
    }

    /// The card, laid out at 600 x 337.5 and rendered at 2x.
    private var card: some View {
        let up = p.upnl >= 0
        let col = up ? Color(nsColor: .systemGreen) : Color(nsColor: .systemRed)
        let side = p.isLong ? Color(nsColor: .systemGreen) : Color(nsColor: .systemRed)
        return ZStack(alignment: .topLeading) {
            LinearGradient(colors: [Color(white: 0.10), Color(white: 0.03)], startPoint: .top, endPoint: .bottom)
            // soft glow in the result's color and the order-book motif of the app icon
            RadialGradient(colors: [col.opacity(0.35), .clear], center: .bottomTrailing, startRadius: 10, endRadius: 420)
            ShareBookMotif().opacity(0.16).frame(width: 260, height: 220).position(x: 470, y: 190)
            VStack(alignment: .leading, spacing: 0) {
                HStack(spacing: 8) {
                    if let icon = NSImage(named: "Depth") ?? NSApp.applicationIconImage { Image(nsImage: icon).resizable().frame(width: 26, height: 26) }
                    Text("Depth").font(.system(size: 17, weight: .semibold))
                    Spacer()
                    Text(Date.now.formatted(date: .abbreviated, time: .shortened)).font(.system(size: 12)).foregroundStyle(.white.opacity(0.55))
                }
                Spacer(minLength: 18)
                HStack(spacing: 8) {
                    CoinIcon(base: p.base, size: 24)
                    Text(p.symbol).font(.system(size: 22, weight: .bold))
                    Text("Perp").font(.system(size: 12, weight: .medium)).padding(.horizontal, 7).padding(.vertical, 3).background(.white.opacity(0.12), in: .capsule)
                    Text("\(p.isLong ? "Long" : "Short") \(Int(p.lev))x").font(.system(size: 12, weight: .semibold)).foregroundStyle(side)
                        .padding(.horizontal, 7).padding(.vertical, 3).background(side.opacity(0.18), in: .capsule)
                }
                Text(Fmt.signed(p.roe * 100) + "%").font(.system(size: 64, weight: .heavy).monospacedDigit()).foregroundStyle(col)
                    .padding(.top, 10)
                if showAmount {
                    Text("\(Fmt.signed(p.upnl)) USDT").font(.system(size: 18, weight: .semibold).monospacedDigit()).foregroundStyle(col.opacity(0.9))
                }
                Spacer(minLength: 16)
                if showPrices {
                    HStack(spacing: 34) {
                        kv("Entry", Fmt.px(p.entry))
                        kv("Mark", Fmt.px(p.mark))
                        kv("Venue", p.ex)
                    }
                }
            }
            .padding(26)
            .foregroundStyle(.white)
        }
        .frame(width: 600, height: 337.5)
    }

    private func kv(_ k: String, _ v: String) -> some View {
        VStack(alignment: .leading, spacing: 3) {
            Text(k).font(.system(size: 11)).foregroundStyle(.white.opacity(0.5))
            Text(v).font(.system(size: 15, weight: .semibold).monospacedDigit())
        }
    }

    @MainActor private func image() -> NSImage? {
        let r = ImageRenderer(content: card.environment(\.colorScheme, .dark))
        r.scale = 2
        return r.nsImage
    }
    @MainActor private func copyImage() {
        guard let img = image() else { return }
        NSPasteboard.general.clearContents()
        NSPasteboard.general.writeObjects([img])
        note = L("Copied")
    }
    @MainActor private func save() {
        guard let img = image(), let tiff = img.tiffRepresentation, let rep = NSBitmapImageRep(data: tiff), let png = rep.representation(using: .png, properties: [:]) else { return }
        let panel = NSSavePanel()
        panel.nameFieldStringValue = "\(p.symbol)-pnl.png"
        panel.allowedContentTypes = [.png]
        if panel.runModal() == .OK, let url = panel.url { try? png.write(to: url); note = L("Saved") }
    }
}

/// The app icon's mirrored order book, as a background motif.
private struct ShareBookMotif: View {
    let bids: [Double] = [0.34, 0.56, 0.44, 0.82, 0.62, 1.0]
    let asks: [Double] = [0.30, 0.50, 0.72, 0.46, 0.92, 0.76]
    var body: some View {
        HStack(spacing: 8) {
            VStack(alignment: .trailing, spacing: 9) { ForEach(bids.indices, id: \.self) { i in RoundedRectangle(cornerRadius: 4).fill(Color(nsColor: .systemGreen)).frame(width: 110 * bids[i], height: 20) } }
                .frame(width: 110, alignment: .trailing)
            Capsule().fill(.white).frame(width: 5, height: 190)
            VStack(alignment: .leading, spacing: 9) { ForEach(asks.indices, id: \.self) { i in RoundedRectangle(cornerRadius: 4).fill(Color(nsColor: .systemRed)).frame(width: 110 * asks[i], height: 20) } }
                .frame(width: 110, alignment: .leading)
        }
    }
}

/// Positions as hand-laid rows (not Table): full-row side tint, one font, aligned numeric columns,
/// "Close All" in the close column's header like Binance.
struct PositionList: View {
    let rows: [PositionRow]
    let state: AppState
    let closeAll: () -> Void
    let row: (PositionRow) -> AnyView
    /// minimum widths; spare width is shared evenly by every column except the symbol and Reverse
    static let w = (size: 130.0, px: 92.0, margin: 116.0, pnl: 140.0, close: 268.0, tpsl: 130.0, funding: 140.0, reverse: 78.0)
    static let symbolW = 210.0

    var body: some View {
        let w = Self.w
        // vertical only: the symbol column absorbs the spare width, so the rows always span the strip
        ScrollView(.vertical) {
            VStack(spacing: 0) {
                HStack(spacing: 0) {
                    Text(L("Symbol")).frame(width: Self.symbolW, alignment: .leading)
                    head(L("Size"), w.size); head(L("Entry"), w.px); head(L("Mark"), w.px); head(L("Liq. Price"), w.px)
                    head(L("Margin"), w.margin); head(L("PnL (ROE%)"), w.pnl)
                    HStack(spacing: 8) {
                        Text(L("Close Position"))
                        Button(L("Close All Positions"), action: closeAll).buttonStyle(.borderless).foregroundStyle(.orange)
                            .disabled(rows.isEmpty).help(L("Market-close every position listed here (asks to confirm)"))
                    }
                    .frame(minWidth: w.close, maxWidth: .infinity, alignment: .leading).padding(.leading, 18)
                    head(L("TP / SL"), w.tpsl); head(L("Est. Funding"), w.funding); head("", w.reverse)
                }
                .font(.caption).foregroundStyle(.secondary)
                .padding(.horizontal, 14).frame(height: 30)
                Divider()
                if rows.isEmpty {
                    Text(L("No open positions")).foregroundStyle(.tertiary).padding(.top, 24)
                }
                ForEach(rows) { p in
                    row(p)
                    Divider().opacity(0.5)
                }
            }
            .frame(maxWidth: .infinity)
        }
        .scrollIndicators(.automatic)
    }

    private func head(_ t: String, _ width: Double) -> some View {
        Text(t).frame(minWidth: width, maxWidth: width == Self.w.reverse ? width : .infinity, alignment: .trailing)
    }
}

/// Right-aligned numeric column of a position row.
struct PosCol<C: View>: View {
    let w: Double
    @ViewBuilder var content: C
    var body: some View { VStack(alignment: .trailing, spacing: 2) { content }.frame(minWidth: w, maxWidth: .infinity, alignment: .trailing) }
}

/// Small rounded input used inside rows.
struct PosField: View {
    @Binding var text: String
    var width: Double
    var body: some View {
        TextField("", text: $text).textFieldStyle(.roundedBorder).multilineTextAlignment(.trailing).monospacedDigit().frame(width: width)
    }
}
