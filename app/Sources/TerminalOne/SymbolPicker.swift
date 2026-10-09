import SwiftUI

struct TickersReply: Decodable { var tickers: [Ticker] }

/// Pair picker popover: USDT perpetuals (engine `tickers`), search ranked exact > prefix > contains,
/// sortable columns, favorites and recently opened pairs (Pairs). Click picks; arrows move the
/// selection, Enter picks it (or the first match), Escape closes.
struct SymbolPicker: View {
    let current: String
    let close: () -> Void

    @Environment(Store.self) private var store
    @State private var query = ""
    @AppStorage("t1.pickerFavs") private var favsOnly = false
    @State private var tickers: [Ticker] = []
    @State private var sel: String?
    @State private var favs = Set(Pairs.favorites)
    @State private var sort = [KeyPathComparator(\Ticker.quote_vol, order: .reverse)]
    @FocusState private var focused: Bool

    private var rows: [Ticker] {
        let q = query.trimmingCharacters(in: .whitespaces).uppercased()
            .replacingOccurrences(of: "USDT", with: "").replacingOccurrences(of: "/", with: "")
        // 0 exact, 1 prefix, 2 contains; the column sort orders within each group
        let rank = { (b: String) -> Int in q.isEmpty ? 0 : b == q ? 0 : b.hasPrefix(q) ? 1 : 2 }
        return tickers
            .filter { (q.isEmpty || $0.base.contains(q)) && (!favsOnly || !q.isEmpty || favs.contains($0.base)) }
            .sorted(using: sort)
            .sorted { rank($0.base) < rank($1.base) }
    }

    var body: some View {
        let rows = rows
        VStack(alignment: .leading, spacing: 10) {
            TextField(L("Search pairs"), text: $query)
                .textFieldStyle(.roundedBorder)
                .focused($focused)
                .onSubmit { if let b = sel.flatMap({ s in rows.first { $0.base == s } })?.base ?? rows.first?.base { pick(b) } }
                .onKeyPress(.downArrow) { move(1, rows) }
                .onKeyPress(.upArrow) { move(-1, rows) }
                .onChange(of: query) { sel = rows.first?.base }
            HStack {
                Picker("", selection: $favsOnly) {
                    Text(L("Favorites")).tag(true)
                    Text(L("All")).tag(false)
                }
                .pickerStyle(.segmented)
                .labelsHidden()
                .fixedSize()
                .disabled(!query.isEmpty)
                Spacer()
                Text("⌘[ ⌘]  \(L("favorites"))  ·  ⌘D  \(L("star"))").font(.caption2).foregroundStyle(.tertiary)
            }
            let recent = Pairs.recent.filter { $0 != current }.prefix(6)
            if !recent.isEmpty && query.isEmpty {
                HStack(spacing: 6) {
                    Text(L("Recent")).font(.caption).foregroundStyle(.secondary)
                    ForEach(Array(recent), id: \.self) { b in
                        Button { pick(b) } label: {
                            HStack(spacing: 4) { CoinIcon(base: b, size: 13); Text(b) }
                        }
                        .buttonStyle(.bordered).controlSize(.small)
                    }
                }
            }

            if rows.isEmpty {
                Text(tickers.isEmpty ? L("Loading…") : favsOnly && query.isEmpty ? L("No favorites yet: star a pair in All") : L("No matching pairs"))
                    .foregroundStyle(.secondary)
                    .frame(maxWidth: .infinity, maxHeight: .infinity)
            } else {
                // a click selects = picks; arrow keys from the search field only move `sel`
                ScrollViewReader { sv in
                    Table(rows, selection: Binding(get: { sel }, set: { if let b = $0 { pick(b) } }), sortOrder: $sort) {
                        TableColumn("") { t in
                            Button { toggle(t.base) } label: {
                                Image(systemName: favs.contains(t.base) ? "star.fill" : "star")
                                    .foregroundStyle(favs.contains(t.base) ? AnyShapeStyle(.yellow) : AnyShapeStyle(.tertiary))
                            }
                            .buttonStyle(.borderless)
                        }
                        .width(20)
                        TableColumn(L("Pair"), value: \.base) { t in
                            HStack(spacing: 6) {
                                CoinIcon(base: t.base, size: 16)
                                Text(t.base).fontWeight(.medium) + Text("USDT").foregroundStyle(.secondary)
                                if t.base == current { Image(systemName: "checkmark").font(.caption2).foregroundStyle(.secondary) }
                            }
                        }
                        TableColumn(L("Last"), value: \.last) { t in
                            Text(Fmt.px(t.last)).monospacedDigit().frame(maxWidth: .infinity, alignment: .trailing)
                        }
                        .width(96)
                        TableColumn(L("24h Chg"), value: \.chg_pct) { t in
                            Text(String(format: "%+.2f%%", t.chg_pct)).monospacedDigit()
                                .foregroundStyle(t.chg_pct >= 0 ? T1.up : T1.down)
                                .frame(maxWidth: .infinity, alignment: .trailing)
                        }
                        .width(70)
                        TableColumn(L("Volume"), value: \.quote_vol) { t in
                            Text(Fmt.big(t.quote_vol)).monospacedDigit().foregroundStyle(.secondary)
                                .frame(maxWidth: .infinity, alignment: .trailing)
                        }
                        .width(64)
                    }
                    .tableStyle(.inset(alternatesRowBackgrounds: false))
                    .onChange(of: sel) { if let s = sel { sv.scrollTo(s) } }
                }
            }
        }
        .padding(12)
        .frame(width: 480, height: 560)
        .onAppear { focused = true; sel = current }
        .task {
            // refresh while open: last / change / volume move
            while !Task.isCancelled {
                if let r = store.call("tickers", as: TickersReply.self) { tickers = r.tickers }
                try? await Task.sleep(for: .seconds(1))
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
        Pairs.toggleFavorite(b)
        favs = Set(Pairs.favorites)
    }

    private func pick(_ b: String) {
        Pairs.open(b)
        close()
    }
}

/// Favorites, recently opened pairs and pair switching, shared by the picker and the Market menu.
@MainActor enum Pairs {
    static let modes: [(id: String, label: String)] = [("Spot", "Spot"), ("Margin", "Margin"), ("Perp", "Perpetual"), ("Option", "Options")]
    /// Control-1 … Control-6
    static let intervals: [(Int, String)] = [(1, "1m"), (5, "5m"), (15, "15m"), (60, "1H"), (240, "4H"), (1440, "1D")]

    static var favorites: [String] { UserDefaults.standard.stringArray(forKey: "t1.favorites") ?? ["BTC", "ETH", "SOL"] }
    /// most recent first, current pair included
    static var recent: [String] { UserDefaults.standard.stringArray(forKey: "t1.recent") ?? [] }

    static func toggleFavorite(_ b: String) {
        var f = favorites
        if let i = f.firstIndex(of: b) { f.remove(at: i) } else { f.append(b) }
        UserDefaults.standard.set(f, forKey: "t1.favorites")
    }

    static func open(_ b: String) {
        let cur = Store.shared.state.base
        if b != cur { Store.shared.call("set_base", ["base": b]) }
        let r = [b] + ([cur] + recent).filter { $0 != b }
        UserDefaults.standard.set(Array(NSOrderedSet(array: r).array.prefix(10)) as? [String], forKey: "t1.recent")
    }

    /// Cycle through favorites in their saved order.
    static func step(_ d: Int) {
        let f = favorites
        guard !f.isEmpty else { return }
        let i = f.firstIndex(of: Store.shared.state.base).map { ($0 + d + f.count) % f.count } ?? 0
        open(f[i])
    }
}
