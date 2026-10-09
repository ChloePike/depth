using Microsoft.UI.Dispatching;
using Microsoft.UI.Text;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Documents;
using Microsoft.UI.Xaml.Input;
using Windows.UI;
using Ellipse = Microsoft.UI.Xaml.Shapes.Ellipse;
using VirtualKey = Windows.System.VirtualKey;
using static Depth.I18n;

namespace Depth;

/// Favorites, recently opened pairs and pair switching, shared by the picker and the shortcuts.
static class Pairs
{
    public static readonly (string Id, string Label)[] Modes = { ("Spot", "Spot"), ("Margin", "Margin"), ("Perp", "Perpetual"), ("Option", "Options") };
    /// Ctrl+Shift+1 … 6
    public static readonly (int Min, string Label)[] Intervals = { (1, "1m"), (5, "5m"), (15, "15m"), (60, "1H"), (240, "4H"), (1440, "1D") };

    public static List<string> Favorites => HostPrefs.Get("favorites", new List<string> { "BTC", "ETH", "SOL" });
    /// most recent first, current pair included
    public static List<string> Recent => HostPrefs.Get("recent", new List<string>());

    public static void ToggleFavorite(string b)
    {
        var f = Favorites;
        if (!f.Remove(b)) f.Add(b);
        HostPrefs.Set("favorites", f);
    }

    public static void Open(string b)
    {
        var cur = Store.Shared.State.Base;
        if (b != cur) Store.Shared.Call("set_base", new() { ["base"] = b });
        HostPrefs.Set("recent", new[] { b, cur }.Concat(Recent).Distinct().Take(10).ToList());
    }

    /// Cycle through favorites in their saved order.
    public static void Step(int d)
    {
        var f = Favorites;
        if (f.Count == 0) return;
        int i = f.IndexOf(Store.Shared.State.Base);
        Open(f[i < 0 ? 0 : (i + d + f.Count) % f.Count]);
    }
}

/// Pair picker flyout (port of SymbolPicker.swift): USDT perpetuals from the engine's tickers,
/// search ranked exact > prefix > contains, sortable columns, favorites and recent pairs.
/// Click picks; arrows move the selection, Enter picks it (or the first match), Escape closes.
sealed class SymbolPicker : Grid
{
    // ponytail: rows are plain elements, not a virtualized list; capped so 600+ pairs stay cheap.
    // Upgrade path: ItemsRepeater with an IElementFactory if the cap ever hides a pair people want.
    const int MaxRows = 300;

    sealed class RowUi
    {
        public readonly Grid Root = new() { Height = 28, Padding = new Thickness(4, 0, 4, 0), CornerRadius = new CornerRadius(4), Background = T1.Clear };
        public readonly TextBlock Last = Cell(), Chg = Cell(), Vol = Cell();
        public readonly FontIcon Star = new() { FontSize = 12 };
        static TextBlock Cell() { var t = Ui.Text("", 12, num: true); t.TextAlignment = TextAlignment.Right; t.HorizontalAlignment = HorizontalAlignment.Right; return t; }
    }

    readonly string current;
    readonly Action close;
    readonly TextBox search = new() { PlaceholderText = L("Search pairs") };
    readonly Grid favSeg;
    readonly StackPanel recent = Ui.H(6);
    readonly Grid head = new() { Padding = new Thickness(4, 0, 4, 0) };
    readonly StackPanel list = new();
    readonly ScrollViewer scroll;
    readonly TextBlock empty = Ui.Text("", 13, T1.Mu);
    readonly Dictionary<string, RowUi> rows = new();
    readonly HashSet<string> favs = new(Pairs.Favorites);
    List<string> shown = new();
    List<Ticker> tickers = new();
    bool favsOnly = HostPrefs.Get("pickerFavs", false);
    string? sel;
    string sortKey = "quote_vol";
    bool desc = true;
    DispatcherQueueTimer? timer;

    static ColumnDefinition Col(double w) => new() { Width = w > 0 ? new GridLength(w) : new GridLength(1, GridUnitType.Star) };
    static void Cols(Grid g) { foreach (var w in new double[] { 24, 0, 96, 70, 64 }) g.ColumnDefinitions.Add(Col(w)); }

    public SymbolPicker(string current, Action close)
    {
        this.current = current;
        this.close = close;
        sel = current;
        Width = 480;
        Height = 560;
        RowSpacing = 10;
        foreach (var h in new[] { GridLength.Auto, GridLength.Auto, GridLength.Auto, GridLength.Auto, new GridLength(1, GridUnitType.Star) })
            RowDefinitions.Add(new RowDefinition { Height = h });

        search.TextChanged += (_, _) => { sel = null; Refresh(); };
        search.PreviewKeyDown += OnKey;
        Children.Add(search);

        favSeg = Ui.Segmented(new[] { (true, L("Favorites")), (false, L("All")) }, favsOnly, v => { favsOnly = v; HostPrefs.Set("pickerFavs", v); Refresh(); });
        var hint = Ui.Text($"Ctrl+[ Ctrl+]  {L("favorites")}  ·  Ctrl+D  {L("star")}", 10, T1.Dim);
        var top = Ui.Spread(favSeg, hint);
        Grid.SetRow(top, 1);
        Children.Add(top);

        var rec = Pairs.Recent.Where(b => b != current).Take(6).ToList();
        if (rec.Count > 0) recent.Children.Add(Ui.Text(L("Recent"), 11, T1.Mu));
        foreach (var b in rec)
        {
            var btn = Ui.Small(new Button { Content = Ui.H(4, T1.CoinIcon(b, 13), Ui.Text(b, 12)) });
            btn.Click += (_, _) => Pick(b);
            recent.Children.Add(btn);
        }
        Grid.SetRow(recent, 2);
        Children.Add(recent);

        Cols(head);
        Grid.SetRow(head, 3);
        Children.Add(head);
        BuildHead();

        scroll = new ScrollViewer { Content = list, VerticalScrollBarVisibility = ScrollBarVisibility.Auto };
        var area = new Grid();
        area.Children.Add(scroll);
        empty.HorizontalAlignment = HorizontalAlignment.Center;
        area.Children.Add(empty);
        Grid.SetRow(area, 4);
        Children.Add(area);
    }

    /// Flyout opened: focus the search field, refresh tickers every second (last / change / volume move).
    public void Start()
    {
        search.Focus(FocusState.Programmatic);
        timer = DispatcherQueue.GetForCurrentThread().CreateTimer();
        timer.Interval = TimeSpan.FromSeconds(1);
        timer.Tick += (_, _) => Load();
        timer.Start();
        Load();
    }

    public void Stop() => timer?.Stop();

    void Load()
    {
        if (Store.Shared.Query<TickersReply>("tickers") is { } r) tickers = r.Tickers;
        Refresh();
    }

    string Query => search.Text.Trim().ToUpperInvariant().Replace("USDT", "").Replace("/", "");

    List<Ticker> Rows()
    {
        var q = Query;
        // 0 exact, 1 prefix, 2 contains; the column sort orders within each group (OrderBy is stable)
        int Rank(string b) => q.Length == 0 || b == q ? 0 : b.StartsWith(q, StringComparison.Ordinal) ? 1 : 2;
        var f = tickers.Where(t => (q.Length == 0 || t.Base.Contains(q, StringComparison.Ordinal)) && (!favsOnly || q.Length > 0 || favs.Contains(t.Base)));
        IOrderedEnumerable<Ticker> s = sortKey switch
        {
            "base" => desc ? f.OrderByDescending(t => t.Base, StringComparer.Ordinal) : f.OrderBy(t => t.Base, StringComparer.Ordinal),
            "last" => desc ? f.OrderByDescending(t => t.Last) : f.OrderBy(t => t.Last),
            "chg_pct" => desc ? f.OrderByDescending(t => t.ChgPct) : f.OrderBy(t => t.ChgPct),
            _ => desc ? f.OrderByDescending(t => t.QuoteVol) : f.OrderBy(t => t.QuoteVol),
        };
        return s.ToList().OrderBy(t => Rank(t.Base)).Take(MaxRows).ToList();
    }

    void Refresh()
    {
        var q = Query;
        foreach (var c in favSeg.Children.OfType<Control>()) c.IsEnabled = q.Length == 0;
        recent.Visibility = q.Length == 0 && recent.Children.Count > 0 ? Visibility.Visible : Visibility.Collapsed;
        var rs = Rows();
        var bases = rs.Select(t => t.Base).ToList();
        if (!bases.SequenceEqual(shown))
        {
            list.Children.Clear();
            foreach (var b in bases) list.Children.Add(Row(b).Root);
            shown = bases;
        }
        foreach (var t in rs)
        {
            var r = rows[t.Base];
            r.Last.Text = Fmt.Px(t.Last);
            r.Chg.Text = Fmt.F(t.ChgPct, 2, true) + "%";
            r.Chg.Foreground = T1.B(t.ChgPct >= 0 ? T1.Up : T1.Down);
            r.Vol.Text = Fmt.Big(t.QuoteVol);
        }
        if (sel == null || !shown.Contains(sel)) sel = shown.FirstOrDefault();
        Highlight();
        empty.Text = rs.Count > 0 ? "" : tickers.Count == 0 ? L("Loading…") : favsOnly && q.Length == 0 ? L("No favorites yet: star a pair in All") : L("No matching pairs");
        empty.Visibility = rs.Count > 0 ? Visibility.Collapsed : Visibility.Visible;
    }

    RowUi Row(string b)
    {
        if (rows.TryGetValue(b, out var r)) return r;
        r = new RowUi();
        Cols(r.Root);
        var star = Ui.Flat(r.Star);
        star.Click += (_, _) => { Pairs.ToggleFavorite(b); if (!favs.Remove(b)) favs.Add(b); StarIcon(r, b); if (favsOnly) Refresh(); };
        StarIcon(r, b);
        r.Root.Children.Add(star);

        var name = new TextBlock { VerticalAlignment = VerticalAlignment.Center, FontSize = 12 };
        name.Inlines.Add(new Run { Text = b, FontWeight = FontWeights.Medium });
        name.Inlines.Add(new Run { Text = "USDT", Foreground = T1.B(T1.Mu) });
        var pair = Ui.H(6, T1.CoinIcon(b, 16), name);
        if (b == current) pair.Children.Add(Ui.Text("✓", 10, T1.Mu));
        // the tap target spans the pair and the numbers, not the star
        var hit = new Grid { Background = T1.Clear };
        foreach (var w in new double[] { 0, 96, 70, 64 }) hit.ColumnDefinitions.Add(Col(w));
        Grid.SetColumn(hit, 1);
        Grid.SetColumnSpan(hit, 4);
        FrameworkElement[] cells = { pair, r.Last, r.Chg, r.Vol };
        for (int c = 0; c < cells.Length; c++) { Grid.SetColumn(cells[c], c); hit.Children.Add(cells[c]); }
        r.Vol.Foreground = T1.B(T1.Mu);
        hit.Tapped += (_, _) => Pick(b);
        r.Root.Children.Add(hit);
        return rows[b] = r;
    }

    void StarIcon(RowUi r, string b)
    {
        bool on = favs.Contains(b);
        r.Star.Glyph = on ? "" : "";
        r.Star.Foreground = T1.B(on ? Color.FromArgb(255, 0xff, 0xcc, 0x00) : T1.Dim);
    }

    void Highlight()
    {
        foreach (var (b, r) in rows) r.Root.Background = b == sel ? T1.B(T1.Hl) : T1.Clear;
    }

    void BuildHead()
    {
        head.Children.Clear();
        foreach (var (key, label, col) in new[] { ("base", L("Pair"), 1), ("last", L("Last"), 2), ("chg_pct", L("24h Chg"), 3), ("quote_vol", L("Volume"), 4) })
        {
            var text = label + (sortKey == key ? (desc ? " ▾" : " ▴") : "");
            var b = Ui.Flat(Ui.Text(text, 11, T1.Mu));
            b.HorizontalAlignment = col == 1 ? HorizontalAlignment.Left : HorizontalAlignment.Right;
            b.Click += (_, _) =>
            {
                if (sortKey == key) desc = !desc;
                else { sortKey = key; desc = key != "base"; }
                BuildHead();
                Refresh();
            };
            Grid.SetColumn(b, col);
            head.Children.Add(b);
        }
    }

    void OnKey(object sender, KeyRoutedEventArgs e)
    {
        switch (e.Key)
        {
            case VirtualKey.Down: Move(1); e.Handled = true; break;
            case VirtualKey.Up: Move(-1); e.Handled = true; break;
            case VirtualKey.Enter:
                if ((sel ?? shown.FirstOrDefault()) is string b) Pick(b);
                e.Handled = true;
                break;
        }
    }

    void Move(int d)
    {
        if (shown.Count == 0) return;
        int i = sel == null ? -1 : shown.IndexOf(sel);
        if (i < 0) i = d > 0 ? -1 : shown.Count;
        sel = shown[Math.Clamp(i + d, 0, shown.Count - 1)];
        Highlight();
        rows[sel].Root.StartBringIntoView();
    }

    void Pick(string b)
    {
        Stop();
        close();
        Pairs.Open(b);
    }
}

/// Regular trading sessions of the big stock markets, in each exchange's own time zone, so
/// daylight saving is handled by the time zone rules.
/// ponytail: weekdays only, public holidays and half days are not known; add a holiday table if it matters.
sealed class StockMarket
{
    public readonly string Name, Short;
    /// (start, end) in minutes after local midnight; two entries where there is a lunch break
    readonly (int Start, int End)[] sessions;
    readonly TimeZoneInfo zone;

    StockMarket(string name, string shortName, string iana, params (int, int)[] sessions)
    {
        Name = name; Short = shortName; this.sessions = sessions; zone = Zone(iana);
    }

    public static readonly StockMarket[] All =
    {
        new("New York", "NYSE", "America/New_York", (570, 960)),
        new("London", "LSE", "Europe/London", (480, 990)),
        new("Frankfurt", "Xetra", "Europe/Berlin", (540, 1050)),
        new("Tokyo", "TSE", "Asia/Tokyo", (540, 690), (750, 930)),
        new("Hong Kong", "HKEX", "Asia/Hong_Kong", (570, 720), (780, 960)),
        new("Shanghai", "SSE", "Asia/Shanghai", (570, 690), (780, 900)),
    };

    static TimeZoneInfo Zone(string iana)
    {
        try { return TimeZoneInfo.FindSystemTimeZoneById(iana); } catch (Exception) { }
        if (TimeZoneInfo.TryConvertIanaIdToWindowsId(iana, out var w))
            try { return TimeZoneInfo.FindSystemTimeZoneById(w); } catch (Exception) { }
        return TimeZoneInfo.Utc;
    }

    /// Open now (and when that session ends), or closed and when the next session starts.
    public (bool Open, DateTimeOffset Until) Status(DateTimeOffset now)
    {
        var today = TimeZoneInfo.ConvertTime(now, zone).DateTime.Date;
        for (int d = 0; d < 8; d++)
        {
            var day = today.AddDays(d);
            if (day.DayOfWeek is DayOfWeek.Saturday or DayOfWeek.Sunday) continue;
            foreach (var (a, b) in sessions)
            {
                var s = Utc(day.AddMinutes(a));
                var e = Utc(day.AddMinutes(b));
                if (now < s) return (false, s);
                if (now < e) return (true, e);
            }
        }
        return (false, now);
    }

    DateTimeOffset Utc(DateTime local) => new(TimeZoneInfo.ConvertTimeToUtc(DateTime.SpecifyKind(local, DateTimeKind.Unspecified), zone), TimeSpan.Zero);

    /// 1d 02:03:04 / 02:03:04
    public static string Left(DateTimeOffset t, DateTimeOffset now)
    {
        long s = Math.Max(0, (long)(t - now).TotalSeconds);
        var hms = $"{s / 3600 % 24:00}:{s % 3600 / 60:00}:{s % 60:00}";
        return s >= 86400 ? $"{s / 86400}d {hms}" : hms;
    }
}

/// Title bar item: the next market to open with a countdown; click for every market.
sealed class MarketClock
{
    public readonly Button View;
    readonly TextBlock label = Ui.Text("", 12, num: true);
    readonly Grid grid = new() { ColumnSpacing = 14, RowSpacing = 8, Padding = new Thickness(4), MinWidth = 280 };
    bool shown;
    string? tip;

    public MarketClock()
    {
        var fly = new Flyout { Content = grid };
        fly.Opened += (_, _) => { shown = true; Tick(); };
        fly.Closed += (_, _) => shown = false;
        View = Ui.Small(new Button { Content = label, Flyout = fly });
        foreach (var w in new[] { GridLength.Auto, GridLength.Auto, GridLength.Auto, GridLength.Auto })
            grid.ColumnDefinitions.Add(new ColumnDefinition { Width = w });
    }

    /// Every second.
    public void Tick()
    {
        var now = DateTimeOffset.UtcNow;
        var st = StockMarket.All.Select(m => (m, s: m.Status(now))).ToList();
        var next = st.Where(x => !x.s.Open).OrderBy(x => x.s.Until).FirstOrDefault();
        label.Text = next.m != null ? $"{next.m.Short} {L("opens")} {StockMarket.Left(next.s.Until, now)}" : L("Markets");
        var tip = L("Stock market sessions");
        if (tip != this.tip) ToolTipService.SetToolTip(View, this.tip = tip);
        if (!shown) return;
        grid.Children.Clear();
        grid.RowDefinitions.Clear();
        for (int i = 0; i < st.Count; i++)
        {
            var (m, s) = st[i];
            grid.RowDefinitions.Add(new RowDefinition { Height = GridLength.Auto });
            var dot = new Ellipse { Width = 7, Height = 7, Fill = s.Open ? T1.B(T1.Up) : T1.B(T1.Mu, 0.4), VerticalAlignment = VerticalAlignment.Center };
            var name = Ui.V(1, Ui.Text(L(m.Name), 13), Ui.Text(m.Short, 11, T1.Mu));
            var what = Ui.Text(s.Open ? L("Closes in") : L("Opens in"), 13, T1.Mu);
            var left = Ui.Text(StockMarket.Left(s.Until, now), 13, num: true);
            left.HorizontalAlignment = HorizontalAlignment.Right;
            FrameworkElement[] cells = { dot, name, what, left };
            for (int c = 0; c < cells.Length; c++) { Grid.SetRow(cells[c], i); Grid.SetColumn(cells[c], c); grid.Children.Add(cells[c]); }
        }
    }
}
