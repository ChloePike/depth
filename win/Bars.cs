using System.Globalization;
using System.Text.Json.Nodes;
using Microsoft.UI.Text;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Controls.Primitives;
using Windows.UI;
using static Depth.I18n;

namespace Depth;

/// Market strip under the title bar (port of TopStats in TopBar.swift): the order venue's mark /
/// index / funding with countdown, 24h range and volume, open interest; then the cross-venue extras.
sealed class TopStats : Grid
{
    sealed class Stat
    {
        public readonly StackPanel View;
        readonly TextBlock value, extra;
        string? tip;

        public Stat(string caption)
        {
            value = Ui.Text("–", 13, num: true);
            value.FontWeight = FontWeights.Medium;
            value.MinWidth = 70;
            extra = Ui.Text("", 13, T1.Mu, num: true);
            View = Ui.V(1, Ui.Text(caption, 11, T1.Mu), Ui.H(5, value, extra));
        }

        public void Set(string v, Color? c = null, string? ex = null, string? tip = null)
        {
            value.Text = v;
            Ui.Fg(value, c);
            extra.Text = ex ?? "";
            if (tip != this.tip) { this.tip = tip; ToolTipService.SetToolTip(View, tip); }
        }

        public void Extra(string ex) => extra.Text = ex;
    }

    readonly StackPanel row = new() { Orientation = Orientation.Horizontal, Spacing = 24, Padding = new Thickness(14, 6, 14, 6) };
    readonly Dictionary<string, Stat> stats = new();
    string layout = "";

    public TopStats()
    {
        Children.Add(new ScrollViewer
        {
            Content = row,
            HorizontalScrollMode = ScrollMode.Enabled, HorizontalScrollBarVisibility = ScrollBarVisibility.Hidden,
            VerticalScrollMode = ScrollMode.Disabled, VerticalScrollBarVisibility = ScrollBarVisibility.Disabled,
        });
    }

    public void Update()
    {
        var s = Store.Shared.State;
        var h = s.Header;
        bool perp = s.Mode == "Perp";
        var iv = h.FundingIntervalH is double i ? $" ({i.ToString("G", CultureInfo.InvariantCulture)}h)" : "";
        var key = $"{s.Mode}|{s.Base}|{iv}|{s.Lang}|{T1.IsDark}";
        if (key != layout)
        {
            layout = key;
            row.Children.Clear();
            stats.Clear();
            void Add(string id, string caption) { var st = new Stat(caption); stats[id] = st; row.Children.Add(st.View); }
            if (perp)
            {
                Add("mark", L("Mark"));
                Add("index", L("Index"));
                Add("funding", $"{L("Funding")}{iv} / {L("Countdown")}");
            }
            Add("chg", L("24h Change"));
            Add("high", L("24h High"));
            Add("low", L("24h Low"));
            Add("volb", $"{L("24h Vol")} ({s.Base})");
            Add("volu", $"{L("24h Vol")} (USD)");
            if (perp)
            {
                Add("oi", L("Open Interest"));
                Add("pred", L("Pred. funding (all)"));
                Add("basis", L("Basis"));
            }
            if (s.Mode != "Option") Add("cvd", L("CVD"));
        }
        else if (!Store.Shared.Dirty("header")) return;

        void Set(string id, string v, Color? c = null, string? ex = null, string? tip = null) { if (stats.TryGetValue(id, out var st)) st.Set(v, c, ex, tip); }
        var vol = L("Summed across Binance, Bybit, OKX and Hyperliquid");
        Set("mark", Fmt.Px(h.Mark), tip: $"{h.Venue} {L("mark price")}");
        Set("index", Fmt.Px(h.Index), tip: $"{h.Venue} {L("index price")}");
        Set("funding", h.FundingRate is double r ? Fmt.F(r * 100, 4) + "%" : "–", h.FundingRate is double r2 ? (r2 >= 0 ? T1.Orange : T1.Down) : null,
            Countdown(h), $"{h.Venue} · {L("rate per settlement")}");
        Set("chg", h.Chg24 is double c ? Fmt.F(c, 2, true) + "%" : "–", Sign(h.Chg24));
        Set("high", Fmt.Px(h.High24));
        Set("low", Fmt.Px(h.Low24));
        Set("volb", Fmt.Big(h.Vol24Base), tip: vol);
        Set("volu", Fmt.Big(h.Vol24Usd), tip: vol);
        Set("oi", h.OiUsd is double o ? "$" + Fmt.Big(o) : "–", ex: h.Oi is double oi ? $"{Fmt.Big(oi)} {s.Base}" : null, tip: L("Sum of every connected perp venue"));
        Set("pred", Bph(h.FundingPredBph), Sign(h.FundingPredBph), tip: L("Open-interest-weighted across venues, per hour") + " · " + Apr(h.FundingPredBph));
        Set("basis", h.BasisBps is double b ? Fmt.F(b, 2, true) + " bp" : "–", Sign(h.BasisBps));
        Set("cvd", h.Cvd is double cvd ? Fmt.Signed(cvd, 1) : "–", Sign(h.Cvd));
    }

    /// Every second: the funding countdown.
    public void Tick() { if (stats.TryGetValue("funding", out var st)) st.Extra(Countdown(Store.Shared.State.Header)); }

    static string Countdown(Header h) =>
        "/ " + (h.NextFundingMs is long n ? Fmt.Hms(Math.Max(0, (n - DateTimeOffset.UtcNow.ToUnixTimeMilliseconds()) / 1000)) : "–");
    static string Bph(double? v) => v is double x ? Fmt.F(x, 4, true) + " bp/h" : "–";
    static string Apr(double? v) => v is double x ? $"{L("APR")} {Fmt.F(x / 1e4 * 24 * 365 * 100, 2, true)}%" : "";
    static Color? Sign(double? v) => v is double x && x != 0 ? (x > 0 ? T1.Up : T1.Down) : null;
}

/// Native controls above the egui chart (port of ChartBar.swift): interval, source, indicators,
/// drawing tools. The book's header controls live in BookPanel.
sealed class ChartBar : Grid
{
    readonly StackPanel row = new() { Orientation = Orientation.Horizontal, Spacing = 10, VerticalAlignment = VerticalAlignment.Center };

    public ChartBar()
    {
        Height = 34;
        Padding = new Thickness(10, 0, 10, 0);
        Children.Add(row);
    }

    public void Update()
    {
        if (!Store.Shared.Dirty("chart", "mode")) return;
        row.Children.Clear();
        var s = Store.Shared.State;
        if (s.Mode == "Option") return;
        var c = s.Chart;
        var iv = c.Intervals;
        var quick = iv.Take(c.Quick).ToList();
        var more = iv.Skip(c.Quick).ToList();

        // the quick intervals; a "more" interval in use shows as an extra selected segment
        var seg = quick.ToList();
        int sel = quick.Any(x => x.Min == c.Tf) ? c.Tf : -1;
        if (sel == -1) seg.Add((-1, more.FirstOrDefault(x => x.Min == c.Tf).Label ?? ""));
        row.Children.Add(Ui.Segmented(seg, sel, tf => { if (tf > 0) Chart(new() { ["tf"] = tf }); }));
        var mf = new MenuFlyout();
        foreach (var (m, l) in more) mf.Items.Add(Ui.Check(l, m == c.Tf, () => Chart(new() { ["tf"] = m })));
        row.Children.Add(Ui.Tip(Ui.Small(new Button { Content = "···", Flyout = mf }), L("More intervals")));

        var sf = new MenuFlyout();
        sf.Items.Add(Ui.Check(L("Aggregate"), c.Src == null, () => Chart(new() { ["src"] = null })));
        sf.Items.Add(new MenuFlyoutSeparator());
        foreach (var v in c.Venues) sf.Items.Add(Ui.Check(v, c.Src == v, () => Chart(new() { ["src"] = v })));
        row.Children.Add(Ui.Tip(Ui.Small(new DropDownButton { Content = Ui.H(6, Ui.Icon("", 12), Ui.Text(c.Src ?? L("Aggregate"), 12)), Flyout = sf }),
            L("Chart source: all venues or one venue")));

        var f = new MenuFlyout();
        void Toggle(string label, string key, bool on) => f.Items.Add(Ui.Check(label, on, () => Chart(new() { [key] = !on })));
        f.Items.Add(Ui.Header(L("Panes")));
        foreach (var p in c.Panes) f.Items.Add(Ui.Check(p.Label, p.On, () => Chart(new() { ["pane"] = p.Key, ["on"] = !p.On })));
        f.Items.Add(new MenuFlyoutSeparator());
        f.Items.Add(Ui.Header(L("Overlays")));
        Toggle("MA (7 / 25 / 99)", "ma", c.Ma);
        Toggle(L("Book heatmap"), "heat", c.Heat);
        if (c.Perp) Toggle(L("Liquidation map"), "liqmap", c.Liqmap);
        f.Items.Add(new MenuFlyoutSeparator());
        f.Items.Add(Ui.Header(L("Levels")));
        Toggle(L("Volume profile (POC · value area · HVN)"), "sr", c.Sr);
        Toggle(L("Breakouts (closes through VAH / VAL / HVN)"), "breakouts", c.Breakouts);
        Toggle(L("Session VWAP ±1σ ±2σ"), "vwap", c.Vwap);
        Toggle(L("Expected move cone (volatility model)"), "cone", c.Cone);
        Toggle(L("Liquidity walls"), "walls", c.Walls);
        if (c.Perp) Toggle(L("Mark price"), "mark", c.Mark);
        row.Children.Add(Ui.Small(new DropDownButton { Content = Ui.H(6, Ui.Icon("", 12), Ui.Text(L("Indicators"), 12)), Flyout = f }));

        var tools = Ui.H(2, Tool("hline", "—", L("Horizontal line"), c), Tool("trend", "╱", L("Trend line"), c), Tool("ray", "↗", L("Ray"), c));
        if (c.Drawings > 0)
        {
            var clear = Ui.Small(new Button { Content = Ui.Icon("", 12) });
            clear.Click += (_, _) => Chart(new() { ["clear_drawings"] = true });
            tools.Children.Add(Ui.Tip(clear, $"{L("Clear drawings")} ({c.Drawings})"));
        }
        row.Children.Add(tools);
        if (c.Panned)
        {
            var latest = Ui.Small(new Button { Content = Ui.H(6, Ui.Text("⇥", 12), Ui.Text(L("Latest"), 12)) });
            latest.Click += (_, _) => Chart(new() { ["reset_view"] = true });
            row.Children.Add(Ui.Tip(latest, L("Back to the live edge and default zoom")));
        }
    }

    static ToggleButton Tool(string id, string glyph, string tip, ChartCtl c)
    {
        var b = Ui.Small(new ToggleButton { Content = glyph, IsChecked = c.Tool == id });
        b.Click += (_, _) => Chart(new() { ["tool"] = c.Tool == id ? null : id });
        return Ui.Tip(b, tip);
    }

    static void Chart(JsonObject a) => Store.Shared.Call("chart", a);
}

/// Bottom strip (port of StatusBar.swift): per-venue connection toggles (off = disconnected, the
/// engine restarts), engine stats, clock in the display time zone, language.
sealed class StatusBar : Grid
{
    readonly StackPanel venues = Ui.H(4), stats = Ui.H(14);
    readonly TextBlock clock = Ui.Text("", 11, num: true);
    readonly ComboBox lang = new() { FontSize = 11, MinHeight = 0, Padding = new Thickness(8, 1, 4, 1), BorderThickness = new Thickness(0), VerticalAlignment = VerticalAlignment.Center };
    bool building;

    public StatusBar()
    {
        Padding = new Thickness(12, 4, 12, 4);
        ColumnSpacing = 4;
        foreach (var w in new[] { new GridLength(1, GridUnitType.Star), GridLength.Auto, GridLength.Auto, GridLength.Auto, GridLength.Auto, GridLength.Auto })
            ColumnDefinitions.Add(new ColumnDefinition { Width = w });
        // the venues scroll instead of imposing their width on the window
        var sv = new ScrollViewer
        {
            Content = venues,
            HorizontalScrollMode = ScrollMode.Enabled, HorizontalScrollBarVisibility = ScrollBarVisibility.Hidden,
            VerticalScrollMode = ScrollMode.Disabled, VerticalScrollBarVisibility = ScrollBarVisibility.Disabled,
        };
        UIElement[] cells = { sv, Ui.Divider(), stats, clock, Ui.Divider(), lang };
        for (int i = 0; i < cells.Length; i++) { Grid.SetColumn((FrameworkElement)cells[i], i); Children.Add(cells[i]); }
        clock.Foreground = T1.B(T1.Mu);
        clock.Margin = new Thickness(8, 0, 0, 0);
        lang.SelectionChanged += (_, _) =>
        {
            var ls = Store.Shared.State.Langs;
            if (!building && lang.SelectedIndex >= 0 && lang.SelectedIndex < ls.Count)
                Store.Shared.Call("set_lang", new() { ["lang"] = ls[lang.SelectedIndex][0] });
        };
    }

    public void Update()
    {
        var st = Store.Shared;
        var s = st.State;
        if (st.Dirty("venues"))
        {
            venues.Children.Clear();
            foreach (var v in s.Venues) venues.Children.Add(Venue(v));
        }
        if (st.Dirty("stats"))
        {
            var x = s.Stats;
            stats.Children.Clear();
            Color? Lvl(double v, double warn, double bad) => v > bad ? T1.Red : v > warn ? T1.Orange : T1.Mu;
            stats.Children.Add(Ui.Text($"{Fmt.Num(x.MsgsPerS ?? 0, 0)} {L("msg/s")}", 11, T1.Mu, num: true));
            if (x.MemMb is double mb) stats.Children.Add(Ui.Text($"{L("Mem")} {Fmt.Num(mb, 0)} MB", 11, Lvl(mb, 800, 1500), num: true));
            var bl = x.Backlog ?? 0;
            stats.Children.Add(Ui.Text($"{L("Backlog")} {bl}", 11, Lvl(bl, 1_000, 10_000), num: true));
            var h = x.History ?? new List<long> { 0, 0 };
            bool done = h.Count == 2 && h[0] >= h[1];
            stats.Children.Add(Ui.Text(done ? L("History loaded") : $"{L("History")} {h.FirstOrDefault()}/{h.LastOrDefault()}", 11, done ? T1.Mu : T1.Orange, num: true));
            if (x.Errors is long e && e > 0) stats.Children.Add(Ui.Text($"{e} {L("errors")}", 11, T1.Red, num: true));
        }
        if (st.Dirty("langs", "lang"))
        {
            building = true;
            lang.Items.Clear();
            foreach (var l in s.Langs) lang.Items.Add(l.Count > 1 ? l[1] : l[0]);
            lang.SelectedIndex = s.Langs.FindIndex(l => l.Count > 0 && l[0] == s.Lang);
            Ui.Tip(lang, L("Language"));
            building = false;
        }
    }

    /// Every second: the clock.
    public void Tick()
    {
        var now = DateTimeOffset.UtcNow;
        clock.Text = T1.TzLabel(T1.Offset(now)) + " " + T1.Local(now).ToString("HH:mm:ss", CultureInfo.InvariantCulture);
    }

    static Button Venue(VenueStatus v)
    {
        Color col = v.LatMs is double l && v.On ? (!v.Alive ? T1.Red : l < 150 ? T1.Green : l < 400 ? T1.Orange : T1.Red) : T1.Dim;
        var icon = T1.VenueIcon(v.Ex, 12);
        icon.Opacity = v.On ? 1 : 0.5;
        var b = Ui.Flat(Ui.H(5, icon, Ui.Text(v.Ex, 11, v.On ? null : T1.Dim),
            Ui.Text(v.On ? (v.LatMs is double ms ? $"{(int)ms}" : "–") : L("off"), 11, col, num: true)));
        b.Padding = new Thickness(0, 1, 8, 1);
        b.Click += (_, _) => Store.Shared.Call("toggle_venue", new() { ["ex"] = v.Ex });
        return Ui.Tip(b, $"{v.Ex}: " + (v.On ? L("Connected; click to disconnect") : L("Disconnected; click to connect")) + (v.LatMs is double t ? $" · {(int)t} ms" : ""));
    }
}
