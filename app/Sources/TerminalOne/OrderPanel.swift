import SwiftUI

/// Order entry for the current base's USDT perp, built as a compact grouped Form (hosted as the
/// window's trailing inspector). Every price comes from the trade venue's own book
/// (`trade.bid/ask`); the composite (`header.price`) never prices an order.
struct OrderPanel: View {
    @Environment(Store.self) private var store
    @FocusState private var focus: OrderFieldID?

    @State private var close = false
    /// T1_ORDER_KIND preselects a type (screenshots)
    @State private var kind = OrderKind(rawValue: ProcessInfo.processInfo.environment["T1_ORDER_KIND"] ?? "") ?? .limit
    @State private var price = ""
    @State private var qty = ""
    @State private var pct = 0.0
    @State private var bboOn = false
    @State private var bboQueue = false
    @State private var bboLevel = 1
    @State private var tpslOn = false
    @State private var tp = ""
    @State private var sl = ""
    @State private var trigLast = false
    /// side of the last Buy/Sell click: Enter in a field repeats it
    @State private var lastPos: String?
    @State private var inlineError: String?
    @State private var ticket: OrderTicket?
    @State private var tpslFor: PositionRow?
    /// limit time in force: gtc, ioc, fok, post (post-only / ALO)
    @State private var tif = "gtc"
    // conditional
    @State private var trigger = ""
    @State private var triggerLast = false
    @State private var stopLimit = false
    // trailing
    @State private var callback = "1.0"
    @State private var activation = ""
    // scaled
    @State private var scaledFrom = ""
    @State private var scaledTo = ""
    @State private var scaledCount = 5
    @State private var scaledSkew = 0.0
    @State private var scaledPost = true
    // twap
    @State private var twapMinutes = "15"
    @State private var twapSlices = "10"
    @State private var twapSlip = "10"
    @State private var twapLimit = ""
    @State private var algoConfirm: AlgoTicket?
    private let pending = OrderPendingTpSl.shared

    private var s: AppState { store.state }
    private var t: TradeCtx { s.trade }
    private var useBbo: Bool { kind == .limit && bboOn && !t.bbo_levels.isEmpty }
    /// a typed limit price is sent: limit (not BBO) and stop-limit
    private var typedPrice: Bool { (kind == .limit && !useBbo) || (kind == .stop && stopLimit) }
    private var kindArg: String { useBbo ? "bbo" : kind.rawValue }
    private var mid: Double? { if let b = t.bid, let a = t.ask { return (b + a) / 2 }; return t.bid ?? t.ask }
    /// reference price for size and cost: the typed limit, else the venue's own mid
    private var refPx: Double {
        switch kind {
        case .stop: return (stopLimit ? OrderNum.parse(price) : nil) ?? OrderNum.parse(trigger) ?? mid ?? 0
        case .scaled: if let a = OrderNum.parse(scaledFrom), let b = OrderNum.parse(scaledTo) { return (a + b) / 2 }; return mid ?? 0
        default: return (typedPrice ? OrderNum.parse(price) : nil) ?? mid ?? 0
        }
    }
    private var lev: Double { max(t.lev ?? 1, 1) }
    private var qtyV: Double { OrderNum.parse(qty) ?? 0 }
    private var positions: [PositionRow] { s.positions.filter { $0.symbol == t.symbol && (s.route.smart || $0.ex == t.venue) } }
    private func held(_ side: String) -> Double { positions.filter { $0.side == side }.reduce(0) { $0 + $1.qty } }
    /// 100% of the slider: margin x leverage when opening, the larger held side when closing
    private var maxQty: Double { close ? max(held("long"), held("short")) : refPx > 0 ? (t.available ?? 0) * lev / refPx : 0 }
    private var canTrade: Bool { t.has_key || s.route.smart }

    var body: some View {
      // cards size to their content; the column scrolls as a whole when the window is short
      ScrollView {
       VStack(spacing: 8) {
          VStack(alignment: .leading, spacing: 14) {
            header
            GlassSegments(selection: $close, items: [(false, L("Open")), (true, L("Close"))])
            typeTabs
            Divider()
            order
            Divider()
            if !close && (kind == .limit || kind == .market) { tpslSection }
            let jobs = s.algos.filter { !$0.cancelled && $0.status != "done" || terminal_recent($0) }
            if !jobs.isEmpty { AlgoJobsView(jobs: jobs) }
            buttons
            summary
            if let e = inlineError { OrderNote(text: e, color: .red, icon: "xmark.octagon.fill") }
            let waiting = pending.items.filter { $0.symbol == t.symbol }
            if !waiting.isEmpty {
                PanelSection(L("TP/SL waiting for fill")) { ForEach(waiting) { pendingRow($0) } }
            }
            ForEach(positions) { positionLine($0) }
          }
          .padding(14)
          .glassCard()
          accountCard
       }
      }
      .scrollIndicators(.never)
        .toggleStyle(RowSwitch())
        .labeledContentStyle(RowLabeled())
        .onChange(of: s.book_click) {
            // a level clicked in the venue's own book becomes the limit price
            guard let c = s.book_click, c.ex == t.venue else { return }
            kind = .limit; bboOn = false; price = OrderNum.px(c.px, tick: t.tick)
        }
        .onChange(of: t.bid, initial: true) { if price.isEmpty, let b = t.bid { price = OrderNum.px(b, tick: t.tick) } }
        .onChange(of: t.symbol) { price = t.bid.map { OrderNum.px($0, tick: t.tick) } ?? ""; qty = ""; pct = 0; tp = ""; sl = ""; inlineError = nil }
        .onChange(of: close) { pct = 0; qty = ""; inlineError = nil }
        .onChange(of: t.bbo_levels, initial: true) { if !t.bbo_levels.contains(bboLevel) { bboLevel = t.bbo_levels.first ?? 1 } }
        .onChange(of: s.positions.map { "\($0.id)|\($0.qty)" }) { pending.check(s.positions, orders: s.orders) }
        .sheet(item: $ticket) { OrderConfirm(ticket: $0) }
        .sheet(item: $tpslFor) { OrderTpSlSheet(p: $0) }
        .confirmationDialog(algoConfirm?.title ?? "", isPresented: Binding(get: { algoConfirm != nil }, set: { if !$0 { algoConfirm = nil } }), titleVisibility: .visible, presenting: algoConfirm) { a in
            Button(L("Cancel"), role: .cancel) {}
            Button(L("Confirm")) {
                if store.call(a.op, a.args)?["ok"] as? Bool != true { inlineError = store.lastError ?? L("Failed") }
            }
        } message: { a in Text(a.message + (t.verified ? "" : "\n" + L("This venue is untested: start with the minimum size."))) }
    }

    // MARK: routing / account

    private var smartVenues: [String] {
        s.tradable.filter { ex in s.keys.contains { $0.ex == ex && $0.configured } && s.venues.first { $0.ex == ex }?.on != false }
    }

    /// Account card pinned under the order form (Binance: Account / Portfolio Margin Info / balance).
    @ViewBuilder private var accountCard: some View {
        if let b = s.balances.first(where: { $0.ex == t.venue }) {
            let upnl = s.positions.filter { $0.ex == t.venue }.reduce(0) { $0 + $1.upnl }
            VStack(alignment: .leading, spacing: 9) {
                HStack {
                    Text(L("Account")).font(.headline)
                    Spacer()
                    Button { NotificationCenter.default.post(name: .init("T1OpenTransfer"), object: t.venue) } label: {
                        Label(L("Transfer"), systemImage: "arrow.left.arrow.right")
                    }
                    .buttonStyle(.borderless).help(L("Move funds between this venue's accounts"))
                }
                Divider()
                if let u = b.uni_mmr {
                    LabeledContent(L("UniMMR")) {
                        Text(Fmt.num(u, 2)).font(.title3.weight(.semibold)).monospacedDigit()
                            .foregroundStyle(u < 1.5 ? Color.red : u < 3 ? Color.orange : Color.green)
                    }
                    .help(L("Portfolio Margin: the whole account is liquidated when UniMMR falls to 1.05"))
                } else if let r = b.mm_rate {
                    LabeledContent(L("Margin ratio")) {
                        Text(String(format: "%.2f%%", r * 100)).font(.title3.weight(.semibold)).monospacedDigit()
                            .foregroundStyle(r > 0.7 ? Color.red : r > 0.4 ? Color.orange : Color.green)
                    }
                    .help(L("Maintenance margin / margin balance; liquidation at 100%"))
                }
                if let m = b.maint_margin { row(L("Maintenance margin"), "\(Fmt.usd(m)) USD") }
                if let e = b.adj_equity { row(L("Adjusted equity"), "\(Fmt.usd(e)) USD") }
                row(L("Equity"), "\(Fmt.usd(b.equity)) USD")
                row(L("Unrealized PnL"), "\(Fmt.signed(upnl)) USD", upnl > 0 ? T1.up : upnl < 0 ? T1.down : nil)
                if !t.has_key {
                    Button(L("Add API keys")) { openSettings("keys") }
                }
                if let e = t.error { OrderNote(text: e, color: .red, icon: "exclamationmark.octagon.fill") }
            }
            .font(.callout)
            .padding(14)
            .glassCard()
        } else if !t.has_key {
            VStack(alignment: .leading, spacing: 8) {
                Label(L("No API key for this venue"), systemImage: "key.slash").foregroundStyle(.orange)
                Button(L("Add API keys")) { openSettings("keys") }
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(14)
            .glassCard()
        }
    }

    private func row(_ k: String, _ v: String, _ c: Color? = nil) -> some View {
        LabeledContent(k) { Text(v).monospacedDigit().foregroundStyle(c ?? .primary) }
    }

    /// This symbol's position in one line, with its actions in a menu (details are in the positions table).
    private func positionLine(_ p: PositionRow) -> some View {
        let margin = p.margin > 0 ? p.margin : p.entry * p.qty / max(p.lev, 1)
        return HStack(spacing: 6) {
            Text(p.isLong ? L("Long") : L("Short")).fontWeight(.semibold).foregroundStyle(p.isLong ? OrderColor.up : OrderColor.down)
            Text("\(Fmt.qty(p.qty)) \(p.base)").monospacedDigit()
            Spacer()
            Text("\(Fmt.signed(p.upnl))").monospacedDigit().foregroundStyle(p.upnl >= 0 ? OrderColor.up : OrderColor.down)
                .help(margin > 0 ? String(format: "ROE %+.2f%%", p.upnl / margin * 100) : "")
            Menu {
                Button(L("TP/SL…")) { tpslFor = p }
                Button(L("Limit close")) { limitClose(p) }
                Button(L("Market close…"), role: .destructive) { marketClose(p) }
            } label: { Image(systemName: "ellipsis.circle") }
            .menuStyle(.borderlessButton).menuIndicator(.hidden).fixedSize()
        }
        .font(.callout)
        .padding(.horizontal, 12).padding(.vertical, 9)
        .background(.fill.quaternary, in: .rect(cornerRadius: 10))
    }

    // MARK: order form

    private var order: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack(spacing: 6) {
                Text(L("Avbl")).foregroundStyle(.secondary)
                Text(t.available.map { "\(Fmt.usd($0)) USDT" } ?? "–").monospacedDigit()
                if t.has_key {
                    Button { NotificationCenter.default.post(name: .init("T1OpenTransfer"), object: t.venue) } label: {
                        Image(systemName: "arrow.left.arrow.right").foregroundStyle(Color.accentColor)
                    }
                    .buttonStyle(.plain).help(L("Move funds between accounts on this venue"))
                }
                Spacer()
            }
            .font(.callout)
            kindFields
            OrderField(label: L("Size"), text: $qty, unit: s.base, id: .qty, focus: $focus, onSubmit: enter)
                .onChange(of: qty) { if focus == .qty { pct = maxQty > 0 ? min(qtyV / maxQty, 1) : 0 } }
            HStack(spacing: 10) {
                Slider(value: Binding(get: { pct * 100 }, set: { pct = $0 / 100; qty = pct > 0 ? OrderNum.qty(maxQty * pct, step: t.step) : "" }), in: 0...100, step: 25)
                    .labelsHidden()
                Text(String(format: "%.0f%%", pct * 100)).monospacedDigit().foregroundStyle(.secondary).frame(width: 38, alignment: .trailing)
            }
            .help(close ? L("Share of the position") : L("Share of available margin × leverage"))
        }
    }

    /// Venue and its account mode / leverage (read-only: changed on the exchange), like Binance's chips.
    private var header: some View {
        HStack(spacing: 6) {
            chip(t.mode.map { $0 == "hedge" ? L("Hedge") : L("One-Way") } ?? "–")
                .help(L("Position mode and leverage for this symbol (change them on the exchange)"))
            chip(t.lev.map { String(format: "%gx", $0) } ?? "–")
            if !t.verified {
                Image(systemName: "exclamationmark.triangle.fill").foregroundStyle(.orange)
                    .help(L("Order placement on this venue is not yet verified on a live account: start with the minimum size."))
            }
            Spacer()
            Menu {
                Button(L("Order routing…")) { openSettings("trading") }
                Button(L("API keys…")) { openSettings("keys") }
            } label: {
                HStack(spacing: 5) {
                    if s.route.smart {
                        Image(systemName: "arrow.triangle.branch")
                        Text(L("Smart"))
                    } else {
                        Text(t.venue)
                    }
                }
            }
            .menuStyle(.borderlessButton).fixedSize()
            .help("\(L("Push")) \(ms(t.push_ms)) · \(L("RTT")) \(ms(t.rtt_ms))")
        }
        .font(.callout)
    }

    private func chip(_ text: String) -> some View {
        Text(text).font(.callout.weight(.medium)).monospacedDigit().lineLimit(1).fixedSize()
            .padding(.horizontal, 14).padding(.vertical, 5)
            .background(.fill.tertiary, in: .rect(cornerRadius: 7))
    }

    /// Limit / Market / Post Only as text tabs with an accent underline.
    private var typeTabs: some View {
        let c = t.caps
        let main: [(OrderKind, String)] = [(.limit, L("Limit")), (.market, L("Market"))] + (c.stop ? [(.stop, L("Conditional"))] : [])
        let more: [(OrderKind, String)] = (c.trailing ? [(.trailing, L("Trailing Stop"))] : []) + [(.scaled, L("Scaled")), (.twap, L("TWAP"))]
        let inMore = more.contains { $0.0 == kind }
        return HStack(spacing: 14) {
            ForEach(main, id: \.0) { k, name in tab(name, kind == k) { kind = k } }
            Menu {
                ForEach(more, id: \.0) { k, name in Button(name) { kind = k } }
            } label: {
                VStack(spacing: 5) {
                    Text((inMore ? (more.first { $0.0 == kind }?.1 ?? "") : L("More")) + " ▾")
                        .font(.body.weight(inMore ? .semibold : .regular)).foregroundStyle(inMore ? .primary : .secondary)
                        .lineLimit(1).fixedSize()
                    Capsule().fill(inMore ? Color.accentColor : .clear).frame(width: 18, height: 3)
                }
            }
            .menuStyle(.button).buttonStyle(.plain).menuIndicator(.hidden).fixedSize()
            Spacer()
        }
        .onChange(of: t.venue) { if (kind == .stop && !c.stop) || (kind == .trailing && !c.trailing) { kind = .limit } }
    }

    private func tab(_ name: String, _ on: Bool, _ action: @escaping () -> Void) -> some View {
        Button(action: action) {
            VStack(spacing: 5) {
                Text(name).font(.body.weight(on ? .semibold : .regular)).foregroundStyle(on ? .primary : .secondary)
                    .lineLimit(1).fixedSize()
                Capsule().fill(on ? Color.accentColor : .clear).frame(width: 18, height: 3)
            }
        }
        .buttonStyle(.plain)
    }

    /// The kind-specific inputs between "Avbl" and the size.
    @ViewBuilder private var kindFields: some View {
        switch kind {
        case .limit:
            priceField
            HStack(spacing: 6) {
                Text(L("Time in force")).foregroundStyle(.secondary)
                Spacer()
                Picker(L("Time in force"), selection: $tif) {
                    Text(L("GTC · good till cancelled")).tag("gtc")
                    Text(L("IOC · fill what's there, cancel the rest")).tag("ioc")
                    if t.caps.fok { Text(L("FOK · fill completely or cancel")).tag("fok") }
                    Text(L("Post only (ALO) · maker or cancel")).tag("post")
                }
                .pickerStyle(.menu).labelsHidden().fixedSize().disabled(useBbo)
                .help(useBbo ? L("BBO orders are good till cancelled") : L("How long the order may rest on the book"))
            }
            .font(.callout)
        case .market:
            priceField
        case .stop:
            OrderField(label: L("Trigger price"), text: $trigger, unit: "USDT", id: .price, focus: $focus, onSubmit: enter) {
                Picker(L("Trigger by"), selection: $triggerLast) { Text(L("Mark")).tag(false); Text(L("Last")).tag(true) }
                    .pickerStyle(.menu).labelsHidden().fixedSize().buttonStyle(.borderless)
                    .help(L("Which price must reach the trigger: mark (harder to spike) or last trade"))
            }
            Toggle(L("Limit order when triggered"), isOn: $stopLimit)
            if stopLimit {
                OrderField(label: L("Limit price"), text: $price, unit: "USDT", id: .tp, focus: $focus, onSubmit: enter)
            }
            OrderNote(text: L("Fires once the trigger is reached: above the price it acts as a buy stop / sell take-profit, below it as a sell stop / buy take-profit."), color: .secondary)
        case .trailing:
            OrderField(label: L("Callback rate"), text: $callback, unit: "%", id: .price, focus: $focus, onSubmit: enter) {
                Menu { ForEach(["0.5", "1.0", "2.0", "5.0"], id: \.self) { v in Button("\(v)%") { callback = v } } } label: { Image(systemName: "chevron.down") }
                    .menuStyle(.borderlessButton).menuIndicator(.hidden).fixedSize()
            }
            OrderField(label: L("Activation price (optional)"), text: $activation, unit: "USDT", id: .tp, focus: $focus, onSubmit: enter)
            OrderNote(text: t.venue == "Bybit"
                ? L("Bybit trails an open position: use it from Close. It closes the whole position at market once price retraces by the callback from its best level.")
                : L("A market order once price retraces by the callback from its best level since activation."), color: .secondary)
        case .scaled:
            HStack(spacing: 8) {
                OrderField(label: L("From price"), text: $scaledFrom, unit: "", id: .price, focus: $focus, onSubmit: enter)
                OrderField(label: L("To price"), text: $scaledTo, unit: "", id: .tp, focus: $focus, onSubmit: enter)
            }
            Stepper(value: $scaledCount, in: 2...50) {
                HStack { Text(L("Orders")).foregroundStyle(.secondary); Spacer(); Text("\(scaledCount)").monospacedDigit() }
            }
            .font(.callout)
            VStack(alignment: .leading, spacing: 4) {
                HStack { Text(L("Size distribution")).foregroundStyle(.secondary); Spacer(); Text(scaledSkew < -0.05 ? L("Larger first") : scaledSkew > 0.05 ? L("Larger last") : L("Equal")) }
                Slider(value: $scaledSkew, in: -1...1, step: 0.25).labelsHidden()
            }
            .font(.callout)
            Toggle(L("Post only (maker or cancel)"), isOn: $scaledPost)
        case .twap:
            HStack(spacing: 8) {
                OrderField(label: L("Duration (min)"), text: $twapMinutes, unit: "", id: .price, focus: $focus, onSubmit: enter)
                OrderField(label: L("Slices"), text: $twapSlices, unit: "", id: .tp, focus: $focus, onSubmit: enter)
            }
            HStack(spacing: 8) {
                OrderField(label: L("Max slippage (bp)"), text: $twapSlip, unit: "", id: .sl, focus: $focus, onSubmit: enter)
                OrderField(label: L("Price limit (optional)"), text: $twapLimit, unit: "", id: .level(0), focus: $focus, onSubmit: enter)
            }
            OrderNote(text: String(format: L("One IOC order every %@ at the venue's best price ± your slippage; slices wait while the price is beyond your limit. Keep this pair open while it runs."),
                                   twapGap), color: .secondary)
        }
    }

    private var twapGap: String {
        guard let m = OrderNum.parse(twapMinutes), let n = Double(twapSlices), m > 0, n > 0 else { return "–" }
        let sec = m * 60 / n
        return sec >= 60 ? String(format: "%.1f min", sec / 60) : String(format: "%.0f s", sec)
    }

    /// Price input; BBO lives inside the field (Binance-style): on, the field becomes the level menu.
    @ViewBuilder private var priceField: some View {
        let bboAvail = kind == .limit && !t.bbo_levels.isEmpty
        if kind == .market {
            OrderField(label: L("Price"), text: .constant(""), unit: "", id: .price, focus: $focus, placeholder: L("Market price"), disabled: true)
        } else if useBbo {
            VStack(alignment: .leading, spacing: 6) {
            Text(L("Price")).font(.callout).foregroundStyle(.secondary)
            OrderFieldShell(active: true) {
                Picker(L("Price"), selection: bboChoice) {
                    Section(L("Counterparty")) { ForEach(t.bbo_levels, id: \.self) { Text(bboName(false, $0)).tag(BboChoice(queue: false, level: $0)) } }
                    Section(L("Queue")) { ForEach(t.bbo_levels, id: \.self) { Text(bboName(true, $0)).tag(BboChoice(queue: true, level: $0)) } }
                }
                .pickerStyle(.menu).labelsHidden().buttonStyle(.borderless).fixedSize()
                .help(bboQueue ? L("Own side of the book: rests as a maker at the best price.") : L("Opposite side of the book: fills like a taker at the price the venue sees on arrival."))
                Spacer()
                bboButton
            }
            }
        } else {
            OrderField(label: L("Price"), text: $price, unit: "USDT", id: .price, focus: $focus, onSubmit: enter) {
                if bboAvail { bboButton }
            }
        }
    }

    private var bboButton: some View {
        Button { bboOn.toggle() } label: {
            Text("BBO").font(.caption.weight(.bold))
                .padding(.horizontal, 7).padding(.vertical, 3)
                .foregroundStyle(bboOn ? Color.white : Color.secondary)
                .background(bboOn ? AnyShapeStyle(Color.accentColor) : AnyShapeStyle(.fill.secondary), in: .capsule)
        }
        .buttonStyle(.plain)
        .help(L("Priced by the venue's book when the order arrives (Counterparty: opposite side; Queue: own side; number: book level)"))
    }

    private func quickFill(_ label: String, _ v: Double?, _ c: Color) -> some View {
        Button { if let v { price = OrderNum.px(v, tick: t.tick) } } label: {
            HStack(spacing: 4) {
                Text(label).foregroundStyle(.secondary)
                Text(Fmt.px(v)).monospacedDigit().foregroundStyle(c)
            }
        }
        .buttonStyle(.bordered).controlSize(.small)
        .help(L("Use the venue's own best price"))
    }

    private func bboName(_ queue: Bool, _ level: Int) -> String { "\(queue ? L("Queue") : L("Counterparty")) \(level)" }
    private var bboChoice: Binding<BboChoice> {
        Binding(get: { BboChoice(queue: bboQueue, level: bboLevel) }, set: { bboQueue = $0.queue; bboLevel = $0.level })
    }

    private var tpslSection: some View {
        PanelSection {
            Toggle(L("TP/SL"), isOn: $tpslOn).toggleStyle(.switch)
                .help(L("Attach a take-profit / stop-loss to the position this order opens"))
            if tpslOn {
                Picker(L("Trigger"), selection: $trigLast) {
                    Text(L("Mark")).tag(false)
                    Text(L("Last")).tag(true)
                }
                .pickerStyle(.segmented)
                OrderField(label: L("Take profit"), text: $tp, unit: "USDT", id: .tp, focus: $focus, onSubmit: enter)
                OrderField(label: L("Stop loss"), text: $sl, unit: "USDT", id: .sl, focus: $focus, onSubmit: enter)
            }
        } footer: {
            if tpslOn { Text(L("Applies to the whole position once the order fills.")).font(.caption).foregroundStyle(.secondary) }
        }
    }

    private var buttons: some View {
        let (l1, l2) = close ? (L("Close Short"), L("Close Long")) : (L("Open Long"), L("Open Short"))
        return VStack(spacing: 8) {
            HStack(spacing: 8) {
                OrderBigButton(title: l1, color: OrderColor.up, enabled: canTrade) { submit(close ? "short" : "long") }
                    .help(close ? L("Buy to reduce the short position") : L("Open or add to a long position"))
                OrderBigButton(title: l2, color: OrderColor.down, enabled: canTrade) { submit(close ? "long" : "short") }
                    .help(close ? L("Sell to reduce the long position") : L("Open or add to a short position"))
            }
            HStack(alignment: .top) {
                sideFacts(close ? "short" : "long", leading: true)
                Spacer()
                sideFacts(close ? "long" : "short", leading: false)
            }
        }
    }

    private func sideFacts(_ pos: String, leading: Bool) -> some View {
        let rows: [(String, String)] = close
            ? [(L("Max"), "\(Fmt.qty(held(pos))) \(s.base)")]
            : [(L("Cost"), "\(Fmt.usd(qtyV * refPx / lev)) USDT"), (L("Max"), "\(Fmt.qty(maxQty)) \(s.base)")]
        return VStack(alignment: leading ? .leading : .trailing, spacing: 3) {
            ForEach(rows, id: \.0) { k, v in
                HStack(spacing: 4) { Text(k).foregroundStyle(.secondary); Text(v).monospacedDigit() }
            }
        }
        .font(.caption)
    }

    @ViewBuilder private var summary: some View {
        let notional = qtyV * refPx
        let taker = kind == .market || kind == .twap || kind == .trailing || (kind == .stop && !stopLimit) || (useBbo && !bboQueue) || (kind == .limit && (tif == "ioc" || tif == "fok"))
        let rate = taker ? t.fee.taker : t.fee.maker
        HStack(spacing: 4) {
            Image(systemName: "percent")
            Text("\(taker ? L("Taker") : L("Maker")) \(String(format: "%.3f%%", rate * 100))")
            Spacer()
            if notional > 0 { Text("\(Fmt.usd(notional)) USDT · \(L("fee")) ≈ \(Fmt.usd(notional * rate, 3))").monospacedDigit() }
        }
        .font(.caption).foregroundStyle(.secondary)
        if t.lev == nil && t.has_key && !close {
            OrderNote(text: L("Leverage unknown: max size and cost assume 1x."), color: .orange, icon: "exclamationmark.triangle.fill")
        }
        if qtyV > 0, let m = t.min_qty, qtyV < m {
            OrderNote(text: "\(L("Below the minimum size")) \(Fmt.qty(m)) \(s.base)", color: .orange, icon: "exclamationmark.triangle.fill")
        } else if notional > 0, let m = t.min_notional, notional < m {
            OrderNote(text: "\(L("Below the minimum order value")) \(Fmt.usd(m)) USDT", color: .orange, icon: "exclamationmark.triangle.fill")
        }
    }

    private func pendingRow(_ p: OrderPendingTpSl.Item) -> some View {
        LabeledContent {
            Button(role: .destructive) { pending.items.removeAll { $0.id == p.id } } label: { Image(systemName: "xmark.circle.fill") }
                .buttonStyle(.borderless).help(L("Forget this TP/SL"))
        } label: {
            HStack(spacing: 6) {
                VenueIcon(ex: p.ex, size: 12)
                Text("TP \(Fmt.px(p.tp)) · SL \(Fmt.px(p.sl))").monospacedDigit()
            }
        }
    }

    // MARK: actions

    private func ms(_ v: Double?) -> String { v.map { String(format: "%.0fms", $0) } ?? "–" }
    private func openSettings(_ tab: String) { NotificationCenter.default.post(name: .init("T1OpenSettings"), object: tab) }

    private func enter() {
        if let p = lastPos { submit(p) } else { inlineError = L("Press Buy or Sell once; Enter then repeats that side.") }
    }

    /// Preview first (validates and routes), then the confirm sheet, or place directly when
    /// confirmations are off.
    private func submit(_ pos: String) {
        inlineError = nil
        lastPos = pos
        guard qtyV > 0 else { inlineError = L("Enter a size"); focus = .qty; return }
        var args: [String: Any] = ["pos": pos, "close": close, "kind": kindArg, "qty": qtyV, "tif": tif]
        switch kind {
        case .scaled:
            guard let a = OrderNum.parse(scaledFrom), let b = OrderNum.parse(scaledTo), a > 0, b > 0 else { inlineError = L("Enter both prices"); return }
            args["from"] = a; args["to"] = b; args["count"] = scaledCount; args["skew"] = scaledSkew; args["post_only"] = scaledPost
            algoConfirm = AlgoTicket(op: "place_scaled", args: args, title: L("Place scaled orders?"),
                message: String(format: L("%d %@ limit orders from %@ to %@, %@ %@ in total on %@."), scaledCount, pos == "long" ? (close ? L("buy") : L("long")) : (close ? L("sell") : L("short")),
                                Fmt.px(a), Fmt.px(b), Fmt.qty(qtyV), s.base, t.venue))
            return
        case .twap:
            guard let m = OrderNum.parse(twapMinutes), m > 0, let n = Int(twapSlices), n > 0 else { inlineError = L("Enter a duration and a number of slices"); return }
            args["minutes"] = m; args["slices"] = n; args["max_slip_bps"] = OrderNum.parse(twapSlip) ?? 10
            if let l = OrderNum.parse(twapLimit), l > 0 { args["limit"] = l }
            algoConfirm = AlgoTicket(op: "start_twap", args: args, title: L("Start TWAP?"),
                message: String(format: L("%@ %@ %@ on %@ in %d slices over %@ min, at most %@ bp slippage per slice%@."), pos == "long" ? (close ? L("Buy") : L("Long")) : (close ? L("Sell") : L("Short")),
                                Fmt.qty(qtyV), s.base, t.venue, n, Fmt.num(m, 0), twapSlip, (args["limit"] as? Double).map { ", " + L("limit") + " " + Fmt.px($0) } ?? ""))
            return
        case .stop:
            guard let tr = OrderNum.parse(trigger), tr > 0 else { inlineError = L("Enter a trigger price"); return }
            args["trigger"] = tr; args["trigger_by"] = triggerLast ? "last" : "mark"
        case .trailing:
            guard let cb = OrderNum.parse(callback), cb >= 0.1, cb <= 10 else { inlineError = L("Callback must be 0.1% to 10%"); return }
            args["callback_pct"] = cb
            if let a = OrderNum.parse(activation), a > 0 { args["activation"] = a }
        default: break
        }
        if typedPrice {
            guard let p = OrderNum.parse(price), p > 0 else { inlineError = L("Enter a price"); focus = .price; return }
            args["price"] = p
        }
        if useBbo { args["bbo_queue"] = bboQueue; args["bbo_level"] = bboLevel }
        var tpV: Double?, slV: Double?
        if tpslOn && !close {
            tpV = OrderNum.parse(tp); slV = OrderNum.parse(sl)
            if tpV == nil && slV == nil { inlineError = L("Enter a take-profit or stop-loss price, or turn TP/SL off"); return }
            if let e = orderTpSlCheck(long: pos == "long", ref: refPx, refName: L("the order price"), tp: tpV, sl: slV) { inlineError = e; return }
        }
        guard let plan = store.call("preview", args, as: RoutePlan.self) else { inlineError = store.lastError ?? L("Preview failed"); return }
        if plan.legs.isEmpty {
            inlineError = plan.excluded.isEmpty ? L("No venue can take this order") : plan.excluded.map { "\($0.ex): \($0.reason)" }.joined(separator: "\n")
            return
        }
        let tk = OrderTicket(pos: pos, close: close, symbol: t.symbol, base: s.base, args: args, plan: plan, tp: tpV, sl: slV, trigger: trigLast ? "last" : "mark")
        if s.prefs.confirm { ticket = tk } else if let e = orderPlace(tk) { inlineError = e }
    }

    private func marketClose(_ p: PositionRow) {
        inlineError = nil
        let args: [String: Any] = ["pos": p.side, "close": true, "kind": "market", "qty": p.qty]
        guard let plan = store.call("preview", args, as: RoutePlan.self), !plan.legs.isEmpty else { inlineError = store.lastError ?? L("No venue can take this order"); return }
        let tk = OrderTicket(pos: p.side, close: true, symbol: p.symbol, base: s.base, args: args, plan: plan, tp: nil, sl: nil, trigger: "mark")
        ticket = tk  // destructive: always confirmed, whatever the preference
    }

    /// Prefill the form: close tab, limit at the venue's own touch on the passive side.
    private func limitClose(_ p: PositionRow) {
        close = true; kind = .limit; bboOn = false
        DispatchQueue.main.async {
            if let px = p.isLong ? t.ask : t.bid { price = OrderNum.px(px, tick: t.tick) }
            qty = OrderNum.qty(p.qty, step: t.step)
            pct = maxQty > 0 ? min(p.qty / maxQty, 1) : 0
            lastPos = p.side
            focus = .price
        }
    }
}

/// Order types of the panel. limit / market / stop / trailing are venue orders; scaled and twap
/// are client-side algorithms built from limit orders (work on every venue).
enum OrderKind: String { case limit, market, stop, trailing, scaled, twap }
struct BboChoice: Hashable { var queue: Bool; var level: Int }
enum OrderFieldID: Hashable { case price, qty, tp, sl, level(Int), wholeTp, wholeSl }

/// Pre-validation of TP/SL against a reference price (mark for a position, the order price for
/// a new order): TP above / SL below for longs, the reverse for shorts.
@MainActor func orderTpSlCheck(long: Bool, ref: Double, refName: String, tp: Double?, sl: Double?) -> String? {
    if let tp, tp <= 0 || (long ? tp <= ref : tp >= ref) { return "\(long ? L("Take-profit must be above") : L("Take-profit must be below")) \(refName) (\(Fmt.px(ref)))" }
    if let sl, sl <= 0 || (long ? sl >= ref : sl <= ref) { return "\(long ? L("Stop-loss must be below") : L("Stop-loss must be above")) \(refName) (\(Fmt.px(ref)))" }
    return nil
}

// MARK: - number input helpers

enum OrderNum {
    static func parse(_ s: String) -> Double? {
        Double(s.replacingOccurrences(of: ",", with: "").trimmingCharacters(in: .whitespaces)).flatMap { $0.isFinite ? $0 : nil }
    }
    static func decimals(_ step: Double?) -> Int? {
        guard let s = step, s > 0 else { return nil }
        return max(0, Int((-log10(s) - 1e-9).rounded(.up)))
    }
    /// price for an input box: tick decimals when the venue rules are known, else Fmt.px's
    static func px(_ v: Double, tick: Double?) -> String {
        let a = abs(v)
        let d = decimals(tick) ?? (a >= 10_000 ? 1 : a >= 100 ? 2 : a >= 1 ? 4 : 6)
        return String(format: "%.\(d)f", v)
    }
    /// size floored to the venue's step (never rounds up past the max); 6 decimals when unknown
    static func qty(_ v: Double, step: Double?) -> String {
        if let s = step, s > 0, let d = decimals(s) { return String(format: "%.\(d)f", ((v / s) + 1e-9).rounded(.down) * s) }
        var out = String(format: "%.6f", (v * 1e6).rounded(.down) / 1e6)
        while out.contains("."), out.hasSuffix("0") || out.hasSuffix(".") { out.removeLast() }
        return out
    }
}

// MARK: - TP/SL waiting for the order to fill

/// TP/SL attached to an opening order: applied with `set_tpsl` (whole position) once the
/// position on that venue grows past its size at placement.
/// ponytail: checked from the order panel's position updates, so it only fires while the panel is
/// on screen (Perp mode); move into Store/Rust if orders can fill while another mode is shown.
@MainActor @Observable final class OrderPendingTpSl {
    static let shared = OrderPendingTpSl()
    struct Item: Identifiable {
        let id = UUID(); var ex: String; var symbol: String; var pos: String; var baseQty: Double
        var tp: Double?; var sl: Double?; var trigger: String; var until: Date
    }
    var items: [Item] = []

    func add(ex: String, symbol: String, pos: String, positions: [PositionRow], tp: Double?, sl: Double?, trigger: String) {
        let q = positions.first { $0.ex == ex && $0.symbol == symbol && $0.side == pos }?.qty ?? 0
        items.append(Item(ex: ex, symbol: symbol, pos: pos, baseQty: q, tp: tp, sl: sl, trigger: trigger, until: Date().addingTimeInterval(120)))
    }

    func check(_ positions: [PositionRow], orders: [OrderRow]) {
        items.removeAll { it in
            let q = positions.first { $0.ex == it.ex && $0.symbol == it.symbol && $0.side == it.pos }?.qty ?? 0
            if q > it.baseQty * (1 + 1e-9) + 1e-12 {
                var a: [String: Any] = ["ex": it.ex, "symbol": it.symbol, "pos": it.pos, "trigger": it.trigger]
                if let tp = it.tp { a["tp"] = tp }
                if let sl = it.sl { a["sl"] = sl }
                Store.shared.call("set_tpsl", a)
                return true
            }
            // a resting limit keeps it alive; otherwise give up after the window
            return Date() > it.until && !orders.contains { $0.ex == it.ex && $0.symbol == it.symbol }
        }
    }
}

// MARK: - shared bits (used by the Order* views)

/// Up/down follow the system green/red, swapped by the red-up preference.
@MainActor enum OrderColor {
    static var up: Color { Store.shared.state.prefs.red_up ? .red : .green }
    static var down: Color { Store.shared.state.prefs.red_up ? .green : .red }
}

/// Capsule input: label inside on the left, value right-aligned, unit and an optional accessory.
struct OrderField<Accessory: View>: View {
    let label: String
    @Binding var text: String
    let unit: String
    let id: OrderFieldID
    var focus: FocusState<OrderFieldID?>.Binding
    var placeholder = ""
    var disabled = false
    var onSubmit: () -> Void = {}
    @ViewBuilder var accessory: () -> Accessory

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            Text(label).font(.callout).foregroundStyle(.secondary)
            OrderFieldShell(active: focus.wrappedValue == id) {
                TextField(label, text: $text, prompt: Text(placeholder))
                    .labelsHidden()
                    .textFieldStyle(.plain)
                    .font(.body.monospacedDigit())
                    .focused(focus, equals: id)
                    .onSubmit(onSubmit)
                    .disabled(disabled)
                if !unit.isEmpty { Text(unit).foregroundStyle(.secondary) }
                accessory()
            }
        }
    }
}

extension OrderField where Accessory == EmptyView {
    init(label: String, text: Binding<String>, unit: String, id: OrderFieldID, focus: FocusState<OrderFieldID?>.Binding,
         placeholder: String = "", disabled: Bool = false, onSubmit: @escaping () -> Void = {}) {
        self.init(label: label, text: text, unit: unit, id: id, focus: focus, placeholder: placeholder, disabled: disabled, onSubmit: onSubmit) { EmptyView() }
    }
}

/// The capsule every order input sits in.
struct OrderFieldShell<Content: View>: View {
    var active = false
    @ViewBuilder var content: () -> Content
    var body: some View {
        HStack(spacing: 8) { content() }
            .padding(.horizontal, 12).frame(height: 38)
            .background(.fill.tertiary, in: .rect(cornerRadius: 10))
            .overlay { RoundedRectangle(cornerRadius: 10).strokeBorder(active ? Color.accentColor : Color.primary.opacity(0.08), lineWidth: active ? 1.5 : 1) }
    }
}

/// Full-width segmented choice with a Liquid Glass thumb.
struct GlassSegments<V: Hashable>: View {
    @Binding var selection: V
    let items: [(V, String)]
    @Namespace private var ns
    var body: some View {
        GlassEffectContainer {
            HStack(spacing: 0) {
                ForEach(items, id: \.0) { v, name in
                    Button { withAnimation(.smooth(duration: 0.25)) { selection = v } } label: {
                        Text(name).fontWeight(.semibold).frame(maxWidth: .infinity).frame(height: 30)
                            .foregroundStyle(selection == v ? .primary : .secondary)
                            .contentShape(.capsule)
                            .glassEffect(selection == v ? .regular.interactive() : .identity, in: .capsule)
                            .glassEffectID(name, in: ns)
                    }
                    .buttonStyle(.plain)
                }
            }
            .padding(3)
            .background(.fill.tertiary, in: .capsule)
        }
    }
}

struct OrderBigButton: View {
    let title: String
    let color: Color
    var enabled = true
    let action: () -> Void
    var body: some View {
        Button(action: action) {
            Text(title).font(.headline).foregroundStyle(.white).frame(maxWidth: .infinity).frame(height: 40).contentShape(.rect(cornerRadius: 10))
        }
        .buttonStyle(.plain)
        // explicit fill: system prominent / tinted glass buttons go grey whenever the window is inactive
        .background(color.opacity(enabled ? 0.9 : 0.35).gradient, in: .rect(cornerRadius: 10))
        .disabled(!enabled)
    }
}

/// Compact "label  value" line (caption, digits aligned).
struct OrderKV: View {
    let k: String
    let v: String
    var color: Color? = nil
    var body: some View {
        LabeledContent {
            Text(v).monospacedDigit().foregroundStyle(color ?? .secondary).lineLimit(1).minimumScaleFactor(0.8)
        } label: {
            Text(k).foregroundStyle(.secondary).lineLimit(1)
        }
        .font(.callout)
    }
}

struct OrderNote: View {
    let text: String
    let color: Color
    var icon = "info.circle"
    var body: some View {
        Label { Text(text).fixedSize(horizontal: false, vertical: true) } icon: { Image(systemName: icon) }
            .font(.caption)
            .foregroundStyle(color)
            .frame(maxWidth: .infinity, alignment: .leading)
    }
}

/// A scaled or TWAP request waiting for confirmation.
struct AlgoTicket: Identifiable { let id = UUID(); let op: String; let args: [String: Any]; let title: String; let message: String }

/// Finished jobs stay listed for a minute.
func terminal_recent(_ j: AlgoJobRow) -> Bool { Double(Date.now.timeIntervalSince1970 * 1000) - Double(j.end_ms) < 60_000 }

/// Running TWAP jobs: progress, status and cancel.
struct AlgoJobsView: View {
    let jobs: [AlgoJobRow]
    var body: some View {
        PanelSection(L("Running algorithms")) {
            ForEach(jobs) { j in
                VStack(alignment: .leading, spacing: 4) {
                    HStack {
                        Text("TWAP \(j.buy ? L("buy") : L("sell")) \(Fmt.qty(j.total))").font(.callout.weight(.semibold))
                        Text("\(j.ex) · \(j.symbol)").font(.caption).foregroundStyle(.secondary)
                        Spacer()
                        if !j.cancelled && j.status != "done" {
                            Button(L("Cancel")) { Store.shared.call("cancel_algo", ["id": j.id]) }.buttonStyle(.borderless).foregroundStyle(.red)
                        }
                    }
                    ProgressView(value: Double(j.done), total: Double(max(j.slices, 1)))
                    Text("\(j.done)/\(j.slices) · \(Fmt.qty(j.sent)) \(L("sent")) · \(L(j.status))").font(.caption).foregroundStyle(j.status.hasPrefix("waiting") ? Color.orange : Color.secondary)
                }
            }
        }
    }
}
