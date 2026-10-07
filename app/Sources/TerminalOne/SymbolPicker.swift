import SwiftUI

struct TickersReply: Decodable { var tickers: [Ticker] }

/// Pair picker popover: USDT perpetuals by volume (engine `tickers`), search, favorites
/// (UserDefaults "t1.favorites"). Click picks; arrows move the selection, Enter picks it
/// (or the first match).
struct SymbolPicker: View {
    let current: String
    let close: () -> Void

    @Environment(Store.self) private var store
    @State private var query = ""
    @State private var favsOnly = false
    @State private var tickers: [Ticker] = []
    @State private var sel: String?
    @State private var favs = Set(UserDefaults.standard.stringArray(forKey: "t1.favorites") ?? ["BTC", "ETH", "SOL"])
    @FocusState private var focused: Bool

    private var rows: [Ticker] {
        let q = query.trimmingCharacters(in: .whitespaces).uppercased()
        return tickers.filter { (q.isEmpty || $0.base.contains(q)) && (!favsOnly || favs.contains($0.base)) }
    }

    var body: some View {
        let rows = rows
        VStack(spacing: 10) {
            TextField(L("Search pairs"), text: $query)
                .textFieldStyle(.roundedBorder)
                .focused($focused)
                .onSubmit { if let b = sel.flatMap({ s in rows.first { $0.base == s } })?.base ?? rows.first?.base { pick(b) } }
                .onKeyPress(.downArrow) { move(1, rows) }
                .onKeyPress(.upArrow) { move(-1, rows) }
            Picker("", selection: $favsOnly) {
                Text(L("Favorites")).tag(true)
                Text(L("All")).tag(false)
            }
            .pickerStyle(.segmented)
            .labelsHidden()

            if rows.isEmpty {
                Text(tickers.isEmpty ? L("Loading…") : L("No matching pairs"))
                    .foregroundStyle(.secondary)
                    .frame(maxWidth: .infinity, maxHeight: .infinity)
            } else {
                // a click selects = picks; arrow keys from the search field only move `sel`
                Table(rows, selection: Binding(get: { sel }, set: { if let b = $0 { pick(b) } })) {
                    TableColumn("") { t in
                        Button { toggle(t.base) } label: {
                            Image(systemName: favs.contains(t.base) ? "star.fill" : "star")
                                .foregroundStyle(favs.contains(t.base) ? AnyShapeStyle(.yellow) : AnyShapeStyle(.tertiary))
                        }
                        .buttonStyle(.borderless)
                    }
                    .width(20)
                    TableColumn(L("Pair")) { t in
                        HStack(spacing: 6) {
                            CoinIcon(base: t.base, size: 16)
                            Text(t.base).fontWeight(.medium) + Text("USDT").foregroundStyle(.secondary)
                        }
                    }
                    TableColumn(L("Last")) { t in
                        Text(Fmt.px(t.last)).monospacedDigit().frame(maxWidth: .infinity, alignment: .trailing)
                    }
                    .width(96)
                    TableColumn(L("24h Chg")) { t in
                        Text(String(format: "%+.2f%%", t.chg_pct)).monospacedDigit()
                            .foregroundStyle(t.chg_pct >= 0 ? T1.up : T1.down)
                            .frame(maxWidth: .infinity, alignment: .trailing)
                    }
                    .width(70)
                    TableColumn(L("Volume")) { t in
                        Text(t.quote_vol >= 1e9 ? String(format: "%.2fB", t.quote_vol / 1e9) : String(format: "%.1fM", t.quote_vol / 1e6))
                            .monospacedDigit().foregroundStyle(.secondary)
                            .frame(maxWidth: .infinity, alignment: .trailing)
                    }
                    .width(64)
                }
                .tableStyle(.inset(alternatesRowBackgrounds: false))
                .alternatingRowBackgrounds(.disabled)
            }
        }
        .padding(12)
        .frame(width: 460, height: 520)
        .onAppear { focused = true; sel = current }
        .task {
            // refresh while open: last / change / volume move
            while !Task.isCancelled {
                if let r = store.call("tickers", as: TickersReply.self) { tickers = r.tickers }
                try? await Task.sleep(for: .seconds(3))
            }
        }
    }

    private func move(_ d: Int, _ rows: [Ticker]) -> KeyPress.Result {
        guard !rows.isEmpty else { return .ignored }
        let i = sel.flatMap { s in rows.firstIndex { $0.base == s } } ?? (d > 0 ? -1 : rows.count)
        sel = rows[min(max(i + d, 0), rows.count - 1)].base
        return .handled
    }

    private func toggle(_ b: String) {
        if favs.remove(b) == nil { favs.insert(b) }
        UserDefaults.standard.set(favs.sorted(), forKey: "t1.favorites")
    }

    private func pick(_ b: String) {
        if b != current { store.call("set_base", ["base": b]) }
        close()
    }
}
