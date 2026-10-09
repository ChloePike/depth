using System.Text.Json;
using System.Text.Json.Serialization;

namespace Depth;

// Mirrors of the JSON produced by ui/src/native.rs (App::state_json), port of Models.swift.
// Names map through the snake_case policy (Json.Opts); every field optional or defaulted so a
// Rust-side addition never breaks decoding, and Store decodes each top-level section on its own.

/// One decoded t1_state document.
sealed class AppState
{
    public string Base { get; set; } = "BTC";
    /// "Spot" | "Margin" | "Perp" | "Option"
    public string Mode { get; set; } = "Perp";
    public string Lang { get; set; } = "en";
    /// [code, native name] of every UI language
    public List<List<string>> Langs { get; set; } = new();
    public Header Header { get; set; } = new();
    public List<VenueStatus> Venues { get; set; } = new();
    public Stats Stats { get; set; } = new();
    public TradeCtx Trade { get; set; } = new();
    public List<PositionRow> Positions { get; set; } = new();
    public List<OrderRow> Orders { get; set; } = new();
    public List<TpSlRow> Tpsl { get; set; } = new();
    public HistoryState History { get; set; } = new();
    public List<VenueWallets> Wallets { get; set; } = new();
    /// margin account per keyed venue (equity, available, risk)
    public List<BalanceRow> Balances { get; set; } = new();
    public List<AlgoJobRow> Algos { get; set; } = new();
    public bool Transferring { get; set; }
    public bool BinancePm { get; set; }
    public List<LogRow> Log { get; set; } = new();
    public List<KeyStatus> Keys { get; set; } = new();
    public Prefs Prefs { get; set; } = new();
    public RoutePolicy Route { get; set; } = new();
    public List<string> Tradable { get; set; } = new();
    public BookClick? BookClick { get; set; }
    public ChartCtl Chart { get; set; } = new();
    public BookCtl Book { get; set; } = new();
    public List<SignalRow> Signals { get; set; } = new();
    public QuantState Quant { get; set; } = new();
}

sealed class Header
{
    public double? Price { get; set; }
    public double? Oi { get; set; }
    public double? OiUsd { get; set; }
    public double? FundingPredBph { get; set; }
    public double? FundingSettledBph { get; set; }
    public double? BasisBps { get; set; }
    public double? Cvd { get; set; }
    /// the order venue: its own perp mark / index / funding
    public string? Venue { get; set; }
    public double? Mark { get; set; }
    public double? Index { get; set; }
    /// rate per settlement when the interval is known, else per hour
    public double? FundingRate { get; set; }
    public double? FundingIntervalH { get; set; }
    public long? NextFundingMs { get; set; }
    public double? High24 { get; set; }
    public double? Low24 { get; set; }
    public double? Vol24Base { get; set; }
    public double? Vol24Usd { get; set; }
    public double? Chg24 { get; set; }
}

sealed class VenueStatus
{
    public string Ex { get; set; } = "";
    public bool On { get; set; }
    public bool Alive { get; set; }
    public double? LatMs { get; set; }
}

sealed class Stats
{
    public double? MsgsPerS { get; set; }
    public double? MemMb { get; set; }
    public long? Backlog { get; set; }
    /// [loaded, total]
    public List<long>? History { get; set; }
    public long? Errors { get; set; }
    public List<string>? HistErrors { get; set; }
}

sealed class TradeCtx
{
    public string Venue { get; set; } = "Bybit";
    public string Symbol { get; set; } = "";
    public bool HasKey { get; set; }
    public bool Verified { get; set; } = true;
    /// "hedge" | "oneway"
    public string? Mode { get; set; }
    public double? Lev { get; set; }
    public double? MaxLev { get; set; }
    public double? Available { get; set; }
    public double? Equity { get; set; }
    public double? UniMmr { get; set; }
    public double? MmRate { get; set; }
    public double? Bid { get; set; }
    public double? Ask { get; set; }
    public double? Tick { get; set; }
    public double? Step { get; set; }
    public double? MinQty { get; set; }
    public double? MinNotional { get; set; }
    public List<int> BboLevels { get; set; } = new();
    public OrderCaps Caps { get; set; } = new();
    /// [min, max] minutes when the venue runs TWAP itself
    public List<int>? NativeTwap { get; set; }
    public double? PushMs { get; set; }
    public double? RttMs { get; set; }
    public Fee Fee { get; set; } = new();
    public bool Smart { get; set; }
    public string? Error { get; set; }
}

sealed class Fee
{
    public double Taker { get; set; } = 0.00055;
    public double Maker { get; set; } = 0.0002;
}

/// Order types the trade venue accepts natively (trade::caps).
sealed class OrderCaps
{
    public bool Fok { get; set; }
    public bool Stop { get; set; }
    public bool Trailing { get; set; }
    public bool PostOnly { get; set; } = true;
}

sealed class PositionRow
{
    public string Ex { get; set; } = "";
    public string Symbol { get; set; } = "";
    /// "long" | "short"
    public string Side { get; set; } = "";
    public double Qty { get; set; }
    public double Entry { get; set; }
    public double Mark { get; set; }
    public double? Liq { get; set; }
    public double Upnl { get; set; }
    public double Lev { get; set; }
    public double Margin { get; set; }
    public bool? Cross { get; set; }
    public double? FundingRate { get; set; }
    public double? FundingIntervalH { get; set; }
    public long? NextFundingMs { get; set; }
    public double? FundingEst { get; set; }
    [JsonIgnore] public string Id => $"{Ex}|{Symbol}|{Side}";
    [JsonIgnore] public bool IsLong => Side == "long";
    [JsonIgnore] public string Base => Symbol.EndsWith("USDT") ? Symbol[..^4] : Symbol;
}

sealed class OrderRow
{
    public string Ex { get; set; } = "";
    public string Symbol { get; set; } = "";
    public string Id { get; set; } = "";
    /// "buy" | "sell"
    public string Side { get; set; } = "";
    public double Price { get; set; }
    public double Qty { get; set; }
    public double Filled { get; set; }
    public string Kind { get; set; } = "";
    public bool ReduceOnly { get; set; }
    public long Ts { get; set; }
    /// position side in hedge mode
    public string? Pos { get; set; }
}

sealed class TpSlRow
{
    public string Ex { get; set; } = "";
    public string Id { get; set; } = "";
    public string Symbol { get; set; } = "";
    public string Pos { get; set; } = "";
    public bool TakeProfit { get; set; }
    public double TriggerPx { get; set; }
    public double? Qty { get; set; }
    /// "last" | "mark"
    public string Trigger { get; set; } = "mark";
}

sealed class HistoryState
{
    public List<HistOrder> Orders { get; set; } = new();
    public List<FillRow> Fills { get; set; } = new();
    public List<ClosedRow> Closed { get; set; } = new();
    public long? UpdatedMs { get; set; }
    public bool Loading { get; set; }
}

sealed class HistOrder
{
    public string Ex { get; set; } = "";
    public string Symbol { get; set; } = "";
    public string Side { get; set; } = "";
    public string Kind { get; set; } = "";
    public double Price { get; set; }
    public double Avg { get; set; }
    public double Qty { get; set; }
    public double Filled { get; set; }
    public string Status { get; set; } = "";
    public long Ts { get; set; }
}

sealed class FillRow
{
    public string Ex { get; set; } = "";
    public string Symbol { get; set; } = "";
    public string Side { get; set; } = "";
    public double Price { get; set; }
    public double Qty { get; set; }
    public double Fee { get; set; }
    public double? Realized { get; set; }
    public long Ts { get; set; }
}

sealed class ClosedRow
{
    public string Ex { get; set; } = "";
    public string Symbol { get; set; } = "";
    public bool? Long { get; set; }
    public double? Qty { get; set; }
    public double? Entry { get; set; }
    public double? Exit { get; set; }
    public double Pnl { get; set; }
    public long Ts { get; set; }
}

sealed class VenueWallets
{
    public string Ex { get; set; } = "";
    public bool Loading { get; set; }
    public long? UpdatedMs { get; set; }
    public List<string> Notes { get; set; } = new();
    public List<WalletAccount> Accounts { get; set; } = new();
    public AutoRule? Auto { get; set; }
}

sealed class WalletAccount
{
    public string Id { get; set; } = "";
    public double Usd { get; set; }
    public List<WalletCoin> Coins { get; set; } = new();
}

sealed class WalletCoin
{
    public string Coin { get; set; } = "";
    public double Qty { get; set; }
    public double Free { get; set; }
    public double Usd { get; set; }
    public string? Product { get; set; }
}

sealed class AutoRule
{
    public bool Enabled { get; set; }
    public double Min { get; set; }
    public double Target { get; set; }
}

sealed class BalanceRow
{
    public string Ex { get; set; } = "";
    public double Equity { get; set; }
    public double Available { get; set; }
    public double? UniMmr { get; set; }
    public double? MmRate { get; set; }
    public double? MaintMargin { get; set; }
    public double? AdjEquity { get; set; }
}

sealed class LogRow
{
    public long Ts { get; set; }
    public string Msg { get; set; } = "";
    public bool Ok { get; set; }
}

sealed class KeyStatus
{
    public string Ex { get; set; } = "";
    public bool Configured { get; set; }
    public string? Tail { get; set; }
    public bool Verified { get; set; }
    public KeyTest? Test { get; set; }
    /// field labels: key, secret, extra (passphrase) or null
    public List<string?> Labels { get; set; } = new();
}

sealed class KeyTest
{
    public bool Ok { get; set; }
    public string Msg { get; set; } = "";
}

/// Settings > Appearance / Orders (round-trips through Store.SetPrefs).
sealed class Prefs
{
    /// "system" | "dark" | "light"
    public string Theme { get; set; } = "system";
    /// display time zone, minutes from UTC; null follows the system
    public int? TzMin { get; set; }
    public List<int> Accent { get; set; } = new() { 76, 158, 235 };
    public bool RedUp { get; set; }
    public double Zoom { get; set; } = 1.0;
    public int Radius { get; set; } = 5;
    public bool Confirm { get; set; } = true;
    /// venue -> [taker, maker] in percent
    public Dictionary<string, double[]> Fees { get; set; } = new();
}

/// Smart order routing policy (round-trips through Store.SetRoute).
sealed class RoutePolicy
{
    public bool Smart { get; set; }
    public string Fixed { get; set; } = "Bybit";
    public bool AllowSplit { get; set; } = true;
    public int MaxLegs { get; set; } = 2;
    public double MaxSlipBps { get; set; } = 10.0;
    public double SplitBps { get; set; } = 2.0;
    public double MinSplitNotional { get; set; } = 500.0;
    public long MaxStaleMs { get; set; } = 1500;
    public double MaxDispBps { get; set; } = 30.0;
    /// venue -> [taker, maker] as fractions
    public Dictionary<string, double[]> Fees { get; set; } = new();
}

/// Reply of a "preview" / "place" call.
sealed class RoutePlan
{
    public sealed class Leg
    {
        public string Ex { get; set; } = "";
        public string Pos { get; set; } = "";
        public bool Close { get; set; }
        public double Qty { get; set; }
        public string Kind { get; set; } = "";
        public double RefPx { get; set; }
        public double EstPx { get; set; }
        public double EstFee { get; set; }
        public double SlipBps { get; set; }
        public double AvailAfter { get; set; }
        public string? ClientId { get; set; }
    }
    public sealed class Excluded
    {
        public string Ex { get; set; } = "";
        public string Reason { get; set; } = "";
    }
    public List<Leg> Legs { get; set; } = new();
    [JsonPropertyName("excluded")] public List<Excluded> ExcludedVenues { get; set; } = new();
    public double? ClampedFrom { get; set; }
    public double TotalQty { get; set; }
    public double Vwap { get; set; }
    public double Fees { get; set; }
}

sealed class Ticker
{
    public string Base { get; set; } = "";
    public double Last { get; set; }
    public double ChgPct { get; set; }
    public double QuoteVol { get; set; }
}

sealed class TickersReply
{
    public List<Ticker> Tickers { get; set; } = new();
}

/// Price clicked in the egui order book (a single venue's raw level).
sealed class BookClick
{
    public string Ex { get; set; } = "";
    public double Px { get; set; }
}

/// Chart controls (egui draws the chart; these drive it through the "chart" op).
sealed class ChartCtl
{
    public sealed class Pane
    {
        public string Key { get; set; } = "";
        public string Label { get; set; } = "";
        public bool On { get; set; }
    }
    public int Tf { get; set; } = 60;
    /// [minutes, "label"] pairs
    public List<List<JsonElement>> Tfs { get; set; } = new();
    public int Quick { get; set; } = 6;
    public string? Src { get; set; }
    public List<string> Venues { get; set; } = new();
    public List<Pane> Panes { get; set; } = new();
    public bool Ma { get; set; } = true;
    public bool Heat { get; set; }
    public bool Liqmap { get; set; }
    public bool Sr { get; set; } = true;
    public bool Walls { get; set; } = true;
    public bool Mark { get; set; } = true;
    public bool Breakouts { get; set; } = true;
    public bool Vwap { get; set; } = true;
    public bool Cone { get; set; } = true;
    public bool Perp { get; set; } = true;
    /// "hline" | "trend" | "ray" | null
    public string? Tool { get; set; }
    public int Drawings { get; set; }
    public bool Panned { get; set; }

    [JsonIgnore]
    public List<(int Min, string Label)> Intervals => Tfs
        .Where(p => p.Count == 2 && p[0].ValueKind == JsonValueKind.Number && p[1].ValueKind == JsonValueKind.String)
        .Select(p => (p[0].GetInt32(), p[1].GetString() ?? "")).ToList();
}

/// Order book header controls ("book" op).
sealed class BookCtl
{
    public bool Trades { get; set; }
    /// sizes in quote (USDT) instead of base
    public bool Quote { get; set; }
    public string? Src { get; set; }
    public List<string> Venues { get; set; } = new();
    public int Group { get; set; }
    public List<string> Groups { get; set; } = new();
    public double Width { get; set; } = 320;
}

/// One detected signal (quant::Signal).
sealed class SignalRow
{
    public long Ts { get; set; }
    public string Kind { get; set; } = "";
    public int Severity { get; set; }
    public int Dir { get; set; }
    public string Title { get; set; } = "";
    public string Detail { get; set; } = "";
}

/// Statistical models over the aggregated series.
sealed class QuantState
{
    public sealed class VolModel
    {
        [JsonPropertyName("sigma_1h")] public double Sigma1h { get; set; }
        [JsonPropertyName("sigma_24h")] public double Sigma24h { get; set; }
        public double Percentile { get; set; }
        [JsonPropertyName("range_1h")] public List<double> Range1h { get; set; } = new();
        [JsonPropertyName("range_24h")] public List<double> Range24h { get; set; } = new();
    }
    public sealed class Carry
    {
        public string Ex { get; set; } = "";
        public double? FundingApr { get; set; }
        public double? BasisBps { get; set; }
    }
    public VolModel? Vol { get; set; }
    public double Pressure { get; set; }
    public double BookBidUsd { get; set; }
    public double BookAskUsd { get; set; }
    [JsonPropertyName("carry")] public List<Carry> CarryRows { get; set; } = new();
}

/// A client-side (or venue-native) TWAP job.
sealed class AlgoJobRow
{
    public long Id { get; set; }
    public string Ex { get; set; } = "";
    public string Symbol { get; set; } = "";
    public bool Buy { get; set; }
    public bool Close { get; set; }
    public double Total { get; set; }
    public double Sent { get; set; }
    public int Slices { get; set; }
    public int Done { get; set; }
    public long StartedMs { get; set; }
    public long EndMs { get; set; }
    public string Status { get; set; } = "";
    public bool Cancelled { get; set; }
    [JsonPropertyName("native")] public bool? IsNative { get; set; }
}
