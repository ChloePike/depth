import SwiftUI

/// Regular trading sessions of the big stock markets, in each exchange's own time zone, so
/// daylight saving is handled by the calendar.
/// ponytail: weekdays only, public holidays and half days are not known; add a holiday table if it matters.
struct StockMarket: Identifiable {
    let name: String; let short: String; let tz: String
    /// (start, end) in minutes after local midnight; two entries where there is a lunch break
    let sessions: [(Int, Int)]
    var id: String { name }

    static let all = [
        StockMarket(name: "New York", short: "NYSE", tz: "America/New_York", sessions: [(570, 960)]),
        StockMarket(name: "London", short: "LSE", tz: "Europe/London", sessions: [(480, 990)]),
        StockMarket(name: "Frankfurt", short: "Xetra", tz: "Europe/Berlin", sessions: [(540, 1050)]),
        StockMarket(name: "Tokyo", short: "TSE", tz: "Asia/Tokyo", sessions: [(540, 690), (750, 930)]),
        StockMarket(name: "Hong Kong", short: "HKEX", tz: "Asia/Hong_Kong", sessions: [(570, 720), (780, 960)]),
        StockMarket(name: "Shanghai", short: "SSE", tz: "Asia/Shanghai", sessions: [(570, 690), (780, 900)]),
    ]

    /// Open now (and when that session ends), or closed and when the next session starts.
    func status(at now: Date) -> (open: Bool, until: Date) {
        var cal = Calendar(identifier: .gregorian)
        cal.timeZone = TimeZone(identifier: tz) ?? .gmt
        let today = cal.startOfDay(for: now)
        for d in 0..<8 {
            guard let day = cal.date(byAdding: .day, value: d, to: today) else { continue }
            let wd = cal.component(.weekday, from: day)
            if wd == 1 || wd == 7 { continue }
            for (a, b) in sessions {
                // via date components, not seconds: a DST change day is 23 or 25 hours long
                guard let s = cal.date(bySettingHour: a / 60, minute: a % 60, second: 0, of: day),
                      let e = cal.date(bySettingHour: b / 60, minute: b % 60, second: 0, of: day) else { continue }
                if now < s { return (false, s) }
                if now < e { return (true, e) }
            }
        }
        return (false, now)
    }
}

/// Toolbar item: the next market to open with a countdown; click for every market.
struct MarketClock: View {
    @State private var shown = false

    var body: some View {
        TimelineView(.periodic(from: .now, by: 1)) { ctx in
            let next = StockMarket.all.map { ($0, $0.status(at: ctx.date)) }.filter { !$0.1.open }.min { $0.1.until < $1.1.until }
            Button { shown.toggle() } label: {
                if let (m, s) = next {
                    Text("\(m.short) \(L("opens")) \(Self.left(s.until, ctx.date))").monospacedDigit()
                } else {
                    Text(L("Markets"))
                }
            }
            .help(L("Stock market sessions"))
            .popover(isPresented: $shown, arrowEdge: .bottom) { list }
        }
    }

    private var list: some View {
        TimelineView(.periodic(from: .now, by: 1)) { ctx in
            Grid(alignment: .leading, horizontalSpacing: 14, verticalSpacing: 8) {
                ForEach(StockMarket.all) { m in
                    let s = m.status(at: ctx.date)
                    GridRow {
                        Circle().fill(s.open ? T1.up : Color.secondary.opacity(0.4)).frame(width: 7, height: 7)
                        VStack(alignment: .leading, spacing: 1) {
                            Text(L(m.name))
                            Text(m.short).font(.caption).foregroundStyle(.secondary)
                        }
                        Text(s.open ? L("Closes in") : L("Opens in")).foregroundStyle(.secondary)
                        Text(Self.left(s.until, ctx.date)).monospacedDigit().gridColumnAlignment(.trailing)
                    }
                }
            }
            .padding(14)
            .frame(minWidth: 280)
        }
    }

    /// 1d 02:03:04 / 02:03:04
    static func left(_ t: Date, _ now: Date) -> String {
        let s = max(0, Int(t.timeIntervalSince(now)))
        let hms = String(format: "%02d:%02d:%02d", s / 3600 % 24, s % 3600 / 60, s % 60)
        return s >= 86400 ? "\(s / 86400)d \(hms)" : hms
    }
}
