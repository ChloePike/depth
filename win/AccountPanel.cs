using System.Globalization;
using System.Text.Json.Nodes;
using Microsoft.UI.Dispatching;
using Microsoft.UI.Text;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Media;
using Windows.UI;
using static Depth.I18n;

namespace Depth;

/// Bottom strip (port of AccountPanel.swift): positions, orders, TP/SL, signals, history, order log
/// and assets across keyed venues, with position totals and account-level risk on the right.
/// Also opens the Transfer dialog (AccountPanel.OpenTransfer, used by the order panel too).
sealed class AccountPanel : Grid
{
    public static AccountPanel? Current { get; private set; }
    /// "Limit Close in Order Panel": the order panel takes venue, close mode, size and the mark as the price.
    public static event Action<PositionRow>? PrefillClose;
    public static void OpenTpSl(PositionRow p) { if (ModalDialog.Root.XamlRoot is { } r) _ = OrderTpSlSheet.Show(r, p); }

    enum Tab { Positions, Orders, TpSl, Signals, OrderHistory, TradeHistory, PositionHistory, Log, Assets }
    static readonly (Tab Id, string Title)[] Tabs =
    {
        (Tab.Positions, "Positions"), (Tab.Orders, "Open Orders"), (Tab.TpSl, "TP/SL"), (Tab.Signals, "Signals"),
        (Tab.OrderHistory, "Order History"), (Tab.TradeHistory, "Trade History"), (Tab.PositionHistory, "Position History"),
        (Tab.Log, "Order Log"), (Tab.Assets, "Assets"),
    };
    static bool IsHistory(Tab t) => t is Tab.OrderHistory or Tab.TradeHistory or Tab.PositionHistory;

    Tab tab = Tab.Positions;
    bool historyRequested;
    readonly StackPanel tabs = new() { Orientation = Orientation.Horizontal, Spacing = 18, Margin = new Thickness(0, 4, 0, 0) };
    readonly CheckBox hide = new() { Content = "", FontSize = 11, MinWidth = 0, MinHeight = 0, Padding = new Thickness(4, 0, 0, 0), VerticalAlignment = VerticalAlignment.Center };
    readonly AccountSummary summary = new();
    readonly Border content = new(), line = new();
    readonly DispatcherQueueTimer timer;
    readonly Dictionary<Tab, IAccountView> views = new();
    IAccountView? shown;
    bool noKeys;

    public AccountPanel()
    {
        Current = this;
        OrderPanel.TransferRequested -= OpenTransfer;
        OrderPanel.TransferRequested += OpenTransfer;
        RowDefinitions.Add(new RowDefinition { Height = GridLength.Auto });
        RowDefinitions.Add(new RowDefinition { Height = new GridLength(1) });
        RowDefinitions.Add(new RowDefinition { Height = new GridLength(1, GridUnitType.Star) });
        var top = new Grid { Padding = new Thickness(10, 5, 10, 5), ColumnSpacing = 12 };
        top.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) });
        top.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        top.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        // the tabs scroll instead of pushing the summary out
        top.Children.Add(new ScrollViewer
        {
            Content = tabs,
            HorizontalScrollMode = ScrollMode.Enabled, HorizontalScrollBarVisibility = ScrollBarVisibility.Hidden,
            VerticalScrollMode = ScrollMode.Disabled, VerticalScrollBarVisibility = ScrollBarVisibility.Disabled,
        });
        Grid.SetColumn(hide, 1);
        Grid.SetColumn(summary, 2);
        top.Children.Add(hide);
        top.Children.Add(summary);
        Children.Add(top);
        Grid.SetRow(line, 1);
        Children.Add(line);
        Grid.SetRow(content, 2);
        Children.Add(content);

        hide.IsChecked = HostPrefs.Get("hideOtherSymbols", false);
        hide.Click += (_, _) =>
        {
            HostPrefs.Set("hideOtherSymbols", hide.IsChecked == true);
            if (views.TryGetValue(Tab.Positions, out var v)) v.Update(true);
        };

        timer = DispatcherQueue.GetForCurrentThread().CreateTimer();
        timer.Interval = TimeSpan.FromSeconds(1);
        timer.Tick += (_, _) => shown?.Tick();
        Loaded += (_, _) => { Store.Shared.Changed += OnChanged; timer.Start(); OnChanged(); };
        Unloaded += (_, _) => { Store.Shared.Changed -= OnChanged; timer.Stop(); };
    }

    public static bool HideOthers => HostPrefs.Get("hideOtherSymbols", false);

    void OnChanged()
    {
        var st = Store.Shared;
        bool all = st.Dirty("lang", "prefs");
        if (all) line.Background = T1.B(T1.Line);
        if (st.Dirty("positions", "orders", "tpsl", "signals", "lang", "prefs")) BuildTabs();
        if (st.Dirty("positions", "balances", "lang", "prefs")) summary.Update();
        Show(all);
    }

    void BuildTabs()
    {
        var s = Store.Shared.State;
        tabs.Children.Clear();
        foreach (var (id, title) in Tabs)
        {
            string text = id switch
            {
                Tab.Positions => $"{L(title)} ({s.Positions.Count})",
                Tab.Orders => $"{L(title)} ({s.Orders.Count})",
                Tab.TpSl => $"{L(title)} ({s.Tpsl.Count})",
                Tab.Signals => $"{L(title)} ({s.Signals.Count})",
                _ => L(title),
            };
            bool on = id == tab;
            var label = Ui.Text(text, 13, on ? null : T1.Mu, semibold: on);
            label.TextTrimming = TextTrimming.None;
            var bar = new Border { Width = 18, Height = 3, CornerRadius = new CornerRadius(1.5), Background = on ? T1.B(T1.Accent) : T1.Clear, HorizontalAlignment = HorizontalAlignment.Center };
            var b = Ui.Flat(Ui.V(4, label, bar));
            b.Click += (_, _) => Select(id);
            tabs.Children.Add(b);
        }
        hide.Content = Ui.Text(L("Hide other symbols"), 11);
        hide.Visibility = tab == Tab.Positions ? Visibility.Visible : Visibility.Collapsed;
    }

    void Select(Tab t)
    {
        if (t == tab) return;
        tab = t;
        // history is fetched when first opened (never on a timer); Refresh re-fetches
        if (IsHistory(t) && !historyRequested)
        {
            historyRequested = true;
            var h = Store.Shared.State.History;
            if (h.UpdatedMs == null && !h.Loading) Store.Shared.Call("load_history");
        }
        BuildTabs();
        Show(true);
    }

    void Show(bool force)
    {
        var s = Store.Shared.State;
        bool nk = s.Keys.All(k => !k.Configured) && tab != Tab.Log && tab != Tab.Signals;
        if (nk)
        {
            if (!noKeys || force) { noKeys = true; shown = null; content.Child = NoKeys(); }
            return;
        }
        if (!views.TryGetValue(tab, out var v)) views[tab] = v = Make(tab);
        if (noKeys || shown != v) { noKeys = false; shown = v; content.Child = v.View; force = true; }
        v.Update(force);
    }

    static IAccountView Make(Tab t) => t switch
    {
        Tab.Positions => new PositionsView(),
        Tab.Orders => AccountTables.Orders(),
        Tab.TpSl => AccountTables.TpSl(),
        Tab.Signals => new SignalsView(),
        Tab.OrderHistory => AccountTables.History(AccountTables.OrderHistory()),
        Tab.TradeHistory => AccountTables.History(AccountTables.Fills()),
        Tab.PositionHistory => AccountTables.History(AccountTables.Closed()),
        Tab.Log => new LogView(),
        _ => new AssetsView(),
    };

    static FrameworkElement NoKeys()
    {
        var open = Ui.Small(new Button { Content = L("Open API Keys") });
        open.HorizontalAlignment = HorizontalAlignment.Center;
        open.Click += (_, _) => MainWindow.Current.OpenSettings("keys");
        var icon = Ui.Icon("", 28);
        icon.Foreground = T1.B(T1.Dim);
        var title = Ui.Text(L("No API keys"), 15, semibold: true);
        var desc = Ui.Text(L("Add an API key to see positions, orders and balances."), 12, T1.Mu);
        foreach (var e in new FrameworkElement[] { icon, title, desc }) e.HorizontalAlignment = HorizontalAlignment.Center;
        var p = Ui.V(8, icon, title, desc, open);
        p.HorizontalAlignment = HorizontalAlignment.Center;
        p.VerticalAlignment = VerticalAlignment.Center;
        return p;
    }

    /// One action from a row; a failure shows the engine's error.
    public static bool Act(string op, JsonObject args)
    {
        if (Store.Shared.Call(op, args)?["ok"]?.GetValue<bool>() == true) return true;
        ModalDialog.Alert(L("Action failed"), Store.Shared.LastError ?? L("Failed"));
        return false;
    }

    public static void RaisePrefillClose(PositionRow p) => PrefillClose?.Invoke(p);

    /// Transfer dialog on a keyed venue (the given one, else the trade venue, else the first keyed).
    public static void OpenTransfer(string? ex)
    {
        var s = Store.Shared.State;
        var keyed = s.Wallets.Select(w => w.Ex).ToList();
        var e = ex == null ? null : keyed.FirstOrDefault(k => string.Equals(k, ex, StringComparison.OrdinalIgnoreCase));
        e ??= keyed.Contains(s.Trade.Venue) ? s.Trade.Venue : keyed.FirstOrDefault();
        if (e == null) return;
        // wallets are loaded on demand; the dialog fills in its defaults once they arrive
        if (s.Wallets.FirstOrDefault(w => w.Ex == e) is { UpdatedMs: null, Loading: false }) Store.Shared.Call("load_wallets", new() { ["ex"] = e });
        _ = new TransferDialog(e).Show();
    }
}

/// A tab's content: View is mounted; Update(force) refreshes from the state (force after a switch or
/// a language/theme change, else only when its sections are dirty); Tick runs every second while shown.
interface IAccountView
{
    FrameworkElement View { get; }
    void Update(bool force);
    void Tick() { }
}

/// Position totals and account-level risk (what actually liquidates a unified account).
sealed class AccountSummary : StackPanel
{
    public AccountSummary()
    {
        Orientation = Orientation.Horizontal;
        Spacing = 14;
        VerticalAlignment = VerticalAlignment.Center;
    }

    public void Update()
    {
        var s = Store.Shared.State;
        var ps = s.Positions;
        Children.Clear();
        if (ps.Count > 0)
        {
            double upnl = ps.Sum(p => p.Upnl);
            Children.Add(Kv(L("uPnL"), Fmt.Signed(upnl), upnl >= 0 ? T1.Up : T1.Down));
            Children.Add(Kv(L("Margin"), Fmt.Usd(ps.Sum(p => p.Margin)), null));
            Children.Add(Kv(L("Value"), Fmt.Usd(ps.Sum(p => p.Qty * p.Mark), 0), null));
        }
        var risk = s.Balances.Where(b => b.UniMmr != null || b.MmRate != null).OrderBy(b => b.Ex, StringComparer.Ordinal).ToList();
        if (risk.Count > 0 && ps.Count > 0) Children.Add(Ui.Divider());
        foreach (var b in risk)
        {
            var row = Ui.H(5, T1.VenueIcon(b.Ex, 12));
            if (b.UniMmr is double m)
            {
                row.Children.Add(Ui.Text("uniMMR", 11, T1.Mu));
                row.Children.Add(Ui.Text(Fmt.Num(m, 2), 11.5, m > 1.5 ? T1.Up : m > 1.2 ? T1.Orange : T1.Down, num: true, semibold: true));
            }
            if (b.MmRate is double r)
            {
                row.Children.Add(Ui.Text(L("MM rate"), 11, T1.Mu));
                row.Children.Add(Ui.Text(Fmt.Num(r * 100, 1) + "%", 11.5, r < 0.5 ? T1.Up : r < 0.8 ? T1.Orange : T1.Down, num: true, semibold: true));
            }
            Children.Add(Ui.Tip(row, Tip(b)));
        }
    }

    static StackPanel Kv(string k, string v, Color? c) => Ui.H(5, Ui.Text(k, 11, T1.Mu), Ui.Text(v, 11.5, c, num: true, semibold: true));

    static string Tip(BalanceRow b)
    {
        var t = $"{b.Ex} · {L("Equity")} {Fmt.Usd(b.Equity)} · {L("Available")} {Fmt.Usd(b.Available)}\n";
        if (b.UniMmr != null) t += L("Portfolio Margin maintenance ratio: below 1.20 margin call, below 1.05 the whole account is liquidated. Higher is safer.");
        if (b.MmRate != null) t += L("Unified account maintenance margin rate: 100% triggers liquidation. Lower is safer.");
        return t;
    }
}

/// Detected signals, newest first: severity, direction, what happened and the numbers behind it.
sealed class SignalsView : IAccountView
{
    readonly ScrollViewer sv = new();
    readonly StackPanel list = new();
    public FrameworkElement View => sv;

    public SignalsView() { sv.Content = list; }

    public void Update(bool force)
    {
        if (!force && !Store.Shared.Dirty("signals")) return;
        var rows = Store.Shared.State.Signals;
        list.Children.Clear();
        if (rows.Count == 0)
        {
            list.Children.Add(AccountCells.Empty(L("No signals yet"),
                L("Volume spikes, liquidation cascades, OI / price divergence, crowded funding, spot vs perp flow, basis, whale trades and venue dislocations appear here as they happen.")));
            return;
        }
        foreach (var r in rows)
        {
            var g = new Grid { Padding = new Thickness(14, 8, 14, 8), ColumnSpacing = 10 };
            g.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
            g.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) });
            g.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
            var icon = Ui.Icon(r.Dir > 0 ? "" : r.Dir < 0 ? "" : "", 16);
            icon.Foreground = T1.B(r.Dir > 0 ? T1.Up : r.Dir < 0 ? T1.Down : T1.Orange);
            icon.VerticalAlignment = VerticalAlignment.Top;
            var detail = Ui.Text(r.Detail, 11, T1.Mu);
            detail.TextWrapping = TextWrapping.Wrap;
            var mid = Ui.V(2, Ui.H(6, Ui.Text(L(r.Title), 12, semibold: true),
                Ui.Text(new string('●', Math.Max(0, r.Severity)), 9, r.Severity >= 3 ? T1.Red : r.Severity == 2 ? T1.Orange : T1.Mu)), detail);
            var time = Ui.Text(Fmt.Time(r.Ts), 11, T1.Dim, num: true);
            time.VerticalAlignment = VerticalAlignment.Top;
            Grid.SetColumn(mid, 1);
            Grid.SetColumn(time, 2);
            g.Children.Add(icon);
            g.Children.Add(mid);
            g.Children.Add(time);
            list.Children.Add(g);
            list.Children.Add(AccountCells.Rule());
        }
    }
}

/// This session's order activity (engine log), newest first.
sealed class LogView : IAccountView
{
    readonly ScrollViewer sv = new();
    readonly StackPanel list = new() { Padding = new Thickness(0, 4, 0, 4) };
    public FrameworkElement View => sv;

    public LogView() { sv.Content = list; }

    public void Update(bool force)
    {
        if (!force && !Store.Shared.Dirty("log")) return;
        var rows = Store.Shared.State.Log;
        list.Children.Clear();
        if (rows.Count == 0) { list.Children.Add(AccountCells.Empty(L("No order activity this session"), null)); return; }
        foreach (var r in rows)
        {
            var icon = Ui.Icon(r.Ok ? "" : "", 10);
            icon.Foreground = T1.B(r.Ok ? T1.Dim : T1.Down);
            var msg = Ui.Text(r.Msg, 11, r.Ok ? null : T1.Down, num: true);
            msg.IsTextSelectionEnabled = true;
            msg.TextTrimming = TextTrimming.None;
            msg.TextWrapping = TextWrapping.Wrap;
            var g = new Grid { Padding = new Thickness(12, 3, 12, 3), ColumnSpacing = 10 };
            g.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
            g.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
            g.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) });
            var time = Ui.Text(Fmt.Time(r.Ts), 11, T1.Dim, num: true);
            Grid.SetColumn(time, 1);
            Grid.SetColumn(msg, 2);
            g.Children.Add(icon);
            g.Children.Add(time);
            g.Children.Add(msg);
            list.Children.Add(g);
        }
    }
}

/// ContentDialog on the main window in its effective theme, through Dialogs (OrderConfirm.cs): one
/// dialog at a time, a second Show returns null instead of throwing.
static class ModalDialog
{
    public static FrameworkElement Root => (FrameworkElement)MainWindow.Current.Content;
    public static ElementTheme Theme => Root.ActualTheme;

    public static Task<ContentDialogResult?> Show(ContentDialog d)
    {
        if (Root.XamlRoot == null) return Task.FromResult<ContentDialogResult?>(null);
        d.XamlRoot = Root.XamlRoot;
        d.RequestedTheme = Root.ActualTheme;
        return Dialogs.Show(d);
    }

    /// Cancel is the default button: Enter never confirms a money action by accident.
    public static async Task<bool> Confirm(string title, string message, string action)
    {
        var d = new ContentDialog
        {
            Title = title, PrimaryButtonText = action, CloseButtonText = L("Cancel"), DefaultButton = ContentDialogButton.Close,
            Content = new TextBlock { Text = message, TextWrapping = TextWrapping.Wrap, IsTextSelectionEnabled = true },
        };
        return await Show(d) == ContentDialogResult.Primary;
    }

    public static void Alert(string title, string message) =>
        _ = Show(new ContentDialog { Title = title, CloseButtonText = L("OK"), Content = new TextBlock { Text = message, TextWrapping = TextWrapping.Wrap, IsTextSelectionEnabled = true } });
}

/// Cells shared by the account tables.
static class AccountCells
{
    public static readonly CultureInfo Inv = CultureInfo.InvariantCulture;

    /// User-typed number: grouping commas dropped, invariant decimal point, finite only.
    public static double? Parse(string s) =>
        double.TryParse(s.Replace(",", "").Trim(), NumberStyles.Float, Inv, out var v) && double.IsFinite(v) ? v : null;

    public static TextBlock Num(string s, Color? c = null, double size = 12)
    {
        var t = Ui.Text(s, size, c, num: true);
        t.HorizontalAlignment = HorizontalAlignment.Right;
        t.TextAlignment = TextAlignment.Right;
        return t;
    }

    public static TextBlock Txt(string s, Color? c = null, double size = 12) => Ui.Text(s, size, c);

    public static Color PnlColor(double v) => v >= 0 ? T1.Up : T1.Down;

    public static TextBlock Side(string side, string? pos = null)
    {
        bool buy = side is "buy" or "long";
        var b = L(side == "buy" ? "Buy" : side == "sell" ? "Sell" : side == "long" ? "Long" : "Short");
        var p = pos == null ? "" : " · " + L(pos == "long" ? "Long" : "Short");
        var t = Ui.Text(b + p, 12, buy ? T1.Up : T1.Down);
        t.FontWeight = FontWeights.Medium;
        return t;
    }

    public static Border Badge(string text, Color c)
    {
        var t = Ui.Text(text, 10, c, semibold: true);
        return new Border { Child = t, Background = T1.B(c, 0.15), CornerRadius = new CornerRadius(3), Padding = new Thickness(5, 1, 5, 2), VerticalAlignment = VerticalAlignment.Center };
    }

    public static TextBlock Time(long ts) => Ui.Text(Fmt.Time(ts), 11.5, T1.Mu, num: true);

    /// Coin logo with the venue's logo on its corner, the symbol and an optional badge; a click
    /// jumps the whole terminal to that pair's perpetual market.
    public static FrameworkElement Symbol(string ex, string symbol, FrameworkElement? badge = null)
    {
        var bas = symbol.EndsWith("USDT") ? symbol[..^4] : symbol;
        var icon = new Grid { Width = 16, Height = 16, VerticalAlignment = VerticalAlignment.Center };
        icon.Children.Add(T1.CoinIcon(bas, 16));
        var v = T1.VenueIcon(ex, 9);
        v.HorizontalAlignment = HorizontalAlignment.Right;
        v.VerticalAlignment = VerticalAlignment.Bottom;
        v.RenderTransform = new TranslateTransform { X = 3, Y = 2 };
        icon.Children.Add(v);
        var name = Ui.Text(symbol, 12);
        name.FontWeight = FontWeights.Medium;
        var row = Ui.H(7, icon, name);
        if (badge != null) row.Children.Add(badge);
        var b = Ui.Flat(row);
        b.Padding = new Thickness(0);
        b.HorizontalAlignment = HorizontalAlignment.Left;
        b.Click += (_, _) =>
        {
            Pairs.Open(bas);
            if (Store.Shared.State.Mode != "Perp") Store.Shared.Call("set_mode", new() { ["mode"] = "Perp" });
        };
        return Ui.Tip(b, $"{ex} · {L("Perp")} — {L("click to open this market")}");
    }

    public static Border Rule() => new() { Height = 1, Background = T1.B(T1.Line, 0.5) };

    public static FrameworkElement Empty(string title, string? detail)
    {
        var p = Ui.V(6, Ui.Text(title, 13, T1.Dim));
        if (detail != null)
        {
            var d = Ui.Text(detail, 11, T1.Dim);
            d.TextWrapping = TextWrapping.Wrap;
            d.TextAlignment = TextAlignment.Center;
            d.MaxWidth = 520;
            p.Children.Add(d);
        }
        foreach (var c in p.Children.OfType<FrameworkElement>()) c.HorizontalAlignment = HorizontalAlignment.Center;
        p.HorizontalAlignment = HorizontalAlignment.Center;
        p.Margin = new Thickness(16, 28, 16, 16);
        return p;
    }

    public static void Copy(string s)
    {
        var dp = new Windows.ApplicationModel.DataTransfer.DataPackage();
        dp.SetText(s);
        Windows.ApplicationModel.DataTransfer.Clipboard.SetContent(dp);
    }

    public static MenuFlyoutItem Item(string text, Action a)
    {
        var it = new MenuFlyoutItem { Text = text };
        it.Click += (_, _) => a();
        return it;
    }
}
