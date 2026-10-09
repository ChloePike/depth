using System.Globalization;
using System.Text.Json.Nodes;
using Microsoft.UI.Dispatching;
using Microsoft.UI.Text;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Windows.UI;
using Ellipse = Microsoft.UI.Xaml.Shapes.Ellipse;
using static Depth.I18n;

namespace Depth;

// Assets tab (every account per keyed venue, loaded on demand) and the Transfer dialog with the
// auto top-up rule (port of AccountAssets.swift). Transfers never leave the venue: no withdrawals.

static class AccountWallet
{
    /// accounts that margin perps (mirror of trade::MARGIN_IDS)
    public static readonly string[] MarginIds = { "UNIFIED", "PM", "USDM" };

    public static string Name(string id) => id switch
    {
        "UNIFIED" => L("Unified Trading"),
        "FUND" or "FUNDING" => L("Funding"),
        "EARN" => L("Earn (Flexible)"),
        "PM" => L("Portfolio Margin"),
        "USDM" => L("USDS-M Futures"),
        "SPOT" => L("Spot"),
        _ => id,
    };

    /// Where `from` can transfer to (mirror of trade::destinations).
    public static string[] Destinations(string ex, string from, bool pm)
    {
        var margin = pm ? "PM" : "USDM";
        return (ex, from) switch
        {
            ("Bybit", "UNIFIED") => new[] { "FUND" },
            ("Bybit", "FUND") => new[] { "UNIFIED" },
            ("Bybit", "EARN") => new[] { "UNIFIED", "FUND" },
            ("Binance", "SPOT") => new[] { margin, "FUNDING" },
            ("Binance", "FUNDING") => new[] { margin, "SPOT" },
            ("Binance", "PM") or ("Binance", "USDM") => new[] { "SPOT", "FUNDING" },
            ("Binance", "EARN") => new[] { margin, "SPOT", "FUNDING" },
            _ => Array.Empty<string>(),
        };
    }

    /// Short reason for an account that could not be read (raw text goes in the tooltip).
    public static string NoteHint(string n) =>
        new[] { "10005", "Permission", "-2015", "permission" }.Any(n.Contains) ? L("API key lacks read permission") : L("Read failed");

    public static double MovableUsd(WalletAccount w) => w.Coins.Where(c => c.Free > 0).Sum(c => c.Qty > 0 ? c.Usd * c.Free / c.Qty : 0);

    /// Transfer amount: at most the free balance, floored to 8 decimals (exact decimal arithmetic,
    /// never rounds up past what is there).
    public static double Floor8(double amount, double free)
    {
        var x = Math.Min(amount, free);
        if (!(x > 0)) return 0;
        var d = decimal.Parse(x.ToString("R", CultureInfo.InvariantCulture), NumberStyles.Float, CultureInfo.InvariantCulture);
        return (double)(Math.Floor(d * 100_000_000m) / 100_000_000m);
    }
}

/// One card per keyed venue: total, each account's USD value and top coins, read errors.
sealed class AssetsView : IAccountView
{
    readonly ScrollViewer sv = new();
    readonly Grid grid = new() { Padding = new Thickness(10), ColumnSpacing = 10, RowSpacing = 10 };
    readonly HashSet<string> requested = new();
    readonly List<(TextBlock T, long At)> ages = new();
    readonly List<FrameworkElement> cards = new();
    int columns;
    public FrameworkElement View => sv;

    public AssetsView()
    {
        sv.Content = grid;
        // adaptive columns, at least 420 wide each
        sv.SizeChanged += (_, e) =>
        {
            int n = Math.Max(1, (int)((e.NewSize.Width - 20) / 430));
            if (n != columns) { columns = n; Layout(); }
        };
    }

    public void Update(bool force)
    {
        var st = Store.Shared;
        if (!force && !st.Dirty("wallets")) return;
        var ws = st.State.Wallets;
        cards.Clear();
        ages.Clear();
        foreach (var w in ws) cards.Add(Card(w));
        Layout();
        // each venue's wallets are fetched once when the tab is first shown (Refresh re-fetches);
        // deferred: this runs inside Store.Changed, which must not call back into the engine
        var missing = ws.Where(w => w.UpdatedMs == null && !w.Loading && requested.Add(w.Ex)).Select(w => w.Ex).ToList();
        if (missing.Count > 0)
            DispatcherQueue.GetForCurrentThread().TryEnqueue(() => { foreach (var e in missing) Store.Shared.Call("load_wallets", new() { ["ex"] = e }); });
    }

    void Layout()
    {
        grid.Children.Clear();
        grid.ColumnDefinitions.Clear();
        grid.RowDefinitions.Clear();
        int n = Math.Max(1, columns);
        for (int i = 0; i < n; i++) grid.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) });
        for (int i = 0; i < cards.Count; i++)
        {
            if (i % n == 0) grid.RowDefinitions.Add(new RowDefinition { Height = GridLength.Auto });
            Grid.SetColumn(cards[i], i % n);
            Grid.SetRow(cards[i], i / n);
            grid.Children.Add(cards[i]);
        }
    }

    public void Tick()
    {
        long now = DateTimeOffset.UtcNow.ToUnixTimeMilliseconds();
        foreach (var (t, at) in ages) t.Text = $"{Math.Max(0, (now - at) / 1000)}s";
    }

    FrameworkElement Card(VenueWallets w)
    {
        var head = Ui.H(8, T1.VenueIcon(w.Ex, 16), Ui.Text(w.Ex, 13, semibold: true));
        if (w.UpdatedMs != null)
            head.Children.Add(Ui.Text($"≈ {Fmt.Usd(Math.Max(0, w.Accounts.Sum(a => a.Usd)))} USD", 12, num: true));
        if (w.Auto is { Enabled: true } a)
            head.Children.Add(AccountCells.Badge($"{L("Auto top-up")} < {Fmt.Usd(a.Min, 0)} → {Fmt.Usd(a.Target, 0)}", T1.Up));
        var right = Ui.H(6);
        if (w.Loading) right.Children.Add(new ProgressRing { IsActive = true, Width = 12, Height = 12, MinWidth = 0, MinHeight = 0 });
        else if (w.UpdatedMs is long at)
        {
            var age = Ui.Text("", 10.5, T1.Dim, num: true);
            ages.Add((age, at));
            right.Children.Add(age);
            var refresh = Ui.Flat(Ui.Icon("", 12));
            refresh.Click += (_, _) => Store.Shared.Call("load_wallets", new() { ["ex"] = w.Ex });
            right.Children.Add(Ui.Tip(refresh, L("Refresh")));
        }
        var transfer = Ui.Small(new Button { Content = Ui.H(5, Ui.Icon("", 11), Ui.Text(L("Transfer"), 12)) });
        transfer.Click += (_, _) => AccountPanel.OpenTransfer(w.Ex);
        right.Children.Add(transfer);

        var body = Ui.V(6, Ui.Spread(head, right), new Border { Height = 1, Background = T1.B(T1.Line) });
        if (w.UpdatedMs == null)
        {
            var t = Ui.Text(w.Loading ? L("Reading accounts…") : L("Not loaded"), 11.5, T1.Dim);
            t.Margin = new Thickness(0, 6, 0, 6);
            body.Children.Add(t);
        }
        foreach (var acc in w.Accounts) body.Children.Add(AccountRow(acc));
        foreach (var n in w.Notes)
        {
            var id = n.Split(':')[0];
            var warn = Ui.Icon("", 11);
            warn.Foreground = T1.B(T1.Orange);
            body.Children.Add(Ui.Tip(Ui.H(5, warn, Ui.Text($"{AccountWallet.Name(id)}: {AccountWallet.NoteHint(n)}", 11, T1.Orange)), n));
        }
        return new Border
        {
            Child = body, Padding = new Thickness(10), CornerRadius = new CornerRadius(8),
            Background = T1.B(T1.Panel), BorderBrush = T1.B(T1.Line), BorderThickness = new Thickness(1), VerticalAlignment = VerticalAlignment.Top,
        };
    }

    static FrameworkElement AccountRow(WalletAccount a)
    {
        bool margin = AccountWallet.MarginIds.Contains(a.Id);
        // dust (under a cent) is not worth a slot
        var coins = a.Coins.Where(c => c.Usd >= 0.01 || (c.Usd == 0 && c.Qty >= 1e-4)).OrderByDescending(c => c.Usd).ToList();
        var g = new Grid { Height = 20, ColumnSpacing = 10 };
        g.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(130) });
        g.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(90) });
        g.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) });
        var name = Ui.Text(AccountWallet.Name(a.Id), 11.5, margin ? null : T1.Mu);
        if (margin) name.FontWeight = FontWeights.Medium;
        g.Children.Add(Ui.H(5, new Ellipse { Width = 5, Height = 5, Fill = T1.B(margin ? T1.Accent : T1.Dim), VerticalAlignment = VerticalAlignment.Center }, name));
        var usd = AccountCells.Num(Fmt.Usd(Math.Max(0, a.Usd)), null, 11.5);
        Grid.SetColumn(usd, 1);
        g.Children.Add(usd);
        var list = Ui.H(10);
        foreach (var c in coins.Take(3))
            list.Children.Add(Ui.Tip(Ui.H(3, T1.CoinIcon(c.Coin, 12), Ui.Text(Fmt.Qty(c.Qty), 10.5, T1.Mu, num: true), Ui.Text(c.Coin, 10.5, T1.Dim)),
                $"{c.Coin} {Fmt.Qty(c.Qty)} · {L("free")} {Fmt.Qty(c.Free)} · ≈{Fmt.Usd(c.Usd)} USD"));
        if (coins.Count > 3) list.Children.Add(Ui.Text($"+{coins.Count - 3}", 10.5, T1.Dim, num: true));
        Grid.SetColumn(list, 2);
        g.Children.Add(list);
        return g;
    }
}

/// Move funds between one venue's own accounts, plus the auto top-up rule. Confirm sends one
/// "transfer"; the dialog stays open with the engine's error if it fails.
sealed class TransferDialog
{
    readonly ContentDialog dlg = new();
    readonly ComboBox exBox = Box(), fromBox = Box(), toBox = Box(), coinBox = Box();
    readonly TextBox amount = Ui.Tabular(new TextBox { Width = 150, PlaceholderText = "0.00", TextAlignment = TextAlignment.Right });
    readonly TextBlock reading = Ui.Text("", 11.5, T1.Mu);
    readonly StackPanel notes = new() { Spacing = 4 }, autoNotes = new() { Spacing = 4 };
    readonly ToggleSwitch autoOn = new() { OnContent = "", OffContent = "", MinWidth = 0 };
    readonly TextBox autoMin = UsdBox(), autoTarget = UsdBox();
    readonly Button saveRule = new();
    string ex, from = "", to = "", coin = "USDT";
    AutoRule auto = new();
    bool autoLoaded, building;
    string? error;
    List<string> fromIds = new(), toIds = new(), coinIds = new();

    static ComboBox Box() => new() { MinWidth = 240, FontSize = 12 };
    static TextBox UsdBox() => Ui.Tabular(new TextBox { Width = 120, TextAlignment = TextAlignment.Right });

    public TransferDialog(string ex)
    {
        this.ex = ex;
        dlg.Title = L("Transfer");
        dlg.PrimaryButtonText = L("Confirm Transfer");
        dlg.CloseButtonText = L("Cancel");
        dlg.DefaultButton = ContentDialogButton.Close;
        dlg.PrimaryButtonClick += (_, e) => { if (!Confirm()) e.Cancel = true; };

        exBox.SelectionChanged += (_, _) =>
        {
            if (building || exBox.SelectedItem is not string e || e == this.ex) return;
            this.ex = e;
            from = ""; to = ""; amount.Text = ""; autoLoaded = false;
            var w = Wallet;
            PickDefaults();
            LoadAuto();
            if (w is { UpdatedMs: null, Loading: false }) Store.Shared.Call("load_wallets", new() { ["ex"] = e });
            Sync();
        };
        fromBox.SelectionChanged += (_, _) =>
        {
            if (building || fromBox.SelectedIndex < 0 || fromBox.SelectedIndex >= fromIds.Count) return;
            from = fromIds[fromBox.SelectedIndex];
            var d = AccountWallet.Destinations(this.ex, from, Store.Shared.State.BinancePm);
            if (!d.Contains(to)) to = d.FirstOrDefault() ?? "";
            var cs = Coins(from);
            if (!cs.Any(c => c.Coin == coin)) coin = cs.FirstOrDefault()?.Coin ?? "";
            Sync();
        };
        toBox.SelectionChanged += (_, _) => { if (!building && toBox.SelectedIndex >= 0 && toBox.SelectedIndex < toIds.Count) { to = toIds[toBox.SelectedIndex]; Sync(); } };
        coinBox.SelectionChanged += (_, _) => { if (!building && coinBox.SelectedIndex >= 0 && coinBox.SelectedIndex < coinIds.Count) { coin = coinIds[coinBox.SelectedIndex]; Sync(); } };
        amount.TextChanged += (_, _) => Validate();
        var max = Ui.Small(new Button { Content = L("Max") });
        max.Click += (_, _) => amount.Text = Free.ToString("R", CultureInfo.InvariantCulture);

        autoOn.Toggled += (_, _) => { auto.Enabled = autoOn.IsOn; ValidateAuto(); };
        autoMin.TextChanged += (_, _) => { auto.Min = Usd(autoMin.Text); ValidateAuto(); };
        autoTarget.TextChanged += (_, _) => { auto.Target = Usd(autoTarget.Text); ValidateAuto(); };
        saveRule.Content = L("Save Rule");
        saveRule.HorizontalAlignment = HorizontalAlignment.Right;
        saveRule.Click += (_, _) =>
        {
            var ok = Store.Shared.Call("set_auto", new() { ["ex"] = this.ex, ["enabled"] = auto.Enabled, ["min"] = auto.Min, ["target"] = auto.Target })?["ok"]?.GetValue<bool>() == true;
            error = ok ? null : Store.Shared.LastError;
            Validate();
            ValidateAuto();
        };

        var form = Ui.V(10,
            Row(L("Exchange"), exBox), reading,
            Row(L("From"), fromBox), Row(L("To"), toBox), Row(L("Coin"), coinBox),
            Row(L("Amount"), Ui.H(8, amount, max)), notes,
            new Border { Height = 1, Background = T1.B(T1.Line), Margin = new Thickness(0, 6, 0, 6) },
            Ui.Text(L("Auto top-up"), 12, semibold: true),
            Row(L("Top up margin automatically"), autoOn),
            Row(L("When available is below"), Ui.H(6, autoMin, Ui.Text("USD", 12, T1.Mu))),
            Row(L("Top up to"), Ui.H(6, autoTarget, Ui.Text("USD", 12, T1.Mu))),
            saveRule, autoNotes);
        form.Width = 440;
        dlg.Content = new ScrollViewer { Content = form, MaxHeight = 560 };
    }

    static Grid Row(string label, FrameworkElement control) => Ui.Spread(Ui.Text(label, 12), control);
    static double Usd(string s) => AccountCells.Parse(s) is double v ? Math.Round(v, 2) : 0;

    VenueWallets? Wallet => Store.Shared.State.Wallets.FirstOrDefault(w => w.Ex == ex);
    List<WalletAccount> Accounts => Wallet?.Accounts ?? new();
    List<WalletCoin> Coins(string acc) => Accounts.FirstOrDefault(a => a.Id == acc)?.Coins.Where(c => c.Free > 0).ToList() ?? new();
    WalletCoin? Coin => Coins(from).FirstOrDefault(c => c.Coin == coin);
    double Free => Coin?.Free ?? 0;
    /// Swift Double(): no grouping, invariant point
    double Amt => double.TryParse(amount.Text.Trim(), NumberStyles.Float, CultureInfo.InvariantCulture, out var v) && double.IsFinite(v) ? v : 0;

    public async Task Show()
    {
        PickDefaults();
        LoadAuto();
        Sync();
        Store.Shared.Changed += OnChanged;
        try { await ModalDialog.Show(dlg); }
        finally { Store.Shared.Changed -= OnChanged; }
    }

    void OnChanged()
    {
        if (!Store.Shared.Dirty("wallets", "transferring", "binance_pm", "lang")) return;
        PickDefaults();
        LoadAuto();
        Sync();
    }

    /// Default: from the non-margin account with the most that can move, into the margin account.
    void PickDefaults()
    {
        var ws = Accounts;
        if (from != "" || ws.Count == 0) return;
        var margin = ws.FirstOrDefault(a => AccountWallet.MarginIds.Contains(a.Id))?.Id ?? "";
        var best = ws.Where(a => a.Id != margin).MaxBy(AccountWallet.MovableUsd);
        from = best != null && AccountWallet.MovableUsd(best) > 0 ? best.Id : margin;
        var dests = AccountWallet.Destinations(ex, from, Store.Shared.State.BinancePm);
        to = dests.Contains(margin) ? margin : dests.FirstOrDefault() ?? "";
        var cs = Coins(from);
        if (!cs.Any(c => c.Coin == coin)) coin = cs.FirstOrDefault()?.Coin ?? "";
    }

    void LoadAuto()
    {
        if (autoLoaded) return;
        autoLoaded = true;
        var a = Wallet?.Auto;
        auto = new AutoRule { Enabled = a?.Enabled ?? false, Min = a?.Min ?? 0, Target = a?.Target ?? 0 };
        autoOn.IsOn = auto.Enabled;
        autoMin.Text = auto.Min.ToString("0.##", CultureInfo.InvariantCulture);
        autoTarget.Text = auto.Target.ToString("0.##", CultureInfo.InvariantCulture);
    }

    /// Pickers from the current wallets and selection.
    void Sync()
    {
        building = true;
        var s = Store.Shared.State;
        exBox.Items.Clear();
        foreach (var w in s.Wallets) exBox.Items.Add(w.Ex);
        exBox.SelectedItem = ex;
        reading.Text = L("Reading accounts…");
        reading.Visibility = Wallet?.UpdatedMs == null ? Visibility.Visible : Visibility.Collapsed;

        var sources = Accounts.Where(a => a.Coins.Any(c => c.Free > 0)).ToList();
        fromIds = sources.Select(a => a.Id).ToList();
        fromBox.Items.Clear();
        foreach (var a in sources) fromBox.Items.Add($"{AccountWallet.Name(a.Id)}   {Fmt.Usd(a.Usd)} USD");
        if (!fromIds.Contains(from)) { fromIds.Add(from); fromBox.Items.Add(from == "" ? "—" : AccountWallet.Name(from)); }
        fromBox.SelectedIndex = fromIds.IndexOf(from);

        toIds = AccountWallet.Destinations(ex, from, s.BinancePm).ToList();
        toBox.Items.Clear();
        foreach (var d in toIds) toBox.Items.Add(AccountWallet.Name(d));
        if (!toIds.Contains(to)) { toIds.Add(to); toBox.Items.Add("—"); }
        toBox.SelectedIndex = toIds.IndexOf(to);

        var coins = Coins(from);
        coinIds = coins.Select(c => c.Coin).ToList();
        coinBox.Items.Clear();
        foreach (var c in coins) coinBox.Items.Add($"{c.Coin}   {Fmt.Qty(c.Free)}");
        if (!coinIds.Contains(coin)) { coinIds.Add(coin); coinBox.Items.Add(coin == "" ? "—" : coin); }
        coinBox.SelectedIndex = coinIds.IndexOf(coin);
        building = false;
        Validate();
        ValidateAuto();
    }

    void Note(StackPanel p, string t, Color c)
    {
        var b = Ui.Text(t, 11, c);
        b.TextWrapping = TextWrapping.Wrap;
        b.TextTrimming = TextTrimming.None;
        p.Children.Add(b);
    }

    void Validate()
    {
        double free = Free, amt = Amt;
        var c = Coin;
        bool valid = amt > 0 && amt <= free * (1 + 1e-9) && to != "" && c != null && (from != "EARN" || c.Product != null);
        dlg.IsPrimaryButtonEnabled = valid && !Store.Shared.State.Transferring;
        notes.Children.Clear();
        Note(notes, $"{L("Available")} {Fmt.Qty(free)} {coin}", T1.Dim);
        if (amt > free * (1 + 1e-9)) Note(notes, L("Amount exceeds the available balance"), T1.Down);
        // multi-step and slow routes are spelled out before confirming
        if (from == "EARN") Note(notes, L("Redeems from Flexible Earn, usually instant; a redemption cannot be undone."), T1.Orange);
        if (ex == "Binance" && new[] { ("FUNDING", "PM"), ("PM", "FUNDING"), ("EARN", "PM"), ("EARN", "USDM") }.Contains((from, to)))
            Note(notes, L("No direct route: moves to Spot first, then into the target account (two steps)."), T1.Orange);
        if (from == "EARN" && c != null && c.Product == null) Note(notes, L("On-chain Earn must be redeemed on the exchange website (takes days)."), T1.Down);
        if (error != null) Note(notes, error, T1.Down);
    }

    void ValidateAuto()
    {
        var cur = Wallet?.Auto ?? new AutoRule();
        bool changed = cur.Enabled != auto.Enabled || cur.Min != auto.Min || cur.Target != auto.Target;
        bool bad = auto.Enabled && auto.Target <= auto.Min;
        saveRule.IsEnabled = changed && !bad;
        autoNotes.Children.Clear();
        Note(autoNotes, $"{L("At most once a minute per venue; every transfer is written to the order log. Sources in order:")} {(ex == "Bybit" ? L("Funding → Flexible Earn") : L("Spot → Funding → Flexible Earn"))}", T1.Dim);
        if (bad) Note(autoNotes, L("Target must be above the trigger"), T1.Down);
    }

    bool Confirm()
    {
        var c = Coin;
        var amt = c == null ? 0 : AccountWallet.Floor8(Amt, c.Free);
        if (c == null || amt <= 0) return false;
        var args = new JsonObject { ["ex"] = ex, ["from"] = from, ["to"] = to, ["coin"] = coin, ["amount"] = amt };
        if (c.Product != null) args["product"] = c.Product;
        if (Store.Shared.Call("transfer", args)?["ok"]?.GetValue<bool>() == true) return true;
        error = Store.Shared.LastError;
        Validate();
        return false;
    }
}
