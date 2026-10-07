import AppKit
import Combine
import SwiftUI

/// Bottom strip: positions, orders, TP/SL, history, order log and assets across keyed venues,
/// with position totals and account-level risk on the right. Also hosts the Settings and
/// Transfer sheets (opened through NotificationCenter "T1OpenSettings" / "T1OpenTransfer").
struct AccountPanel: View {
    @Environment(Store.self) private var store
    @State private var tab = AccountTab.initial
    @State private var historyRequested = false
    @State private var settingsTab: SettingsPane?
    @State private var transferEx: TransferTarget?
    @State private var closing: PositionRow?
    @State private var actionError: String?
    @AppStorage("t1.hideOtherSymbols") private var hideOthers = false

    var body: some View {
        let s = store.state
        VStack(spacing: 0) {
            HStack(spacing: 12) {
                // no ViewThatFits here: it re-measures every candidate on each data tick
                textTabs(s).layoutPriority(1)
                Spacer(minLength: 8)
                if tab == .positions {
                    Toggle(L("Hide other symbols"), isOn: $hideOthers).toggleStyle(.checkbox).controlSize(.small).font(.caption)
                }
                AccountSummary(state: s, totals: true).fixedSize()
            }
            .padding(.horizontal, 10).padding(.vertical, 5)
            Divider()
            content(s)
                .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
        }
        .onChange(of: tab, initial: true) { _, t in
            // history is fetched when first opened (never on a timer); Refresh re-fetches
            if t.isHistory && !historyRequested {
                historyRequested = true
                if s.history.updated_ms == nil && !s.history.loading { store.call("load_history") }
            }
        }
        .onReceive(NotificationCenter.default.publisher(for: .init("T1OpenSettings"))) { n in
            settingsTab = SettingsPane(from: n.object as? String)
        }
        .onReceive(NotificationCenter.default.publisher(for: .init("T1OpenTransfer"))) { n in
            openTransfer((n.object as? String) ?? (n.userInfo?["ex"] as? String))
        }
        .onAppear {
            // T1_OPEN_SETTINGS=<general|appearance|trading|keys|about> / T1_OPEN_TRANSFER=1: open at launch (screenshots)
            if let p = ProcessInfo.processInfo.environment["T1_OPEN_SETTINGS"] { settingsTab = SettingsPane(from: p) }
            if ProcessInfo.processInfo.environment["T1_OPEN_TRANSFER"] != nil {
                DispatchQueue.main.asyncAfter(deadline: .now() + 3) { openTransfer(nil) }
            }
            SheetShot.schedule()
        }
        .sheet(item: $settingsTab) { p in
            SettingsView(initial: p).environment(store)
        }
        .sheet(item: $transferEx) { t in
            AccountTransferSheet(ex: t.ex).environment(store)
        }
        .confirmationDialog(L("Close position at market?"), isPresented: Binding(get: { closing != nil }, set: { if !$0 { closing = nil } }), titleVisibility: .visible, presenting: closing) { p in
            Button(L("Cancel"), role: .cancel) {}
            Button(L("Close at Market"), role: .destructive) {
                act("close_position", ["ex": p.ex, "symbol": p.symbol, "side": p.side, "qty": p.qty])
            }
        } message: { p in
            Text("\(p.ex) \(p.symbol) \(L(p.isLong ? "Long" : "Short")) \(Fmt.qty(p.qty)) ≈ \(Fmt.usd(p.qty * p.mark, 0)) USDT\n\(L("A market order fills against the book; slippage applies."))")
        }
        .alert(L("Action failed"), isPresented: Binding(get: { actionError != nil }, set: { if !$0 { actionError = nil } })) {
            Button(L("OK")) {}
        } message: { Text(actionError ?? "") }
    }

    /// Text tabs with an accent underline (same as the order panel's order types).
    private func textTabs(_ s: AppState) -> some View {
        HStack(spacing: 18) {
            ForEach(AccountTab.allCases) { t in
                Button { tab = t } label: {
                    VStack(spacing: 4) {
                        Text(title(t, s)).font(.callout.weight(tab == t ? .semibold : .regular)).foregroundStyle(tab == t ? .primary : .secondary)
                        Capsule().fill(tab == t ? Color.accentColor : .clear).frame(width: 18, height: 3)
                    }
                    .fixedSize()
                }
                .buttonStyle(.plain)
            }
        }
        .padding(.top, 4)
    }

    private func tabPicker(_ s: AppState) -> some View {
        Picker(L("Account view"), selection: $tab) {
            ForEach(AccountTab.allCases) { t in Text(title(t, s)).tag(t) }
        }
        .labelsHidden().controlSize(.small)
    }

    private func title(_ t: AccountTab, _ s: AppState) -> String {
        switch t {
        case .positions: "\(L(t.title)) (\(s.positions.count))"
        case .orders: "\(L(t.title)) (\(s.orders.count))"
        case .tpsl: "\(L(t.title)) (\(s.tpsl.count))"
        case .signals: "\(L(t.title)) (\(s.signals.count))"
        default: L(t.title)
        }
    }

    @ViewBuilder private func content(_ s: AppState) -> some View {
        if tab == .signals {
            SignalsView(rows: s.signals)
        } else if s.keys.allSatisfy({ !$0.configured }) && tab != .log {
            ContentUnavailableView {
                Label(L("No API keys"), systemImage: "key")
            } description: {
                Text(L("Add an API key to see positions, orders and balances."))
            } actions: {
                Button(L("Open API Keys")) { settingsTab = .keys }.controlSize(.small)
            }
        } else {
            switch tab {
            case .positions:
                AccountPositionsTable(state: s, onMarketClose: { closing = $0 }, onLimitClose: prefillClose)
            case .orders:
                AccountOrdersTable(orders: s.orders) { o in act("cancel", ["ex": o.ex, "id": o.id]) }
            case .tpsl:
                AccountTpSlTable(rows: s.tpsl) { t in act("cancel_tpsl", ["ex": t.ex, "id": t.id]) }
            case .orderHistory, .tradeHistory, .positionHistory:
                VStack(spacing: 0) {
                    AccountHistoryBar(history: s.history) { store.call("load_history") }
                    switch tab {
                    case .orderHistory: AccountOrderHistoryTable(rows: s.history.orders)
                    case .tradeHistory: AccountFillsTable(rows: s.history.fills)
                    default: AccountClosedTable(rows: s.history.closed)
                    }
                }
            case .log:
                AccountLogView(rows: s.log)
            case .signals:
                SignalsView(rows: s.signals)
            case .assets:
                AccountAssetsView(state: s) { openTransfer($0) }
            }
        }
    }

    // MARK: actions

    private func act(_ op: String, _ args: [String: Any]) {
        if store.call(op, args)?["ok"] as? Bool != true { actionError = store.lastError ?? L("Failed") }
    }

    /// Limit close: the order panel takes venue, close mode, size and the mark as the price.
    private func prefillClose(_ p: PositionRow) {
        NotificationCenter.default.post(name: .init("T1PrefillClose"), object: nil,
                                        userInfo: ["ex": p.ex, "symbol": p.symbol, "side": p.side, "qty": p.qty, "price": p.mark])
    }

    private func openTransfer(_ ex: String?) {
        let keyed = store.state.wallets.map(\.ex)
        guard let e = ex.flatMap({ e in keyed.first { $0.lowercased() == e.lowercased() } })
            ?? (keyed.contains(store.state.trade.venue) ? store.state.trade.venue : keyed.first) else { return }
        // wallets are loaded on demand; the sheet fills in its defaults once they arrive
        if let w = store.state.wallets.first(where: { $0.ex == e }), w.updated_ms == nil, !w.loading { store.call("load_wallets", ["ex": e]) }
        transferEx = TransferTarget(ex: e)
    }
}

struct TransferTarget: Identifiable { var ex: String; var id: String { ex } }

enum AccountTab: String, CaseIterable, Identifiable {
    case positions, orders, tpsl, signals, orderHistory, tradeHistory, positionHistory, log, assets
    var id: String { rawValue }
    var title: String {
        switch self {
        case .positions: "Positions"
        case .orders: "Open Orders"
        case .tpsl: "TP/SL"
        case .signals: "Signals"
        case .orderHistory: "Order History"
        case .tradeHistory: "Trade History"
        case .positionHistory: "Position History"
        case .log: "Order Log"
        case .assets: "Assets"
        }
    }
    var isHistory: Bool { self == .orderHistory || self == .tradeHistory || self == .positionHistory }
    /// T1_ACCOUNT_TAB=<raw value> preselects a tab (screenshots).
    static var initial: AccountTab { ProcessInfo.processInfo.environment["T1_ACCOUNT_TAB"].flatMap(AccountTab.init(rawValue:)) ?? .positions }
}

/// Position totals and account-level risk (what actually liquidates a unified account).
struct AccountSummary: View {
    let state: AppState
    var totals = true

    var body: some View {
        let ps = state.positions
        HStack(spacing: 14) {
            if totals && !ps.isEmpty {
                let upnl = ps.map(\.upnl).reduce(0, +)
                kv(L("uPnL"), Fmt.signed(upnl), upnl >= 0 ? Color.t1Up : Color.t1Down)
                kv(L("Margin"), Fmt.usd(ps.map(\.margin).reduce(0, +)), .primary)
                kv(L("Value"), Fmt.usd(ps.map { $0.qty * $0.mark }.reduce(0, +), 0), .primary)
            }
            let risk = state.balances.filter { $0.uni_mmr != nil || $0.mm_rate != nil }.sorted { $0.ex < $1.ex }
            if totals && !risk.isEmpty && !ps.isEmpty { Divider().frame(height: 14) }
            ForEach(risk) { b in
                HStack(spacing: 5) {
                    VenueIcon(ex: b.ex, size: 12)
                    if let m = b.uni_mmr {
                        Text("uniMMR").foregroundStyle(.secondary)
                        Text(Fmt.num(m, 2)).font(numFont(11.5, .semibold)).foregroundStyle(m > 1.5 ? Color.t1Up : m > 1.2 ? Color.orange : Color.t1Down)
                    }
                    if let r = b.mm_rate {
                        Text(L("MM rate")).foregroundStyle(.secondary)
                        Text(Fmt.num(r * 100, 1) + "%").font(numFont(11.5, .semibold)).foregroundStyle(r < 0.5 ? Color.t1Up : r < 0.8 ? Color.orange : Color.t1Down)
                    }
                }
                .help(tip(b))
            }
        }
        .font(.system(size: 11))
        .lineLimit(1)
    }

    private func kv(_ k: String, _ v: String, _ c: Color) -> some View {
        HStack(spacing: 5) {
            Text(k).foregroundStyle(.secondary)
            Text(v).font(numFont(11.5, .semibold)).foregroundStyle(c)
        }
    }

    private func tip(_ b: BalanceRow) -> String {
        var t = "\(b.ex) · \(L("Equity")) \(Fmt.usd(b.equity)) · \(L("Available")) \(Fmt.usd(b.available))\n"
        if b.uni_mmr != nil { t += L("Portfolio Margin maintenance ratio: below 1.20 margin call, below 1.05 the whole account is liquidated. Higher is safer.") }
        if b.mm_rate != nil { t += L("Unified account maintenance margin rate: 100% triggers liquidation. Lower is safer.") }
        return t
    }
}

/// System text style closest to a point size (macOS: body 13, callout 12, subheadline 11, footnote 10).
func textStyle(_ size: CGFloat) -> Font.TextStyle {
    size >= 15 ? .title3 : size >= 13 ? .body : size >= 12 ? .callout : size >= 11 ? .subheadline : .footnote
}
func uiFont(_ size: CGFloat, _ weight: Font.Weight = .regular) -> Font { .system(textStyle(size), weight: weight) }
/// Numbers: system text style with monospaced digits so columns line up.
func numFont(_ size: CGFloat, _ weight: Font.Weight = .regular) -> Font { .system(textStyle(size), weight: weight).monospacedDigit() }

extension Color {
    static let t1Dim = Color(nsColor: .tertiaryLabelColor)
    /// Rising / profit color: system green, or system red when Settings flips to red-up.
    @MainActor static var t1Up: Color { Store.shared.state.prefs.red_up ? .red : .green }
    @MainActor static var t1Down: Color { Store.shared.state.prefs.red_up ? .green : .red }
}

/// T1_OPEN_SETTINGS / T1_OPEN_TRANSFER with T1_UI_SHOT: a sheet is its own window, so the
/// app-level shot misses it; save it next to the main shot as `<shot>-sheet.png` a second earlier.
@MainActor enum SheetShot {
    static func schedule() {
        let env = ProcessInfo.processInfo.environment
        guard let path = env["T1_UI_SHOT"], env["T1_OPEN_SETTINGS"] != nil || env["T1_OPEN_TRANSFER"] != nil else { return }
        let after = max(1, (Double(env["T1_UI_SHOT_AFTER"] ?? "") ?? 15) - 1)
        DispatchQueue.main.asyncAfter(deadline: .now() + after) {
            MainActor.assumeIsolated {
                guard let w = NSApp.windows.compactMap(\.attachedSheet).first ?? NSApp.windows.first(where: { $0.isSheet }),
                      let v = w.contentView?.superview ?? w.contentView, let rep = v.bitmapImageRepForCachingDisplay(in: v.bounds) else { return }
                v.cacheDisplay(in: v.bounds, to: rep)
                let out = (path as NSString).deletingPathExtension + "-sheet.png"
                try? rep.representation(using: .png, properties: [:])?.write(to: URL(fileURLWithPath: out))
            }
        }
    }
}

/// Detected signals, newest first: severity, direction, what happened and the numbers behind it.
struct SignalsView: View {
    let rows: [SignalRow]

    var body: some View {
        if rows.isEmpty {
            ContentUnavailableView(L("No signals yet"), systemImage: "waveform.badge.magnifyingglass",
                description: Text(L("Volume spikes, liquidation cascades, OI / price divergence, crowded funding, spot vs perp flow, basis, whale trades and venue dislocations appear here as they happen.")))
        } else {
            ScrollView {
                LazyVStack(spacing: 0) {
                    ForEach(rows) { r in
                        HStack(alignment: .top, spacing: 10) {
                            Image(systemName: r.dir > 0 ? "arrow.up.right.circle.fill" : r.dir < 0 ? "arrow.down.right.circle.fill" : "exclamationmark.circle.fill")
                                .foregroundStyle(r.dir > 0 ? T1.up : r.dir < 0 ? T1.down : Color.orange)
                                .font(.title3)
                            VStack(alignment: .leading, spacing: 2) {
                                HStack(spacing: 6) {
                                    Text(L(r.title)).font(.callout.weight(.semibold))
                                    Text(String(repeating: "●", count: r.severity)).font(.caption2).foregroundStyle(r.severity >= 3 ? Color.red : r.severity == 2 ? Color.orange : Color.secondary)
                                }
                                Text(r.detail).font(.caption).foregroundStyle(.secondary)
                            }
                            Spacer()
                            Text(Fmt.time(r.ts)).font(.caption.monospacedDigit()).foregroundStyle(.tertiary)
                        }
                        .padding(.horizontal, 14).padding(.vertical, 8)
                        Divider().opacity(0.5)
                    }
                }
            }
        }
    }
}
