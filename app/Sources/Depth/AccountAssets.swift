import SwiftUI

// Assets tab (every account per keyed venue, loaded on demand) and the Transfer sheet with the
// auto top-up rule. Transfers never leave the venue: this app has no withdrawals.

enum AccountWallet {
    /// accounts that margin perps (mirror of trade::MARGIN_IDS)
    static let marginIds = ["UNIFIED", "PM", "USDM"]

    @MainActor static func name(_ id: String) -> String {
        switch id {
        case "UNIFIED": L("Unified Trading")
        case "FUND", "FUNDING": L("Funding")
        case "EARN": L("Earn (Flexible)")
        case "PM": L("Portfolio Margin")
        case "USDM": L("USDS-M Futures")
        case "SPOT": L("Spot")
        default: id
        }
    }

    /// Where `from` can transfer to (mirror of trade::destinations).
    static func destinations(_ ex: String, _ from: String, pm: Bool) -> [String] {
        let margin = pm ? "PM" : "USDM"
        switch (ex, from) {
        case ("Bybit", "UNIFIED"): return ["FUND"]
        case ("Bybit", "FUND"): return ["UNIFIED"]
        case ("Bybit", "EARN"): return ["UNIFIED", "FUND"]
        case ("Binance", "SPOT"): return [margin, "FUNDING"]
        case ("Binance", "FUNDING"): return [margin, "SPOT"]
        case ("Binance", "PM"), ("Binance", "USDM"): return ["SPOT", "FUNDING"]
        case ("Binance", "EARN"): return [margin, "SPOT", "FUNDING"]
        default: return []
        }
    }

    /// Short reason for an account that could not be read (raw text goes in the tooltip).
    @MainActor static func noteHint(_ n: String) -> String {
        ["10005", "Permission", "-2015", "permission"].contains { n.contains($0) } ? L("API key lacks read permission") : L("Read failed")
    }

    static func movableUsd(_ w: WalletAccount) -> Double {
        w.coins.filter { $0.free > 0 }.map { $0.qty > 0 ? $0.usd * $0.free / $0.qty : 0 }.reduce(0, +)
    }
}

struct AccountAssetsView: View {
    @Environment(Store.self) private var store
    let state: AppState
    let onTransfer: (String) -> Void
    @State private var requested: Set<String> = []

    var body: some View {
        ScrollView {
            LazyVGrid(columns: [GridItem(.adaptive(minimum: 420), spacing: 10, alignment: .top)], alignment: .leading, spacing: 10) {
                ForEach(state.wallets) { w in card(w) }
            }
            .padding(10)
        }
        .onAppear(perform: loadMissing)
        .onChange(of: state.wallets.map(\.ex)) { loadMissing() }
    }

    /// Each venue's wallets are fetched once when the tab is first shown (Refresh re-fetches).
    private func loadMissing() {
        for w in state.wallets where w.updated_ms == nil && !w.loading && !requested.contains(w.ex) {
            requested.insert(w.ex)
            store.call("load_wallets", ["ex": w.ex])
        }
    }

    private func card(_ w: VenueWallets) -> some View {
        GroupBox {
        VStack(alignment: .leading, spacing: 6) {
            HStack(spacing: 8) {
                VenueIcon(ex: w.ex, size: 16)
                Text(w.ex).font(uiFont(13, .semibold)).foregroundStyle(Color.primary)
                if w.updated_ms != nil {
                    Text("≈ \(Fmt.usd(max(0, w.accounts.map(\.usd).reduce(0, +)))) USD").font(numFont(12, .medium)).foregroundStyle(Color.primary)
                }
                if let a = w.auto, a.enabled {
                    AccountBadge(text: "\(L("Auto top-up")) < \(Fmt.usd(a.min, 0)) → \(Fmt.usd(a.target, 0))", color: Color.t1Up)
                }
                Spacer()
                if w.loading {
                    ProgressView().controlSize(.mini)
                } else if let at = w.updated_ms {
                    TimelineView(.periodic(from: .now, by: 1)) { ctx in
                        Text("\(max(0, Int(ctx.date.timeIntervalSince1970 * 1000 - Double(at)) / 1000))s").font(numFont(10.5)).foregroundStyle(Color.t1Dim)
                    }
                    Button { store.call("load_wallets", ["ex": w.ex]) } label: { Image(systemName: "arrow.clockwise") }
                        .buttonStyle(.borderless).help(L("Refresh"))
                }
                Button { onTransfer(w.ex) } label: { Label(L("Transfer"), systemImage: "arrow.left.arrow.right") }
                    .buttonStyle(.bordered).controlSize(.small)
            }
            Divider()
            if w.updated_ms == nil {
                Text(w.loading ? L("Reading accounts…") : L("Not loaded")).font(uiFont(11.5)).foregroundStyle(Color.t1Dim).padding(.vertical, 6)
            }
            ForEach(w.accounts) { a in accountRow(a) }
            ForEach(w.notes, id: \.self) { n in
                let id = String(n.split(separator: ":").first ?? "")
                Label("\(AccountWallet.name(id)): \(AccountWallet.noteHint(n))", systemImage: "exclamationmark.triangle")
                    .font(uiFont(11)).foregroundStyle(Color.orange).help(n)
            }
        }
        .padding(4)
        }
    }

    private func accountRow(_ a: WalletAccount) -> some View {
        let margin = AccountWallet.marginIds.contains(a.id)
        // dust (under a cent) is not worth a slot
        let coins = a.coins.filter { $0.usd >= 0.01 || ($0.usd == 0 && $0.qty >= 1e-4) }.sorted { $0.usd > $1.usd }
        return HStack(spacing: 10) {
            HStack(spacing: 5) {
                Circle().fill(margin ? Color.accentColor : Color.t1Dim).frame(width: 5, height: 5)
                Text(AccountWallet.name(a.id)).font(uiFont(11.5, margin ? .medium : .regular)).foregroundStyle(margin ? Color.primary : Color.secondary)
            }
            .frame(width: 130, alignment: .leading)
            Text(Fmt.usd(max(0, a.usd))).font(numFont(11.5)).foregroundStyle(Color.primary).frame(width: 90, alignment: .trailing)
            HStack(spacing: 10) {
                ForEach(coins.prefix(3)) { c in
                    HStack(spacing: 3) {
                        CoinIcon(base: c.coin, size: 12)
                        Text(Fmt.qty(c.qty)).font(numFont(10.5)).foregroundStyle(Color.secondary)
                        Text(c.coin).font(uiFont(10.5)).foregroundStyle(Color.t1Dim)
                    }
                    .fixedSize()
                    .help("\(c.coin) \(Fmt.qty(c.qty)) · \(L("free")) \(Fmt.qty(c.free)) · ≈\(Fmt.usd(c.usd)) USD")
                }
                if coins.count > 3 { Text("+\(coins.count - 3)").font(numFont(10.5)).foregroundStyle(Color.t1Dim) }
            }
            Spacer(minLength: 0)
        }
        .lineLimit(1)
        .frame(height: 20)
    }
}

// MARK: transfer sheet

struct AccountTransferSheet: View {
    @Environment(Store.self) private var store
    @Environment(\.dismiss) private var dismiss
    @State var ex: String
    @State private var from = ""
    @State private var to = ""
    @State private var coin = "USDT"
    @State private var amount = ""
    @State private var auto = AutoRule(enabled: false, min: 0, target: 0)
    @State private var autoLoaded = false
    @State private var error: String?

    var body: some View {
        let s = store.state
        let wallet = s.wallets.first { $0.ex == ex }
        let ws = wallet?.accounts ?? []
        let sources = ws.filter { $0.coins.contains { $0.free > 0 } }
        let dests = AccountWallet.destinations(ex, from, pm: s.binance_pm)
        let coins = ws.first { $0.id == from }?.coins.filter { $0.free > 0 } ?? []
        let c = coins.first { $0.coin == coin }
        let free = c?.free ?? 0
        let amt = Double(amount.trimmingCharacters(in: .whitespaces)) ?? 0
        let valid = amt > 0 && amt <= free * (1 + 1e-9) && !to.isEmpty && c.map { from != "EARN" || $0.product != nil } == true

        Form {
                Section {
                    Picker(L("Exchange"), selection: $ex) {
                        ForEach(s.wallets) { w in Text(w.ex).tag(w.ex) }
                    }
                    if wallet?.updated_ms == nil {
                        HStack { ProgressView().controlSize(.small); Text(L("Reading accounts…")).foregroundStyle(Color.secondary) }
                    }
                    Picker(L("From"), selection: $from) {
                        ForEach(sources) { w in Text("\(AccountWallet.name(w.id))   \(Fmt.usd(w.usd)) USD").tag(w.id) }
                        if !sources.contains(where: { $0.id == from }) { Text(from.isEmpty ? "—" : AccountWallet.name(from)).tag(from) }
                    }
                    Picker(L("To"), selection: $to) {
                        ForEach(dests, id: \.self) { d in Text(AccountWallet.name(d)).tag(d) }
                        if !dests.contains(to) { Text("—").tag(to) }
                    }
                    Picker(L("Coin"), selection: $coin) {
                        ForEach(coins) { c in Text("\(c.coin)   \(Fmt.qty(c.free))").tag(c.coin) }
                        if !coins.contains(where: { $0.coin == coin }) { Text(coin.isEmpty ? "—" : coin).tag(coin) }
                    }
                    LabeledContent(L("Amount")) {
                        HStack(spacing: 8) {
                            TextField("", text: $amount, prompt: Text("0.00"))
                                .textFieldStyle(.roundedBorder).font(numFont(12.5)).multilineTextAlignment(.trailing).frame(width: 150)
                            Button(L("Max")) { amount = String(free) }.controlSize(.small)
                        }
                    }
                } footer: {
                    VStack(alignment: .leading, spacing: 4) {
                        Text("\(L("Available")) \(Fmt.qty(free)) \(coin)").foregroundStyle(Color.t1Dim)
                        if amt > free * (1 + 1e-9) { note(L("Amount exceeds the available balance"), Color.t1Down) }
                        // multi-step and slow routes are spelled out before confirming
                        if from == "EARN" { note(L("Redeems from Flexible Earn, usually instant; a redemption cannot be undone."), Color.orange) }
                        if ex == "Binance" && [("FUNDING", "PM"), ("PM", "FUNDING"), ("EARN", "PM"), ("EARN", "USDM")].contains(where: { $0 == (from, to) }) {
                            note(L("No direct route: moves to Spot first, then into the target account (two steps)."), Color.orange)
                        }
                        if from == "EARN", let c, c.product == nil { note(L("On-chain Earn must be redeemed on the exchange website (takes days)."), Color.t1Down) }
                        if let error { note(error, Color.t1Down) }
                    }
                    .font(uiFont(11))
                }

                Section {
                    Toggle(L("Top up margin automatically"), isOn: $auto.enabled)
                    LabeledContent(L("When available is below")) { usdField($auto.min) }
                    LabeledContent(L("Top up to")) { usdField($auto.target) }
                    HStack {
                        Spacer()
                        Button(L("Save Rule")) {
                            if store.call("set_auto", ["ex": ex, "enabled": auto.enabled, "min": auto.min, "target": auto.target])?["ok"] as? Bool != true { error = store.lastError }
                        }
                        .disabled(!autoChanged(wallet) || (auto.enabled && auto.target <= auto.min))
                    }
                } header: {
                    Text(L("Auto top-up"))
                } footer: {
                    VStack(alignment: .leading, spacing: 4) {
                        Text("\(L("At most once a minute per venue; every transfer is written to the order log. Sources in order:")) \(ex == "Bybit" ? L("Funding → Flexible Earn") : L("Spot → Funding → Flexible Earn"))")
                            .foregroundStyle(Color.t1Dim)
                        if auto.enabled && auto.target <= auto.min { note(L("Target must be above the trigger"), Color.t1Down) }
                    }
                    .font(uiFont(11))
                }
        }
        .formStyle(.grouped)
        .navigationTitle(L("Transfer"))
        .toolbar {
            ToolbarItem(placement: .cancellationAction) { Button(L("Cancel")) { dismiss() } }
            ToolbarItem(placement: .confirmationAction) {
                Button(L("Confirm Transfer")) {
                    var args: [String: Any] = ["ex": ex, "from": from, "to": to, "coin": coin, "amount": min(amt, free)]
                    if let p = c?.product { args["product"] = p }
                    if store.call("transfer", args)?["ok"] as? Bool == true { dismiss() } else { error = store.lastError }
                }
                .disabled(!valid || store.state.transferring)
            }
        }
        .frame(width: 480, height: 600)
        .onAppear { pickDefaults(ws); loadAuto(wallet) }
        .onChange(of: ws.map(\.id)) { pickDefaults(accounts) }
        .onChange(of: ex) {
            from = ""; to = ""; amount = ""; autoLoaded = false
            let w = store.state.wallets.first { $0.ex == ex }
            pickDefaults(accounts); loadAuto(w)
            if let w, w.updated_ms == nil, !w.loading { store.call("load_wallets", ["ex": ex]) }
        }
        .onChange(of: from) {
            let d = AccountWallet.destinations(ex, from, pm: store.state.binance_pm)
            if !d.contains(to) { to = d.first ?? "" }
            let cs = accounts.first { $0.id == from }?.coins.filter { $0.free > 0 } ?? []
            if !cs.contains(where: { $0.coin == coin }) { coin = cs.first?.coin ?? "" }
        }
    }

    private var accounts: [WalletAccount] { store.state.wallets.first { $0.ex == ex }?.accounts ?? [] }

    private func note(_ t: String, _ c: Color) -> some View {
        Text(t).foregroundStyle(c).fixedSize(horizontal: false, vertical: true)
    }

    private func usdField(_ v: Binding<Double>) -> some View {
        HStack(spacing: 6) {
        TextField("", value: v, format: .number.precision(.fractionLength(0...2)))
            .textFieldStyle(.roundedBorder).font(numFont(12)).multilineTextAlignment(.trailing).frame(width: 120)
        Text("USD").foregroundStyle(.secondary)
        }
    }

    private func autoChanged(_ w: VenueWallets?) -> Bool {
        let cur = w?.auto ?? AutoRule(enabled: false, min: 0, target: 0)
        return cur.enabled != auto.enabled || cur.min != auto.min || cur.target != auto.target
    }

    private func loadAuto(_ w: VenueWallets?) {
        guard !autoLoaded else { return }
        autoLoaded = true
        auto = w?.auto ?? AutoRule(enabled: false, min: 0, target: 0)
    }

    /// Default: from the non-margin account with the most that can move, into the margin account.
    private func pickDefaults(_ ws: [WalletAccount]) {
        guard from.isEmpty, !ws.isEmpty else { return }
        let margin = ws.first { AccountWallet.marginIds.contains($0.id) }?.id ?? ""
        let best = ws.filter { $0.id != margin }.max { AccountWallet.movableUsd($0) < AccountWallet.movableUsd($1) }
        from = best.flatMap { AccountWallet.movableUsd($0) > 0 ? $0.id : nil } ?? margin
        let dests = AccountWallet.destinations(ex, from, pm: store.state.binance_pm)
        to = dests.contains(margin) ? margin : dests.first ?? ""
        let coins = ws.first { $0.id == from }?.coins.filter { $0.free > 0 } ?? []
        if !coins.contains(where: { $0.coin == coin }) { coin = coins.first?.coin ?? "" }
    }
}
