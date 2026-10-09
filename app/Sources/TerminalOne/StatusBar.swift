import SwiftUI

/// Bottom strip: per-venue connection toggles (off = disconnected, engine restarts), engine stats,
/// UTC clock, language.
struct StatusBar: View {
    @Environment(Store.self) private var store

    var body: some View {
        let s = store.state
        let st = s.stats
        HStack(spacing: 4) {
            // scrolls instead of imposing its width on the window when space is short
            ScrollView(.horizontal) {
                HStack(spacing: 4) { ForEach(s.venues) { v in venue(v) } }
            }
            .scrollIndicators(.never)
            Divider().frame(height: 12).padding(.horizontal, 6)
            HStack(spacing: 14) {
                Text("\(Fmt.num(st.msgs_per_s ?? 0, 0)) \(L("msg/s"))")
                if let mb = st.mem_mb {
                    Text("\(L("Mem")) \(Fmt.num(mb, 0)) MB").foregroundStyle(mb > 1500 ? AnyShapeStyle(.red) : mb > 800 ? AnyShapeStyle(.orange) : AnyShapeStyle(.secondary))
                }
                let bl = st.backlog ?? 0
                Text("\(L("Backlog")) \(bl)").foregroundStyle(bl > 10_000 ? AnyShapeStyle(.red) : bl > 1_000 ? AnyShapeStyle(.orange) : AnyShapeStyle(.secondary))
                let h = st.history ?? [0, 0]
                let done = h.count == 2 && h[0] >= h[1]
                Text(done ? L("History loaded") : "\(L("History")) \(h.first ?? 0)/\(h.last ?? 0)")
                    .foregroundStyle(done ? AnyShapeStyle(.secondary) : AnyShapeStyle(.orange))
                if let e = st.errors, e > 0 {
                    Text("\(e) \(L("errors"))").foregroundStyle(.red)
                }
            }
            .font(.caption.monospacedDigit())
            .foregroundStyle(.secondary)
            .lineLimit(1)
            .fixedSize()
            Spacer(minLength: 8)
            TimelineView(.periodic(from: .now, by: 1)) { ctx in
                Text(T1.tzLabel(T1.tz) + " " + { Self.clock.timeZone = T1.tz; return Self.clock.string(from: ctx.date) }()).font(.caption.monospacedDigit()).foregroundStyle(.secondary)
            }
            Divider().frame(height: 12).padding(.horizontal, 6)
            Picker(L("Language"), selection: Binding(get: { s.lang }, set: { store.call("set_lang", ["lang": $0]) })) {
                ForEach(s.langs, id: \.self) { l in Text(l[1]).tag(l[0]) }
            }
            .pickerStyle(.menu)
            .buttonStyle(.borderless)
            .labelsHidden()
            .controlSize(.small)
            .fixedSize()
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 5)
    }

    private func venue(_ v: VenueStatus) -> some View {
        let col: AnyShapeStyle = {
            guard let l = v.lat_ms, v.on else { return AnyShapeStyle(.tertiary) }
            if !v.alive { return AnyShapeStyle(.red) }
            return AnyShapeStyle(l < 150 ? Color.green : l < 400 ? .orange : .red)
        }()
        return Button { store.call("toggle_venue", ["ex": v.ex]) } label: {
            HStack(spacing: 5) {
                VenueIcon(ex: v.ex, size: 12).saturation(v.on ? 1 : 0).opacity(v.on ? 1 : 0.5)
                Text(v.ex).foregroundStyle(v.on ? .primary : .tertiary)
                Text(v.on ? (v.lat_ms.map { "\(Int($0))" } ?? "–") : L("off")).monospacedDigit().foregroundStyle(col)
            }
            .font(.caption)
            .padding(.trailing, 8)
        }
        .buttonStyle(.plain)
        .help("\(v.ex): " + (v.on ? L("Connected; click to disconnect") : L("Disconnected; click to connect")) + (v.lat_ms.map { " · \(Int($0)) ms" } ?? ""))
    }

    private static let clock: DateFormatter = {
        let f = DateFormatter()
        f.dateFormat = "HH:mm:ss"
        return f
    }()
}
