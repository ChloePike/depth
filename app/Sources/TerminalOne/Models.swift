import Foundation

// Mirrors of the JSON produced by ui/src/native.rs (`App::state_json`). Every field optional or
// defaulted so a Rust-side addition never breaks decoding.


/// Live state for the views. One observable property per section, assigned only when it changed,
/// so a price tick re-renders the views that show prices and not the tables or the toolbar.
@MainActor @Observable
final class AppState {
    var base = "BTC"
    var mode = "Perp"
    var lang_zh = false
    var header = Header()
    var venues: [VenueStatus] = []
    var stats = Stats()
    var trade = TradeCtx()
    var positions: [PositionRow] = []
    var orders: [OrderRow] = []
    var tpsl: [TpSlRow] = []
    var history = HistoryState()
    var wallets: [VenueWallets] = []
    var balances: [BalanceRow] = []
    var algos: [AlgoJobRow] = []
    var transferring = false
    var binance_pm = false
    var log: [LogRow] = []
    var keys: [KeyStatus] = []
    var prefs = Prefs()
    var route = RoutePolicy()
    var tradable: [String] = []
    var book_click: BookClick? = nil
    var chart = ChartCtl()
    var book = BookCtl()
    var signals: [SignalRow] = []
    var quant = QuantState()

    func apply(_ n: Snapshot) {
        if base != n.base { base = n.base }
        if mode != n.mode { mode = n.mode }
        if lang_zh != n.lang_zh { lang_zh = n.lang_zh }
        if header != n.header { header = n.header }
        if venues != n.venues { venues = n.venues }
        if stats != n.stats { stats = n.stats }
        if trade != n.trade { trade = n.trade }
        if positions != n.positions { positions = n.positions }
        if orders != n.orders { orders = n.orders }
        if tpsl != n.tpsl { tpsl = n.tpsl }
        if history != n.history { history = n.history }
        if wallets != n.wallets { wallets = n.wallets }
        if balances != n.balances { balances = n.balances }
        if algos != n.algos { algos = n.algos }
        if transferring != n.transferring { transferring = n.transferring }
        if binance_pm != n.binance_pm { binance_pm = n.binance_pm }
        if log != n.log { log = n.log }
        if keys != n.keys { keys = n.keys }
        if prefs != n.prefs { prefs = n.prefs }
        if route != n.route { route = n.route }
        if tradable != n.tradable { tradable = n.tradable }
        if book_click != n.book_click { book_click = n.book_click }
        if chart != n.chart { chart = n.chart }
        if book != n.book { book = n.book }
        if signals != n.signals { signals = n.signals }
        if quant != n.quant { quant = n.quant }
    }
}

/// One decoded `t1_state` document.
struct Snapshot: Decodable {
    var base = "BTC"
    var mode = "Perp"
    var lang_zh = false
    var header = Header()
    var venues: [VenueStatus] = []
    var stats = Stats()
    var trade = TradeCtx()
    var positions: [PositionRow] = []
    var orders: [OrderRow] = []
    var tpsl: [TpSlRow] = []
    var history = HistoryState()
    var wallets: [VenueWallets] = []
    /// margin account per keyed venue (equity, available, risk)
    var balances: [BalanceRow] = []
    var algos: [AlgoJobRow] = []
    var transferring = false
    var binance_pm = false
    var log: [LogRow] = []
    var keys: [KeyStatus] = []
    var prefs = Prefs()
    var route = RoutePolicy()
    var tradable: [String] = []
    var book_click: BookClick? = nil
    var chart = ChartCtl()
    var book = BookCtl()
    var signals: [SignalRow] = []
    var quant = QuantState()

    init() {}
    init(from d: Decoder) throws {
        let c = try d.container(keyedBy: K.self)
        func v<T: Decodable>(_ k: K, _ def: T) -> T { (try? c.decodeIfPresent(T.self, forKey: k)) ?? def }
        base = v(.base, base); mode = v(.mode, mode); lang_zh = v(.lang_zh, lang_zh); header = v(.header, header)
        venues = v(.venues, venues); stats = v(.stats, stats); trade = v(.trade, trade); positions = v(.positions, positions)
        orders = v(.orders, orders); tpsl = v(.tpsl, tpsl); history = v(.history, history); wallets = v(.wallets, wallets)
        balances = v(.balances, balances); algos = v(.algos, algos); transferring = v(.transferring, transferring); binance_pm = v(.binance_pm, binance_pm); log = v(.log, log); keys = v(.keys, keys); prefs = v(.prefs, prefs)
        route = v(.route, route); tradable = v(.tradable, tradable); book_click = v(.book_click, book_click); chart = v(.chart, chart); book = v(.book, book); signals = v(.signals, signals); quant = v(.quant, quant)
    }
    enum K: String, CodingKey { case base, mode, lang_zh, header, venues, stats, trade, positions, orders, tpsl, history, wallets, balances, algos, transferring, binance_pm, log, keys, prefs, route, tradable, book_click, chart, book, signals, quant }
}

struct Header: Decodable, Equatable {
    var price: Double?; var oi: Double?; var oi_usd: Double?
    var funding_pred_bph: Double?; var funding_settled_bph: Double?; var basis_bps: Double?; var cvd: Double?
    var venue: String?; var mark: Double?; var index: Double?; var funding_rate: Double?; var funding_interval_h: Double?; var next_funding_ms: Int64?
    var high24: Double?; var low24: Double?; var vol24_base: Double?; var vol24_usd: Double?; var chg24: Double?
}

struct VenueStatus: Decodable, Equatable, Identifiable { var ex: String; var on: Bool; var alive: Bool; var lat_ms: Double?; var id: String { ex } }

struct Stats: Decodable, Equatable { var msgs_per_s: Double? = 0; var mem_mb: Double?; var backlog: Int? = 0; var history: [Int]? = [0, 0]; var errors: Int? = 0; var hist_errors: [String]? = [] }

struct TradeCtx: Decodable, Equatable {
    var venue = "Bybit"; var symbol = ""; var has_key = false; var verified = true
    var mode: String?; var lev: Double?; var available: Double?; var equity: Double?; var uni_mmr: Double?; var mm_rate: Double?
    var bid: Double?; var ask: Double?; var tick: Double?; var step: Double?; var min_qty: Double?; var min_notional: Double?
    var bbo_levels: [Int] = []; var caps = OrderCaps(); var native_twap: [Int]?; var push_ms: Double?; var rtt_ms: Double?; var fee = Fee(); var smart = false; var error: String?
}
struct Fee: Decodable, Equatable { var taker = 0.00055; var maker = 0.0002 }

struct PositionRow: Decodable, Equatable, Identifiable {
    var ex: String; var symbol: String; var side: String; var qty: Double; var entry: Double; var mark: Double
    var liq: Double?; var upnl: Double; var lev: Double; var margin: Double
    var cross: Bool?; var funding_rate: Double?; var funding_interval_h: Double?; var next_funding_ms: Int64?; var funding_est: Double?
    var id: String { "\(ex)|\(symbol)|\(side)" }
    var isLong: Bool { side == "long" }
    var base: String { symbol.hasSuffix("USDT") ? String(symbol.dropLast(4)) : symbol }
}

struct OrderRow: Decodable, Equatable, Identifiable {
    var ex: String; var symbol: String; var id: String; var side: String; var price: Double; var qty: Double; var filled: Double
    var kind: String; var reduce_only: Bool; var ts: Int64; var pos: String?
}

struct TpSlRow: Decodable, Equatable, Identifiable {
    var ex: String; var id: String; var symbol: String; var pos: String; var take_profit: Bool; var trigger_px: Double
    var qty: Double?; var trigger: String
}

struct HistoryState: Decodable, Equatable {
    var orders: [HistOrder] = []; var fills: [Fill] = []; var closed: [Closed] = []; var updated_ms: Int64?; var loading = false
}
struct HistOrder: Decodable, Equatable, Identifiable {
    var ex: String; var symbol: String; var side: String; var kind: String; var price: Double; var avg: Double; var qty: Double; var filled: Double; var status: String; var ts: Int64
    var id: String { "\(ex)\(symbol)\(ts)\(qty)\(price)" }
}
struct Fill: Decodable, Equatable, Identifiable {
    var ex: String; var symbol: String; var side: String; var price: Double; var qty: Double; var fee: Double; var realized: Double?; var ts: Int64
    var id: String { "\(ex)\(symbol)\(ts)\(qty)\(price)" }
}
struct Closed: Decodable, Equatable, Identifiable {
    var ex: String; var symbol: String; var long: Bool?; var qty: Double?; var entry: Double?; var exit: Double?; var pnl: Double; var ts: Int64
    var id: String { "\(ex)\(symbol)\(ts)\(pnl)" }
}

struct VenueWallets: Decodable, Equatable, Identifiable {
    var ex: String; var loading = false; var updated_ms: Int64?; var notes: [String] = []; var accounts: [WalletAccount] = []; var auto: AutoRule?
    var id: String { ex }
}
struct WalletAccount: Decodable, Equatable, Identifiable { var id: String; var usd: Double; var coins: [WalletCoin] }
struct WalletCoin: Decodable, Equatable, Identifiable { var coin: String; var qty: Double; var free: Double; var usd: Double; var product: String?; var id: String { coin } }
struct AutoRule: Decodable, Equatable { var enabled: Bool; var min: Double; var target: Double }

struct BalanceRow: Decodable, Equatable, Identifiable {
    var ex: String; var equity: Double; var available: Double; var uni_mmr: Double?; var mm_rate: Double?; var maint_margin: Double?; var adj_equity: Double?
    var id: String { ex }
}

struct LogRow: Decodable, Equatable, Identifiable { var ts: Int64; var msg: String; var ok: Bool; var id: String { "\(ts)\(msg)" } }

struct KeyStatus: Decodable, Equatable, Identifiable {
    var ex: String; var configured: Bool; var tail: String?; var verified: Bool; var test: KeyTest?; var labels: [String?] = []
    var id: String { ex }
}
struct KeyTest: Decodable, Equatable { var ok: Bool; var msg: String }

/// Settings > Appearance / Orders (round-trips through set_prefs).
struct Prefs: Codable, Equatable {
    var accent: [Int] = [76, 158, 235]; var red_up = false; var zoom = 1.0; var radius = 5; var confirm = true
    var fees: [String: [Double]] = [:]
}

/// Smart order routing policy (round-trips through set_route).
struct RoutePolicy: Codable, Equatable {
    var smart = false; var fixed = "Bybit"; var allow_split = true; var max_legs = 2
    var max_slip_bps = 10.0; var split_bps = 2.0; var min_split_notional = 500.0; var max_stale_ms = 1500; var max_disp_bps = 30.0
    var fees: [String: [Double]] = [:]
}

/// Result of a preview / place call.
struct RoutePlan: Decodable, Equatable {
    struct Leg: Decodable, Equatable, Identifiable {
        var ex: String; var pos: String; var close: Bool; var qty: Double; var kind: String; var ref_px: Double; var est_px: Double
        var est_fee: Double; var slip_bps: Double; var avail_after: Double; var client_id: String?
        var id: String { client_id ?? "\(ex)\(qty)" }
    }
    struct Excluded: Decodable, Equatable, Identifiable { var ex: String; var reason: String; var id: String { ex } }
    var legs: [Leg] = []; var excluded: [Excluded] = []; var clamped_from: Double?; var total_qty = 0.0; var vwap = 0.0; var fees = 0.0
}

struct Ticker: Decodable, Equatable, Identifiable { var base: String; var last: Double; var chg_pct: Double; var quote_vol: Double; var id: String { base } }

/// Price clicked in the egui order book (a single venue's raw level).
struct BookClick: Decodable, Equatable { var ex: String; var px: Double }

/// Chart controls (egui draws the chart; these drive it through the "chart" op).
struct ChartCtl: Decodable, Equatable {
    struct Pane: Decodable, Equatable, Identifiable { var key: String; var label: String; var on: Bool; var id: String { key } }
    var tf = 60; var tfs: [[TfItem]] = []; var quick = 6; var src: String?; var venues: [String] = []
    var panes: [Pane] = []; var ma = true; var heat = false; var liqmap = false; var sr = true; var walls = true; var mark = true; var breakouts = true; var vwap = true; var cone = true; var perp = true
    var tool: String?; var drawings = 0; var panned = false
    /// (minutes, label) pairs
    var intervals: [(Int, String)] { tfs.compactMap { p in if case .m(let m) = p.first, case .l(let l) = p.last { return (m, l) }; return nil } }
    static func == (a: ChartCtl, b: ChartCtl) -> Bool {
        a.tf == b.tf && a.src == b.src && a.venues == b.venues && a.panes == b.panes && a.ma == b.ma && a.heat == b.heat
            && a.liqmap == b.liqmap && a.sr == b.sr && a.walls == b.walls && a.mark == b.mark && a.breakouts == b.breakouts && a.vwap == b.vwap && a.cone == b.cone && a.perp == b.perp && a.tool == b.tool && a.drawings == b.drawings && a.panned == b.panned && a.tfs.count == b.tfs.count
    }
}
/// one element of a [minutes, "label"] pair
enum TfItem: Decodable, Equatable {
    case m(Int), l(String)
    init(from d: Decoder) throws {
        let c = try d.singleValueContainer()
        if let i = try? c.decode(Int.self) { self = .m(i) } else { self = .l(try c.decode(String.self)) }
    }
}

/// Order book header controls ("book" op).
struct BookCtl: Decodable, Equatable {
    var trades = false; var quote = false; var src: String?; var venues: [String] = []; var group = 0; var groups: [String] = []; var width: Double = 320
}

/// One detected signal (quant::Signal).
struct SignalRow: Decodable, Equatable, Identifiable {
    var ts: Int64; var kind: String; var severity: Int; var dir: Int; var title: String; var detail: String
    var id: String { "\(ts)\(kind)" }
}

/// Statistical models over the aggregated series.
struct QuantState: Decodable, Equatable {
    struct Vol: Decodable, Equatable { var sigma_1h: Double; var sigma_24h: Double; var percentile: Double; var range_1h: [Double]; var range_24h: [Double] }
    struct Carry: Decodable, Equatable, Identifiable { var ex: String; var funding_apr: Double?; var basis_bps: Double?; var id: String { ex } }
    var vol: Vol?; var pressure: Double = 0; var book_bid_usd: Double = 0; var book_ask_usd: Double = 0; var carry: [Carry] = []
}

/// Order types the trade venue accepts natively (trade::caps).
struct OrderCaps: Decodable, Equatable { var fok = false; var stop = false; var trailing = false; var post_only = true }

/// A client-side TWAP job.
struct AlgoJobRow: Decodable, Equatable, Identifiable {
    var id: Int64; var ex: String; var symbol: String; var buy: Bool; var close: Bool; var total: Double; var sent: Double
    var slices: Int; var done: Int; var started_ms: Int64; var end_ms: Int64; var status: String; var cancelled: Bool
    var native: Bool? = false
}
