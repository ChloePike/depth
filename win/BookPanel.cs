using System.Text.Json.Nodes;
using Microsoft.UI.Dispatching;
using Microsoft.UI.Text;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Windows.UI;
using Rectangle = Microsoft.UI.Xaml.Shapes.Rectangle;
using static Depth.I18n;

namespace Depth;

/// Book column header laid over the egui-drawn rows (port of BookPanel.swift). Book and Trades
/// leave the rows area transparent and hit-test free (click-to-fill goes to egui); the Venues and
/// Quant pages cover the rows with their own opaque view.
sealed class BookPanel : Grid
{
    /// 0 book, 1 trades, 2 venues, 3 quant
    int pane;
    int window = HostPrefs.Get("venueWindow", 60);
    readonly Grid header = new() { Height = 62, RowSpacing = 0 };
    readonly VenueShare share = new();
    readonly QuantPanel quant = new();

    public BookPanel()
    {
        RowDefinitions.Add(new RowDefinition { Height = GridLength.Auto });
        RowDefinitions.Add(new RowDefinition { Height = new GridLength(1, GridUnitType.Star) });
        header.RowDefinitions.Add(new RowDefinition { Height = new GridLength(34) });
        header.RowDefinitions.Add(new RowDefinition { Height = new GridLength(28) });
        Children.Add(header);
        foreach (var v in new FrameworkElement[] { share.View, quant.View }) { Grid.SetRow(v, 1); v.Visibility = Visibility.Collapsed; Children.Add(v); }
        Unloaded += (_, _) => share.Stop();
    }

    int Effective => pane > 1 ? pane : Store.Shared.State.Book.Trades ? 1 : 0;

    public void Update()
    {
        var st = Store.Shared;
        if (st.Dirty("book", "base", "mode")) Build();
        if (pane == 3 && st.Dirty("quant")) quant.Update();
    }

    void Build()
    {
        var b = Store.Shared.State.Book;
        header.Background = T1.B(T1.Bg);
        share.View.Background = quant.View.Background = T1.B(T1.Bg);
        header.Children.Clear();
        var seg = Ui.Segmented(new[] { (0, L("Book")), (1, L("Trades")), (2, L("Venues")), (3, L("Quant")) }, Effective, Pick, stretch: true);
        seg.Margin = new Thickness(10, 8, 10, 0);
        header.Children.Add(seg);
        // second row: the active tab's options
        var opts = Ui.H(6);
        opts.HorizontalAlignment = HorizontalAlignment.Right;
        opts.Margin = new Thickness(10, 0, 10, 0);
        if (pane == 2)
        {
            var c = new ComboBox { FontSize = 12, MinHeight = 0, Padding = new Thickness(8, 1, 4, 1), VerticalAlignment = VerticalAlignment.Center };
            int[] ws = { 5, 60, 1440 };
            foreach (var l in new[] { "5m", "1h", "24h" }) c.Items.Add(l);
            c.SelectedIndex = Math.Max(0, Array.IndexOf(ws, window));
            c.SelectionChanged += (_, _) =>
            {
                if (c.SelectedIndex < 0) return;
                window = ws[c.SelectedIndex];
                HostPrefs.Set("venueWindow", window);
                share.Start(window);
            };
            opts.Children.Add(Ui.Tip(c, L("Volume window")));
        }
        else if (pane < 2) opts.Children.Add(BookRight(b, Store.Shared.State.Base));
        Grid.SetRow(opts, 1);
        header.Children.Add(opts);
        Show();
    }

    void Pick(int v)
    {
        pane = v;
        if (v < 2) Store.Shared.Call("book", new() { ["trades"] = v == 1 });
        if (v == 3) quant.Update();
        Build();
    }

    void Show()
    {
        share.View.Visibility = pane == 2 ? Visibility.Visible : Visibility.Collapsed;
        quant.View.Visibility = pane == 3 ? Visibility.Visible : Visibility.Collapsed;
        if (pane == 2) share.Start(window); else share.Stop();
    }

    /// Source, tick and unit menus of the book header.
    public static StackPanel BookRight(BookCtl b, string baseAsset)
    {
        var p = Ui.H(6);
        static DropDownButton Menu(object content, MenuFlyout f) =>
            Ui.Small(new DropDownButton { Content = content, Flyout = f, Background = T1.Clear, BorderThickness = new Thickness(0) });
        if (!b.Trades)
        {
            var sf = new MenuFlyout();
            sf.Items.Add(Ui.Check(L("Aggregate USD"), b.Src == null, () => Book(new() { ["src"] = null })));
            sf.Items.Add(new MenuFlyoutSeparator());
            foreach (var v in b.Venues) sf.Items.Add(Ui.Check(v, b.Src == v, () => Book(new() { ["src"] = v })));
            object label = b.Src is string src ? Ui.H(4, T1.VenueIcon(src, 13), Ui.Text(src, 12)) : Ui.Text(L("All"), 12);
            p.Children.Add(Ui.Tip(Menu(label, sf), b.Src == null ? L("All venues, mid-aligned (view only)") : L("This venue's raw book (click a level to fill the price)")));
            if (b.Groups.Count > 0)
            {
                var gf = new MenuFlyout();
                for (int i = 0; i < b.Groups.Count; i++) { int g = i; gf.Items.Add(Ui.Check(b.Groups[i], i == b.Group, () => Book(new() { ["group"] = g }))); }
                p.Children.Add(Ui.Tip(Menu(Ui.Text(b.Groups[Math.Min(b.Group, b.Groups.Count - 1)], 12, num: true), gf), L("Price grouping")));
            }
        }
        var uf = new MenuFlyout();
        uf.Items.Add(Ui.Check(baseAsset, !b.Quote, () => Book(new() { ["quote"] = false })));
        uf.Items.Add(Ui.Check("USDT", b.Quote, () => Book(new() { ["quote"] = true })));
        p.Children.Add(Ui.Tip(Menu(Ui.Text(b.Quote ? "USDT" : baseAsset, 12), uf), L("Size unit")));
        return p;
    }

    static void Book(JsonObject a) => Store.Shared.Call("book", a);
}

sealed class VenueShareRow
{
    public string Ex { get; set; } = "";
    public string Color { get; set; } = "#888888";
    public double BidUsd { get; set; }
    public double AskUsd { get; set; }
    public double VolUsd { get; set; }
    public double BuyUsd { get; set; }
    public double SellUsd { get; set; }
    public string Quote { get; set; } = "";
    public double? Bid { get; set; }
    public double? Ask { get; set; }
    public double? PremBps { get; set; }
    public double? NormBps { get; set; }
    public bool Stale { get; set; }
}

/// Best same-quote cross: buy at Buy's ask, sell at Sell's bid; net is after both taker fees.
sealed class VenueArb
{
    public string Buy { get; set; } = "";
    public string Sell { get; set; } = "";
    public double Ask { get; set; }
    public double Bid { get; set; }
    public string Quote { get; set; } = "";
    public double GrossBps { get; set; }
    public double NetBps { get; set; }
}

sealed class VenueShareReply
{
    public List<VenueShareRow> Rows { get; set; } = new();
    public VenueArb? Arb { get; set; }
}

/// Each venue's share of resting depth (±1% of its mid) and of traded volume over the window,
/// plus the cross-venue price spread. Polls venue_share every second while shown.
sealed class VenueShare
{
    public readonly ScrollViewer View = new() { VerticalScrollBarVisibility = ScrollBarVisibility.Hidden };
    readonly StackPanel body = new() { Spacing = 14, Padding = new Thickness(10) };
    DispatcherQueueTimer? timer;
    int window = 60;

    public VenueShare() { View.Content = body; }

    public void Start(int w)
    {
        window = w;
        if (timer == null)
        {
            timer = DispatcherQueue.GetForCurrentThread().CreateTimer();
            timer.Interval = TimeSpan.FromSeconds(1);
            timer.Tick += (_, _) => Refresh();
        }
        timer.Start();
        Refresh();
    }

    public void Stop() => timer?.Stop();

    void Refresh()
    {
        if (Store.Shared.Query<VenueShareReply>("venue_share", new() { ["minutes"] = window }) is not { } r) return;
        var rows = r.Rows;
        body.Children.Clear();
        double depth = rows.Sum(x => x.BidUsd + x.AskUsd), vol = rows.Sum(x => x.VolUsd);
        var span = window >= 1440 ? "24h" : window >= 60 ? "1h" : $"{window}m";
        body.Children.Add(Prices(r));
        body.Children.Add(Section(L("Volume share"), $"{L("Traded")} {span} · ${Fmt.Big(vol)}", rows.OrderByDescending(x => x.VolUsd).ToList(), vol, x => x.VolUsd,
            x => { var b = x.BuyUsd + x.SellUsd; return b > 0 ? $"{L("buy")} {Fmt.F(x.BuyUsd / b * 100, 0)}%" : ""; }));
        body.Children.Add(Section(L("Book share"), $"{L("Depth ±1%")} · ${Fmt.Big(depth)}", rows.OrderByDescending(x => x.BidUsd + x.AskUsd).ToList(), depth, x => x.BidUsd + x.AskUsd,
            x => { var d = x.BidUsd + x.AskUsd; return d > 0 ? $"{L("bid")} {Fmt.F(x.BidUsd / d * 100, 0)}%" : ""; }));
    }

    /// Executable spread between venues, then each venue's premium over the composite next to its usual level.
    static StackPanel Prices(VenueShareReply r)
    {
        var p = Ui.V(7, Ui.Spread(Ui.Text(L("Price spread"), 13, semibold: true), Ui.Text(L("vs composite · usual"), 11, T1.Mu)));
        if (r.Arb is { NetBps: > 0 } a)
        {
            var t = Ui.Text(I18n.F(L("Buy %@ %@, sell %@ %@: %+.1f bp after fees"), a.Buy, Fmt.Px(a.Ask), a.Sell, Fmt.Px(a.Bid), a.NetBps), 11, T1.Up, num: true);
            t.TextWrapping = TextWrapping.Wrap;
            p.Children.Add(Ui.Tip(Ui.H(6, Ui.Text("⇄", 11, T1.Up), t),
                I18n.F(L("%@ books only, gross %+.1f bp, taker fees from Settings → Trading. Top of book only: size and latency decide whether it fills."), a.Quote, a.GrossBps)));
        }
        else
        {
            var msg = r.Arb is { } x ? I18n.F(L("No executable spread (best %+.1f bp after fees)"), x.NetBps) : L("No executable spread");
            p.Children.Add(Ui.Text(msg, 11, T1.Mu, num: true));
        }
        foreach (var v in r.Rows.Where(x => x.PremBps != null).OrderByDescending(x => x.PremBps))
        {
            double prem = v.PremBps ?? 0;
            // far from its own usual premium: highlight (the usual offset itself is structure, not signal)
            bool off = v.NormBps is double n && Math.Abs(prem - n) >= 5;
            var left = Ui.H(6, T1.VenueIcon(v.Ex, 13), Ui.Text(v.Ex, 11), Ui.Text(v.Quote, 10, T1.Dim));
            if (v.Stale) left.Children.Add(Ui.Tip(Ui.Text("⏱", 10, T1.Orange), L("No update for 10 s")));
            var norm = Ui.Text(v.NormBps is double nb ? Fmt.F(nb, 1, true) : "–", 10, T1.Dim, num: true);
            norm.Width = 40;
            norm.TextAlignment = TextAlignment.Right;
            var pr = Ui.Text(Fmt.F(prem, 1, true) + " bp", 11, off ? (prem > (v.NormBps ?? 0) ? T1.Up : T1.Down) : null, num: true, semibold: off);
            pr.Width = 62;
            pr.TextAlignment = TextAlignment.Right;
            p.Children.Add(Ui.Spread(left, Ui.H(0, norm, pr)));
        }
        return p;
    }

    static StackPanel Section(string title, string sub, List<VenueShareRow> list, double total, Func<VenueShareRow, double> value, Func<VenueShareRow, string> extra)
    {
        var p = Ui.V(7, Ui.Spread(Ui.Text(title, 13, semibold: true), Ui.Text(sub, 11, T1.Mu, num: true)));
        // one stacked bar of every venue, then a row per venue
        var bar = new Grid { Height = 6, CornerRadius = new CornerRadius(3) };
        int col = 0;
        foreach (var r in list)
        {
            var v = value(r);
            if (total <= 0 || v <= 0) continue;
            bar.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(v, GridUnitType.Star) });
            var rect = new Rectangle { Fill = T1.B(T1.Hex(r.Color)), Margin = new Thickness(0, 0, 1, 0) };
            Grid.SetColumn(rect, col++);
            bar.Children.Add(rect);
        }
        p.Children.Add(bar);
        foreach (var r in list)
        {
            var share = total > 0 ? value(r) / total : 0;
            var pct = Ui.Text(Fmt.F(share * 100, 1) + "%", 11, num: true);
            pct.Width = 46;
            pct.TextAlignment = TextAlignment.Right;
            var row = Ui.Spread(Ui.H(6, T1.VenueIcon(r.Ex, 13), Ui.Text(r.Ex, 11)), Ui.H(6, Ui.Text(extra(r), 10, T1.Dim, num: true), pct));
            p.Children.Add(Ui.Tip(row, "$" + Fmt.Big(value(r))));
        }
        return p;
    }
}

/// Statistical models for the current market: volatility forecast, book / flow pressure, carry.
sealed class QuantPanel
{
    public readonly ScrollViewer View = new() { VerticalScrollBarVisibility = ScrollBarVisibility.Hidden };
    readonly StackPanel body = new() { Spacing = 16, Padding = new Thickness(12) };

    public QuantPanel() { View.Content = body; }

    public void Update()
    {
        var q = Store.Shared.State.Quant;
        body.Children.Clear();

        var vol = new List<UIElement>();
        if (q.Vol is { } v)
        {
            vol.Add(Row(L("Expected move 1h"), $"±{Fmt.F(v.Sigma1h * 100, 2)}%", $"{Fmt.Px(v.Range1h.FirstOrDefault())} – {Fmt.Px(v.Range1h.LastOrDefault())}"));
            vol.Add(Row(L("Expected move 24h"), $"±{Fmt.F(v.Sigma24h * 100, 2)}%", $"{Fmt.Px(v.Range24h.FirstOrDefault())} – {Fmt.Px(v.Range24h.LastOrDefault())}"));
            var pc = v.Percentile;
            Color? tint = pc > 0.8 ? T1.Orange : pc < 0.2 ? T1.Accent : (Color?)null;
            vol.Add(Ui.V(4,
                Ui.Spread(Ui.Text(L("Volatility regime"), 13, T1.Mu), Ui.Text(pc < 0.2 ? L("Compressed") : pc > 0.8 ? L("Expanded") : L("Normal"), 13, tint)),
                new ProgressBar { Minimum = 0, Maximum = 1, Value = pc, Foreground = T1.B(pc > 0.8 ? T1.Orange : T1.Accent) },
                Ui.Text(I18n.F(L("%.0f%% of the last day's hours were calmer"), pc * 100), 10, T1.Dim)));
        }
        else vol.Add(Ui.Text(L("Needs an hour of data"), 13, T1.Dim));
        body.Children.Add(Group(L("Volatility model"), L("EWMA of 1m returns, 60 min half-life"), vol));

        var p = q.Pressure;
        Color? pc2 = p > 20 ? T1.Up : p < -20 ? T1.Down : (Color?)null;
        var big = Ui.Text(Fmt.F(p, 0, true), 18, p >= 0 ? T1.Up : T1.Down, num: true, semibold: true);
        body.Children.Add(Group(L("Pressure"), L("Book depth ±0.5% and 5 min aggressor flow; describes now, not a forecast"), new List<UIElement>
        {
            Ui.Spread(Ui.Text(p > 20 ? L("Buyers in control") : p < -20 ? L("Sellers in control") : L("Balanced"), 13, pc2), big),
            PressureBar(p),
            Row(L("Bids / asks ±0.5%"), $"${Fmt.Big(q.BookBidUsd)} / ${Fmt.Big(q.BookAskUsd)}", null),
        }));

        var carry = new List<UIElement>();
        foreach (var c in q.CarryRows.OrderByDescending(x => x.FundingApr ?? 0))
        {
            var apr = Ui.Text(c.FundingApr is double a ? Fmt.F(a, 1, true) + "%" : "–", 11, (c.FundingApr ?? 0) >= 0 ? T1.Up : T1.Down, num: true);
            var basis = Ui.Text(c.BasisBps is double bb ? Fmt.F(bb, 1, true) + " bp" : "–", 11, T1.Mu, num: true);
            apr.Width = basis.Width = 64;
            apr.TextAlignment = basis.TextAlignment = TextAlignment.Right;
            carry.Add(Ui.Spread(Ui.H(6, T1.VenueIcon(c.Ex, 13), Ui.Text(c.Ex, 11)), Ui.H(0, apr, basis)));
        }
        body.Children.Add(Group(L("Carry"), L("Predicted funding, annualized; basis: perp over spot"), carry));
    }

    /// -100..100 from the center line.
    static Grid PressureBar(double p)
    {
        double a = Math.Clamp(Math.Abs(p), 0, 100);
        var g = new Grid { Height = 6, CornerRadius = new CornerRadius(3), Background = T1.B(T1.Hl) };
        g.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) });
        g.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) });
        var half = new Grid();
        half.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(p >= 0 ? a : 100 - a, GridUnitType.Star) });
        half.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(p >= 0 ? 100 - a : a, GridUnitType.Star) });
        var fill = new Rectangle { Fill = T1.B(p >= 0 ? T1.Up : T1.Down) };
        Grid.SetColumn(fill, p >= 0 ? 0 : 1);
        half.Children.Add(fill);
        Grid.SetColumn(half, p >= 0 ? 1 : 0);
        g.Children.Add(half);
        var mid = new Rectangle { Width = 1, Fill = T1.B(T1.Mu), HorizontalAlignment = HorizontalAlignment.Left };
        Grid.SetColumn(mid, 1);
        g.Children.Add(mid);
        return g;
    }

    static StackPanel Group(string title, string note, List<UIElement> content)
    {
        var p = Ui.V(8, Ui.Text(title, 13, semibold: true));
        foreach (var c in content) p.Children.Add(c);
        var n = Ui.Text(note, 10, T1.Dim);
        n.TextWrapping = TextWrapping.Wrap;
        p.Children.Add(n);
        return p;
    }

    static Grid Row(string k, string v, string? sub)
    {
        var right = Ui.V(1, Ui.Text(v, 13, num: true));
        right.HorizontalAlignment = HorizontalAlignment.Right;
        foreach (var t in right.Children.OfType<TextBlock>()) t.HorizontalAlignment = HorizontalAlignment.Right;
        if (sub != null) { var s = Ui.Text(sub, 10, T1.Dim, num: true); s.HorizontalAlignment = HorizontalAlignment.Right; right.Children.Add(s); }
        return Ui.Spread(Ui.Text(k, 13, T1.Mu), right);
    }
}
