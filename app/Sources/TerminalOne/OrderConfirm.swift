import SwiftUI

/// A previewed order: the exact `preview` args (re-sent unchanged to `place`) and the plan.
struct OrderTicket: Identifiable {
    let id = UUID()
    var pos: String
    var close: Bool
    var symbol: String
    var base: String
    var args: [String: Any]
    var plan: RoutePlan
    var tp: Double?
    var sl: Double?
    var trigger: String

    /// buy = open long or close short
    var buy: Bool { (pos == "long") != close }
    @MainActor var title: String {
        switch (pos, close) {
        case ("long", false): L("Open Long")
        case ("short", false): L("Open Short")
        case ("long", true): L("Close Long")
        default: L("Close Short")
        }
    }
}

/// Send a previewed ticket; registers its TP/SL to be applied once each leg's position exists.
/// Returns the error text, nil on success.
@MainActor func orderPlace(_ t: OrderTicket) -> String? {
    let store = Store.shared
    let before = store.state.positions
    var args = t.args
    args["expect"] = t.plan.legs.map { ["ex": $0.ex, "qty": $0.qty] as [String: Any] }
    guard let plan = store.call("place", args, as: RoutePlan.self) else { return store.lastError ?? L("Order failed") }
    if t.tp != nil || t.sl != nil {
        for ex in Set(plan.legs.map(\.ex)) {
            OrderPendingTpSl.shared.add(ex: ex, symbol: t.symbol, pos: t.pos, positions: before, tp: t.tp, sl: t.sl, trigger: t.trigger)
        }
    }
    return nil
}

/// Plan leg kind ("market", "limit 123.4", "post 1", "ioc 2", "bbo queue 5") as UI text.
@MainActor func orderKindText(_ k: String) -> String {
    let p = k.split(separator: " ").map(String.init)
    let px = p.count > 1 ? Fmt.px(Double(p[1])) : ""
    switch p.first ?? "" {
    case "market": return L("Market")
    case "limit": return "\(L("Limit")) \(px)"
    case "post": return "\(L("Post Only")) \(px)"
    case "ioc": return "\(L("IOC limit")) \(px)"
    case "bbo": return "BBO \(p.count > 1 && p[1] == "queue" ? L("Queue") : L("Counterparty")) \(p.count > 2 ? p[2] : "")"
    default: return k
    }
}

struct OrderConfirm: View {
    let ticket: OrderTicket
    @Environment(\.dismiss) private var dismiss
    @State private var error: String?

    private var plan: RoutePlan { ticket.plan }
    private var color: Color { ticket.buy ? OrderColor.up : OrderColor.down }

    var body: some View {
        VStack(spacing: 0) {
            Form {
                Section {
                    LabeledContent {
                        Text(ticket.title).font(.body.weight(.semibold)).foregroundStyle(color)
                    } label: {
                        HStack(spacing: 8) {
                            CoinIcon(base: ticket.base, size: 20)
                            Text(ticket.symbol).font(.headline.weight(.semibold))
                            Text(L("Perp")).foregroundStyle(.secondary)
                        }
                    }
                }
                Section(plan.legs.count > 1 ? "\(L("Route")) · \(plan.legs.count) \(L("legs"))" : L("Route")) {
                    ForEach(plan.legs) { leg($0) }
                }
                if !plan.excluded.isEmpty {
                    Section(L("Not used")) {
                        ForEach(plan.excluded) { x in
                            LabeledContent {
                                Text(x.reason).foregroundStyle(.secondary).multilineTextAlignment(.trailing)
                            } label: {
                                HStack(spacing: 6) { VenueIcon(ex: x.ex, size: 12); Text(x.ex) }
                            }
                        }
                    }
                }
                Section(L("Total")) {
                    if let c = plan.clamped_from {
                        OrderNote(text: "\(L("Size reduced from")) \(Fmt.qty(c)) \(L("to")) \(Fmt.qty(plan.total_qty)) \(ticket.base) (\(ticket.close ? L("position size") : L("margin limit")))",
                                  color: Color.orange, icon: "exclamationmark.triangle.fill")
                    }
                    row(L("Size"), "\(Fmt.qty(plan.total_qty)) \(ticket.base)")
                    row(L("Est. average price"), Fmt.px(plan.vwap))
                    row(L("Order value"), "\(Fmt.usd(plan.total_qty * plan.vwap)) USDT")
                    row(L("Est. fees"), "\(Fmt.usd(plan.fees, 3)) USDT")
                }
                Section {
                    if ticket.tp != nil || ticket.sl != nil {
                        OrderNote(text: "\(L("TP")) \(Fmt.px(ticket.tp)) · \(L("SL")) \(Fmt.px(ticket.sl)) (\(ticket.trigger == "last" ? L("Last") : L("Mark"))): \(L("set for the whole position once the order has filled and the position exists."))",
                                  color: Color.accentColor, icon: "target")
                    }
                    if plan.legs.contains(where: { l in Store.shared.state.keys.first { $0.ex == l.ex }?.verified == false }) {
                        OrderNote(text: L("Order placement on this venue is not yet verified on a live account: start with the minimum size."), color: Color.orange, icon: "exclamationmark.triangle.fill")
                    }
                    OrderNote(text: L("Prices and sizes are rounded to the venue's tick and step."), color: .secondary)
                    if let e = error { OrderNote(text: e, color: OrderColor.down, icon: "xmark.octagon.fill") }
                }
            }
            .formStyle(.grouped)
            HStack(spacing: 8) {
                Button { dismiss() } label: { Text(L("Cancel")).frame(maxWidth: .infinity) }
                    .controlSize(.large).keyboardShortcut(.cancelAction)
                OrderBigButton(title: "\(L("Confirm")) \(ticket.title)", color: color) {
                    if let e = orderPlace(ticket) { error = e } else { dismiss() }
                }
                .keyboardShortcut(.defaultAction)
            }
            .padding([.horizontal, .bottom], 20)
        }
        .frame(width: 460, height: 560)
    }

    private func row(_ k: String, _ v: String) -> some View {
        LabeledContent(k) { Text(v).monospacedDigit() }
    }

    private func leg(_ l: RoutePlan.Leg) -> some View {
        let buy = (l.pos == "long") != l.close
        return VStack(alignment: .leading, spacing: 4) {
            LabeledContent {
                Text("\(Fmt.qty(l.qty)) \(ticket.base)").font(.callout.monospacedDigit().weight(.medium))
            } label: {
                HStack(spacing: 6) {
                    VenueIcon(ex: l.ex, size: 14)
                    Text(l.ex).fontWeight(.semibold)
                    Text(buy ? L("Buy") : L("Sell")).fontWeight(.semibold).foregroundStyle(buy ? OrderColor.up : OrderColor.down)
                }
            }
            Grid(alignment: .leading, horizontalSpacing: 14, verticalSpacing: 2) {
                GridRow {
                    OrderKV(k: L("Order"), v: orderKindText(l.kind), color: .primary)
                    OrderKV(k: L("Est. price"), v: Fmt.px(l.est_px), color: .primary)
                }
                GridRow {
                    OrderKV(k: L("Est. fee"), v: Fmt.usd(l.est_fee, 3))
                    OrderKV(k: L("Slippage"), v: String(format: "%.1f bp", l.slip_bps), color: l.slip_bps > 5 ? Color.orange : nil)
                }
                GridRow {
                    OrderKV(k: L("Available after"), v: "\(Fmt.usd(l.avail_after)) USDT", color: l.avail_after < 0 ? OrderColor.down : nil).gridCellColumns(2)
                }
            }
        }
    }
}
