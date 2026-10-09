using System.Text.Json.Nodes;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Windows.UI;
using static Depth.I18n;

namespace Depth;

/// TP/SL for an existing position (port of OrderTpSlSheet in OrderTpSl.swift): whole position (one
/// set_tpsl) or staged levels (one set_tpsl with partial_qty per level). Lists the position's live
/// TP/SL with cancel buttons. Open with OrderTpSlSheet.Show(xamlRoot, position).
sealed class OrderTpSlSheet
{
    sealed class Level
    {
        public bool Tp;
        public string Price = "", Amount = "";
        /// amount is % of the position (else base units)
        public bool Pct = true;
    }

    readonly PositionRow p;
    bool partial, trigLast;
    string tp = "", sl = "";
    readonly List<Level> levels = new() { new() { Tp = true, Amount = "50" }, new() { Tp = false, Amount = "100" } };
    readonly StackPanel modeHost = Ui.V(10), activeHost = Ui.V(10), errHost = Ui.V(0);
    readonly TextBlock sideText = Ui.Text("", 14, semibold: true), sizeText = Ui.Text("", 13, num: true), entryText = Ui.Text("", 13, num: true), liqText = Ui.Text("", 13, T1.Orange, num: true);
    readonly List<Action> pnlUpd = new();
    ContentDialog? dlg;
    string activeKey = "-";

    OrderTpSlSheet(PositionRow p) { this.p = p; }

    public static Task Show(XamlRoot root, PositionRow p) => new OrderTpSlSheet(p).Run(root);

    Store St => Store.Shared;
    /// live row (mark, size move while the sheet is open)
    PositionRow Pos => St.State.Positions.FirstOrDefault(x => x.Id == p.Id) ?? p;
    List<TpSlRow> Existing => St.State.Tpsl.Where(r => r.Ex == p.Ex && r.Symbol == p.Symbol && r.Pos == p.Side).ToList();
    double? Step => p.Symbol == St.State.Trade.Symbol ? St.State.Trade.Step : null;
    double Sign => Pos.IsLong ? 1 : -1;

    async Task Run(XamlRoot root)
    {
        var body = Ui.V(16);
        body.Width = 460;
        body.Children.Add(Ui.Spread(Ui.H(8, T1.VenueIcon(p.Ex, 16), Ui.Text(p.Symbol, 15, semibold: true), Ui.Text(L("TP/SL"), 13, T1.Mu)), sideText));
        body.Children.Add(Ui.V(6,
            Ui.Spread(Ui.Text(L("Size"), 13, T1.Mu), sizeText),
            Ui.Spread(Ui.Text(L("Entry / Mark"), 13, T1.Mu), entryText),
            Ui.Spread(Ui.Text(L("Liq. price"), 13, T1.Mu), liqText)));
        var mode = Ui.Segmented(new[] { (false, L("Entire position")), (true, L("Partial (staged)")) }, partial, v => { partial = v; BuildMode(); }, stretch: true);
        mode.HorizontalAlignment = HorizontalAlignment.Stretch;
        var trig = Ui.Segmented(new[] { (false, L("Mark")), (true, L("Last")) }, trigLast, v => trigLast = v);
        body.Children.Add(Ui.V(10, mode, Ui.Spread(Ui.Text(L("Trigger"), 13, T1.Mu), trig)));
        body.Children.Add(modeHost);
        body.Children.Add(activeHost);
        body.Children.Add(errHost);
        BuildMode();
        Refresh();

        var d = dlg = Dialogs.New(root, new ScrollViewer { Content = body, MaxHeight = 560, VerticalScrollBarVisibility = ScrollBarVisibility.Auto }, L("Confirm"), L("Close"));
        d.PrimaryButtonClick += (_, a) =>
        {
            if (Apply()) d.IsPrimaryButtonEnabled = false; // closing: a second click never resends
            else a.Cancel = true;
        };
        St.Changed += OnChanged;
        try { await Dialogs.Show(d); } finally { St.Changed -= OnChanged; }
    }

    void OnChanged() { if (St.Dirty("positions", "tpsl")) Refresh(); }

    /// Live numbers and the active list (no calls).
    void Refresh()
    {
        var pos = Pos;
        sideText.Text = pos.IsLong ? L("Long") : L("Short");
        Ui.Fg(sideText, pos.IsLong ? T1.Up : T1.Down);
        sizeText.Text = $"{Fmt.Qty(pos.Qty)} {pos.Base}";
        entryText.Text = $"{Fmt.Px(pos.Entry)} / {Fmt.Px(pos.Mark)}";
        liqText.Text = Fmt.Px(pos.Liq);
        foreach (var u in pnlUpd) u();
        var ex = Existing;
        var key = string.Join(",", ex.Select(r => r.Id + "|" + r.Qty + "|" + r.TriggerPx));
        if (key == activeKey) return;
        activeKey = key;
        activeHost.Children.Clear();
        if (ex.Count == 0) return;
        activeHost.Children.Add(Ui.Text(L("Active"), 12, T1.Mu, semibold: true));
        foreach (var r in ex) activeHost.Children.Add(Row(r));
    }

    void SetError(string? e)
    {
        errHost.Children.Clear();
        if (e != null) errHost.Children.Add(OrderUi.Note(e, T1.Down, OrderUi.ErrorGlyph));
    }

    TextBox Box(string text, string placeholder, Action<string> set)
    {
        var b = OrderUi.NumBox(text, placeholder);
        b.TextChanged += (_, _) => { set(b.Text); foreach (var u in pnlUpd) u(); };
        return b;
    }

    /// An estimated PnL line under an input, refreshed with the text and the position.
    Border PnlLine(Func<(double? Px, double? Qty, bool WithPct)> src)
    {
        var host = new Border();
        void U()
        {
            var (px, q, withPct) = src();
            var pos = Pos;
            if (px is not double x || !(x > 0) || q is not double qty) { host.Child = null; host.Visibility = Visibility.Collapsed; return; }
            var v = (x - pos.Entry) * qty * Sign;
            Color c = (x - pos.Entry) * Sign >= 0 ? T1.Up : T1.Down;
            host.Child = withPct
                ? OrderUi.KV(L("Est. PnL"), $"{Fmt.Signed(v)} USDT ({Fmt.F((x / Math.Max(pos.Entry, 1e-12) - 1) * Sign * 100, 2, true)}%)", v >= 0 ? T1.Up : T1.Down)
                : OrderUi.KV($"{Fmt.Qty(qty)} {pos.Base}", $"{Fmt.Signed(v)} USDT", c);
            host.Visibility = Visibility.Visible;
        }
        pnlUpd.Add(U);
        U();
        return host;
    }

    void BuildMode()
    {
        pnlUpd.Clear();
        modeHost.Children.Clear();
        if (!partial)
        {
            modeHost.Children.Add(OrderUi.Field(L("Take profit"), Box(tp, "USDT", s => tp = s), ""));
            modeHost.Children.Add(PnlLine(() => (OrderNum.Parse(tp), Pos.Qty, true)));
            modeHost.Children.Add(OrderUi.Field(L("Stop loss"), Box(sl, "USDT", s => sl = s), ""));
            modeHost.Children.Add(PnlLine(() => (OrderNum.Parse(sl), Pos.Qty, true)));
            modeHost.Children.Add(OrderUi.Caption(L("Closes the whole position at market when triggered, including size added later.")));
            return;
        }
        foreach (var l in levels)
        {
            var g = new Grid { ColumnSpacing = 6 };
            foreach (var w in new[] { GridLength.Auto, new GridLength(1, GridUnitType.Star), new GridLength(70), GridLength.Auto, GridLength.Auto })
                g.ColumnDefinitions.Add(new ColumnDefinition { Width = w });
            var kind = Ui.Segmented(new[] { (true, L("TP")), (false, L("SL")) }, l.Tp, v => { l.Tp = v; foreach (var u in pnlUpd) u(); });
            var price = Box(l.Price, L("Price"), s => l.Price = s);
            var amount = Box(l.Amount, L("Size"), s => l.Amount = s);
            var unit = new ComboBox { FontSize = 12, MinWidth = 0, VerticalAlignment = VerticalAlignment.Center };
            unit.Items.Add("%");
            unit.Items.Add(Pos.Base);
            unit.SelectedIndex = l.Pct ? 0 : 1;
            unit.SelectionChanged += (_, _) =>
            {
                if (unit.SelectedIndex < 0 || (unit.SelectedIndex == 0) == l.Pct) return;
                l.Pct = unit.SelectedIndex == 0;
                amount.Text = "";
            };
            Ui.Tip(unit, L("Size as % of the position, or in base units"));
            var remove = Ui.Flat(Ui.Icon("", 12));
            remove.Click += (_, _) => { levels.Remove(l); BuildMode(); };
            int col = 0;
            foreach (var e in new FrameworkElement[] { kind, price, amount, unit, remove }) { Grid.SetColumn(e, col++); g.Children.Add(e); }
            var line = PnlLine(() => (OrderNum.Parse(l.Price), QtyOf(l), false));
            line.Margin = new Thickness(80, 0, 0, 0);
            modeHost.Children.Add(Ui.V(3, g, line));
        }
        if (levels.Count < 10)
        {
            var addTp = Ui.Small(new Button { Content = Ui.H(4, Ui.Icon("", 11), Ui.Text(L("Take-profit level"), 12)) });
            var addSl = Ui.Small(new Button { Content = Ui.H(4, Ui.Icon("", 11), Ui.Text(L("Stop-loss level"), 12)) });
            addTp.Click += (_, _) => { levels.Add(new Level { Tp = true }); BuildMode(); };
            addSl.Click += (_, _) => { levels.Add(new Level { Tp = false }); BuildMode(); };
            modeHost.Children.Add(Ui.H(8, addTp, addSl));
        }
        modeHost.Children.Add(OrderUi.Caption(L("Each level closes its size when triggered. Take-profit levels and stop levels each add up to at most the position.")));
    }

    double? QtyOf(Level l)
    {
        if (OrderNum.Parse(l.Amount) is not double a || !(a > 0)) return null;
        var q = l.Pct ? Pos.Qty * Math.Min(a, 100) / 100 : a;
        return OrderNum.Parse(OrderNum.Qty(q, Step));
    }

    /// One live TP/SL: a long's TP triggers at or above, its SL at or below; reversed for shorts.
    Grid Row(TpSlRow r)
    {
        var pos = Pos;
        bool up = r.TakeProfit == pos.IsLong;
        var left = Ui.H(8, Ui.Text(r.TakeProfit ? L("TP") : L("SL"), 13, r.TakeProfit ? T1.Up : T1.Down, semibold: true),
            Ui.Text($"{(r.Trigger == "last" ? L("Last") : L("Mark"))} {(up ? "≥" : "≤")} {Fmt.Px(r.TriggerPx)}", 13, num: true));
        // confirmed in a flyout: a second ContentDialog cannot open over this one
        var fly = new Flyout();
        var yes = Ui.Small(new Button { Content = Ui.Text(L("Cancel TP/SL"), 12, T1.Red) });
        var no = Ui.Small(new Button { Content = L("Keep") });
        fly.Content = Ui.V(8, Ui.Text(L("Cancel this TP/SL?"), 13, semibold: true), Ui.Text($"{(r.TakeProfit ? L("TP") : L("SL"))} {Fmt.Px(r.TriggerPx)}", 12, T1.Mu, num: true), Ui.H(8, yes, no));
        yes.Click += (_, _) =>
        {
            fly.Hide();
            SetError(null);
            if (St.Call("cancel_tpsl", new JsonObject { ["ex"] = r.Ex, ["id"] = r.Id })?["ok"]?.GetValue<bool>() != true) SetError(St.LastError ?? L("Cancel failed"));
        };
        no.Click += (_, _) => fly.Hide();
        var cancel = Ui.Small(new Button { Content = L("Cancel"), Flyout = fly });
        var right = Ui.H(8, Ui.Text(r.Qty is double q ? $"{Fmt.Qty(q)} {pos.Base}" : L("Entire position"), 12, T1.Mu, num: true), cancel);
        return Ui.Spread(left, right);
    }

    /// Validate everything, then send; true when every call succeeded (the dialog closes).
    bool Apply()
    {
        SetError(null);
        var pos = Pos;
        bool lng = pos.IsLong;
        double refPx = pos.Mark;
        string refName = L("the mark price");
        JsonObject Base() => new() { ["ex"] = p.Ex, ["symbol"] = p.Symbol, ["pos"] = p.Side, ["trigger"] = trigLast ? "last" : "mark" };
        var calls = new List<JsonObject>();
        if (!partial)
        {
            double? tpV = OrderNum.Parse(tp), slV = OrderNum.Parse(sl);
            if (tpV == null && slV == null) { SetError(L("Enter a take-profit or stop-loss price")); return false; }
            if (OrderNum.TpSlCheck(lng, refPx, refName, tpV, slV) is string e) { SetError(e); return false; }
            var a = Base();
            if (tpV is double x) a["tp"] = x;
            if (slV is double y) a["sl"] = y;
            calls.Add(a);
        }
        else
        {
            if (levels.Count == 0) { SetError(L("Add at least one level")); return false; }
            double sumTp = 0, sumSl = 0;
            for (int i = 0; i < levels.Count; i++)
            {
                var l = levels[i];
                var tag = $"{L("Level")} {i + 1}: ";
                if (OrderNum.Parse(l.Price) is not double px) { SetError(tag + L("enter a price")); return false; }
                if (QtyOf(l) is not double q || !(q > 0)) { SetError(tag + L("size is zero after rounding")); return false; }
                if (OrderNum.TpSlCheck(lng, refPx, refName, l.Tp ? (double?)px : null, l.Tp ? null : (double?)px) is string e) { SetError(tag + e); return false; }
                if (l.Tp) sumTp += q; else sumSl += q;
                var a = Base();
                a[l.Tp ? "tp" : "sl"] = px;
                a["partial_qty"] = q;
                calls.Add(a);
            }
            var cap = pos.Qty * (1 + 1e-9);
            if (sumTp > cap) { SetError(L("Take-profit levels add up to more than the position")); return false; }
            if (sumSl > cap) { SetError(L("Stop-loss levels add up to more than the position")); return false; }
        }
        for (int i = 0; i < calls.Count; i++)
        {
            if (St.Call("set_tpsl", calls[i])?["ok"]?.GetValue<bool>() == true) continue;
            SetError($"{i} / {calls.Count} {L("sent")}. {St.LastError ?? L("failed")}");
            // some levels are live already: Confirm again would place them twice, so review the Active list instead
            if (i > 0 && dlg != null) dlg.IsPrimaryButtonEnabled = false;
            return false;
        }
        return true;
    }
}
