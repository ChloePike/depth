import SwiftUI

/// Native controls above the egui chart and order book: interval, source, indicators, drawing
/// tools on the left; book/trades, unit, source and tick on the right, lined up with the book column.
struct ChartBar: View {
    private var store: Store { Store.shared }

    var body: some View {
        let c = store.state.chart
        let option = store.state.mode == "Option"
        HStack(spacing: 10) {
            if !option { chartControls(c) }
            Spacer(minLength: 0)
        }
        .controlSize(.small)
        .padding(.horizontal, 10)
        .frame(height: 34)
    }

    // MARK: chart

    @ViewBuilder private func chartControls(_ c: ChartCtl) -> some View {
        let quick = Array(c.intervals.prefix(c.quick))
        let more = Array(c.intervals.dropFirst(c.quick))
        Picker(L("Interval"), selection: Binding(get: { quick.contains { $0.0 == c.tf } ? c.tf : -1 }, set: { if $0 > 0 { chart(["tf": $0]) } })) {
            ForEach(quick, id: \.0) { Text($0.1).tag($0.0) }
            if !quick.contains(where: { $0.0 == c.tf }) { Text(more.first { $0.0 == c.tf }?.1 ?? "").tag(-1) }
        }
        .pickerStyle(.segmented).labelsHidden().fixedSize()
        Menu {
            ForEach(more, id: \.0) { m in Button { chart(["tf": m.0]) } label: { if m.0 == c.tf { Label(m.1, systemImage: "checkmark") } else { Text(m.1) } } }
        } label: { Image(systemName: "ellipsis") }
        .menuIndicator(.hidden).fixedSize().help(L("More intervals"))

        Menu {
            Picker(L("Source"), selection: Binding(get: { c.src ?? "" }, set: { chart(["src": $0.isEmpty ? NSNull() : $0]) })) {
                Text(L("Aggregate")).tag("")
                Divider()
                ForEach(c.venues, id: \.self) { Text($0).tag($0) }
            }
            .pickerStyle(.inline)
        } label: {
            Label(c.src ?? L("Aggregate"), systemImage: "square.stack.3d.up")
        }
        .fixedSize().help(L("Chart source: all venues or one venue"))

        Menu {
            Section(L("Panes")) {
                ForEach(c.panes) { p in Toggle(p.label, isOn: Binding(get: { p.on }, set: { chart(["pane": p.key, "on": $0]) })) }
            }
            Section(L("Overlays")) {
                Toggle("MA (7 / 25 / 99)", isOn: Binding(get: { c.ma }, set: { chart(["ma": $0]) }))
                Toggle(L("Book heatmap"), isOn: Binding(get: { c.heat }, set: { chart(["heat": $0]) }))
                if c.perp { Toggle(L("Liquidation map"), isOn: Binding(get: { c.liqmap }, set: { chart(["liqmap": $0]) })) }
            }
            Section(L("Levels")) {
                Toggle(L("Volume profile (POC · value area · HVN)"), isOn: Binding(get: { c.sr }, set: { chart(["sr": $0]) }))
                Toggle(L("Breakouts (closes through VAH / VAL / HVN)"), isOn: Binding(get: { c.breakouts }, set: { chart(["breakouts": $0]) }))
                Toggle(L("Session VWAP ±1σ ±2σ"), isOn: Binding(get: { c.vwap }, set: { chart(["vwap": $0]) }))
                Toggle(L("Expected move cone (volatility model)"), isOn: Binding(get: { c.cone }, set: { chart(["cone": $0]) }))
                Toggle(L("Liquidity walls"), isOn: Binding(get: { c.walls }, set: { chart(["walls": $0]) }))
                if c.perp { Toggle(L("Mark price"), isOn: Binding(get: { c.mark }, set: { chart(["mark": $0]) })) }
            }
        } label: {
            Label(L("Indicators"), systemImage: "waveform.path.ecg")
        }
        .fixedSize()

        ControlGroup {
            tool("hline", "minus", L("Horizontal line"), c)
            tool("trend", "line.diagonal", L("Trend line"), c)
            tool("ray", "arrow.up.right", L("Ray"), c)
            if c.drawings > 0 {
                Button { chart(["clear_drawings": true]) } label: { Image(systemName: "trash") }.help("\(L("Clear drawings")) (\(c.drawings))")
            }
        }
        .fixedSize()
        if c.panned {
            Button { chart(["reset_view": true]) } label: { Label(L("Latest"), systemImage: "arrow.right.to.line") }
                .help(L("Back to the live edge and default zoom"))
        }
    }

    private func tool(_ id: String, _ icon: String, _ help: String, _ c: ChartCtl) -> some View {
        Toggle(isOn: Binding(get: { c.tool == id }, set: { chart(["tool": $0 ? id : NSNull()]) })) { Image(systemName: icon) }
            .toggleStyle(.button).help(help)
    }

    // MARK: book

    @ViewBuilder func bookControls(_ b: BookCtl) -> some View {
        HStack(spacing: 6) {
            Picker(L("Book"), selection: Binding(get: { b.trades }, set: { book(["trades": $0]) })) {
                Text(L("Book")).tag(false)
                Text(L("Trades")).tag(true)
            }
            .pickerStyle(.segmented).labelsHidden().fixedSize()
            Spacer(minLength: 4)
            bookRight(b)
        }
    }

    /// Source, tick and unit menus of the book header.
    @ViewBuilder func bookRight(_ b: BookCtl) -> some View {
        HStack(spacing: 6) {
            if !b.trades {
                Menu {
                    Picker(L("Source"), selection: Binding(get: { b.src ?? "" }, set: { book(["src": $0.isEmpty ? NSNull() : $0]) })) {
                        Text(L("Aggregate USD")).tag("")
                        Divider()
                        ForEach(b.venues, id: \.self) { Text($0).tag($0) }
                    }
                    .pickerStyle(.inline)
                } label: {
                    if let v = b.src { VenueIcon(ex: v, size: 13); Text(v) } else { Text(L("All")) }
                }
                .menuStyle(.borderlessButton).fixedSize()
                .help(b.src == nil ? L("All venues, mid-aligned (view only)") : L("This venue's raw book (click a level to fill the price)"))
                if !b.groups.isEmpty {
                    Menu {
                        Picker(L("Tick"), selection: Binding(get: { b.group }, set: { book(["group": $0]) })) {
                            ForEach(Array(b.groups.enumerated()), id: \.offset) { Text($0.element).tag($0.offset) }
                        }
                        .pickerStyle(.inline)
                    } label: { Text(b.groups[min(b.group, b.groups.count - 1)]).monospacedDigit() }
                    .menuStyle(.borderlessButton).fixedSize().help(L("Price grouping"))
                }
            }
            Menu {
                Picker(L("Unit"), selection: Binding(get: { b.quote }, set: { book(["quote": $0]) })) {
                    Text(store.state.base).tag(false)
                    Text("USDT").tag(true)
                }
                .pickerStyle(.inline)
            } label: { Text(b.quote ? "USDT" : store.state.base) }
            .menuStyle(.borderlessButton).fixedSize().help(L("Size unit"))
        }
    }

    private func chart(_ a: [String: Any]) { store.call("chart", a) }
    func book(_ a: [String: Any]) { store.call("book", a) }
}
