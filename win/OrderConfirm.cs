using System.Globalization;
using System.Text.Json;
using System.Text.Json.Nodes;
using Microsoft.UI;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Windows.UI;
using Colors = Microsoft.UI.Colors;
using static Depth.I18n;

namespace Depth;

/// A previewed order: the exact preview args (re-sent unchanged to place) and the plan.
sealed record OrderTicket(string Pos, bool Close, string Symbol, string Base, JsonObject Args, RoutePlan Plan, double? Tp, double? Sl, string Trigger)
{
    /// buy = open long or close short
    public bool Buy => (Pos == "long") != Close;
    public string Title => (Pos, Close) switch
    {
        ("long", false) => L("Open Long"),
        ("short", false) => L("Open Short"),
        ("long", true) => L("Close Long"),
        _ => L("Close Short"),
    };
}

/// One ContentDialog at a time (WinUI throws on a second); the dialogs follow the app theme.
static class Dialogs
{
    static bool open;

    public static ContentDialog New(XamlRoot root, object content, string primary, string close) => new()
    {
        XamlRoot = root, RequestedTheme = T1.IsDark ? ElementTheme.Dark : ElementTheme.Light,
        Content = content, PrimaryButtonText = primary, CloseButtonText = close, DefaultButton = ContentDialogButton.Primary,
    };

    /// null when another dialog is open (nothing was shown, nothing may be sent).
    public static async Task<ContentDialogResult?> Show(ContentDialog d)
    {
        if (open) return null;
        open = true;
        try { return await d.ShowAsync(); }
        catch (Exception) { return null; }
        finally { open = false; }
    }
}

/// Order confirmation (port of OrderConfirm.swift): route legs, excluded venues, totals, notes;
/// Confirm sends the previewed args with the expected legs, the dialog stays open on an error.
static class OrderConfirm
{
    /// Send a previewed ticket; registers its TP/SL to be applied once each leg's position exists.
    /// Returns the error text, null on success.
    public static string? Place(OrderTicket t)
    {
        var st = Store.Shared;
        var before = st.State.Positions;
        var args = (JsonObject)t.Args.DeepClone();
        var expect = new JsonArray();
        foreach (var l in t.Plan.Legs) expect.Add(new JsonObject { ["ex"] = l.Ex, ["qty"] = l.Qty });
        // Rust refuses the order if the venues or sizes moved since the preview
        args["expect"] = expect;
        var o = st.Call("place", args);
        if (o?["ok"]?.GetValue<bool>() != true) return st.LastError ?? L("Order failed");
        // ok means the legs were submitted: a reply that fails to decode must not read as a failure (a retry would double the order)
        RoutePlan? plan = null;
        try { plan = o!.Deserialize<RoutePlan>(Json.Opts); } catch (Exception) { }
        plan ??= t.Plan;
        if (t.Tp != null || t.Sl != null)
            foreach (var ex in plan.Legs.Select(l => l.Ex).Distinct())
                OrderPendingTpSl.Add(ex, t.Symbol, t.Pos, before, t.Tp, t.Sl, t.Trigger);
        return null;
    }

    /// Plan leg kind ("market", "limit 123.4", "post 1", "ioc 2", "bbo queue 5") as UI text.
    public static string KindText(string k)
    {
        var p = k.Split(' ');
        var px = p.Length > 1 ? Fmt.Px(double.TryParse(p[1], NumberStyles.Float, CultureInfo.InvariantCulture, out var d) ? (double?)d : null) : "";
        return p[0] switch
        {
            "market" => L("Market"),
            "limit" => $"{L("Limit")} {px}",
            "post" => $"{L("Post Only")} {px}",
            "ioc" => $"{L("IOC limit")} {px}",
            "bbo" => $"BBO {(p.Length > 1 && p[1] == "queue" ? L("Queue") : L("Counterparty"))} {(p.Length > 2 ? p[2] : "")}",
            _ => k,
        };
    }

    public static async Task Show(XamlRoot root, OrderTicket t)
    {
        var plan = t.Plan;
        Color color = t.Buy ? T1.Up : T1.Down;
        var body = Ui.V(16);
        body.Width = 440;
        var title = Ui.Text(t.Title, 14, color, semibold: true);
        body.Children.Add(Ui.Spread(Ui.H(8, T1.CoinIcon(t.Base, 20), Ui.Text(t.Symbol, 15, semibold: true), Ui.Text(L("Perp"), 13, T1.Mu)), title));

        var route = OrderUi.Section(plan.Legs.Count > 1 ? $"{L("Route")} · {plan.Legs.Count} {L("legs")}" : L("Route"));
        foreach (var l in plan.Legs) route.Children.Add(Leg(l, t.Base));
        body.Children.Add(route);

        if (plan.ExcludedVenues.Count > 0)
        {
            var ex = OrderUi.Section(L("Not used"));
            foreach (var x in plan.ExcludedVenues)
            {
                var r = Ui.Text(x.Reason, 12, T1.Mu);
                r.TextWrapping = TextWrapping.Wrap;
                r.TextTrimming = TextTrimming.None;
                r.TextAlignment = TextAlignment.Right;
                ex.Children.Add(Ui.Spread(Ui.H(6, T1.VenueIcon(x.Ex, 12), Ui.Text(x.Ex, 12)), r));
            }
            body.Children.Add(ex);
        }

        var total = OrderUi.Section(L("Total"));
        if (plan.ClampedFrom is double c)
            total.Children.Add(OrderUi.Note($"{L("Size reduced from")} {Fmt.Qty(c)} {L("to")} {Fmt.Qty(plan.TotalQty)} {t.Base} ({(t.Close ? L("position size") : L("margin limit"))})", T1.Orange, OrderUi.WarnGlyph));
        total.Children.Add(OrderUi.Row(L("Size"), $"{Fmt.Qty(plan.TotalQty)} {t.Base}"));
        total.Children.Add(OrderUi.Row(L("Est. average price"), Fmt.Px(plan.Vwap)));
        total.Children.Add(OrderUi.Row(L("Order value"), $"{Fmt.Usd(plan.TotalQty * plan.Vwap)} USDT"));
        total.Children.Add(OrderUi.Row(L("Est. fees"), $"{Fmt.Usd(plan.Fees, 3)} USDT"));
        body.Children.Add(total);

        var notes = Ui.V(8);
        if (t.Tp != null || t.Sl != null)
            notes.Children.Add(OrderUi.Note($"{L("TP")} {Fmt.Px(t.Tp)} · {L("SL")} {Fmt.Px(t.Sl)} ({(t.Trigger == "last" ? L("Last") : L("Mark"))}): {L("set for the whole position once the order has filled and the position exists.")}", T1.Accent));
        var keys = Store.Shared.State.Keys;
        if (plan.Legs.Any(l => keys.FirstOrDefault(k => k.Ex == l.Ex)?.Verified == false))
            notes.Children.Add(OrderUi.Note(L("Order placement on this venue is not yet verified on a live account: start with the minimum size."), T1.Orange, OrderUi.WarnGlyph));
        notes.Children.Add(OrderUi.Note(L("Prices and sizes are rounded to the venue's tick and step.")));
        var err = Ui.V(0);
        notes.Children.Add(err);
        body.Children.Add(notes);

        var d = Dialogs.New(root, new ScrollViewer { Content = body, MaxHeight = 520, VerticalScrollBarVisibility = ScrollBarVisibility.Auto }, $"{L("Confirm")} {t.Title}", L("Cancel"));
        var tint = new Style(typeof(Button));
        tint.Setters.Add(new Setter(Control.BackgroundProperty, T1.B(color)));
        tint.Setters.Add(new Setter(Control.ForegroundProperty, T1.B(Colors.White)));
        d.PrimaryButtonStyle = tint;
        bool sent = false;
        d.PrimaryButtonClick += (_, a) =>
        {
            // a second click while the dialog closes never sends again
            if (sent) return;
            if (Place(t) is string e)
            {
                a.Cancel = true;
                err.Children.Clear();
                err.Children.Add(OrderUi.Note(e, T1.Down, OrderUi.ErrorGlyph));
            }
            else
            {
                sent = true;
                d.IsPrimaryButtonEnabled = false;
            }
        };
        await Dialogs.Show(d);
    }

    static StackPanel Leg(RoutePlan.Leg l, string baseAsset)
    {
        bool buy = (l.Pos == "long") != l.Close;
        var head = Ui.Spread(
            Ui.H(6, T1.VenueIcon(l.Ex, 14), Ui.Text(l.Ex, 13, semibold: true), Ui.Text(buy ? L("Buy") : L("Sell"), 13, buy ? T1.Up : T1.Down, semibold: true)),
            Ui.Text($"{Fmt.Qty(l.Qty)} {baseAsset}", 13, num: true, semibold: true));
        var g = new Grid { ColumnSpacing = 14, RowSpacing = 2 };
        g.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) });
        g.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) });
        for (int i = 0; i < 3; i++) g.RowDefinitions.Add(new RowDefinition { Height = GridLength.Auto });
        void At(FrameworkElement e, int row, int col, int span = 1) { Grid.SetRow(e, row); Grid.SetColumn(e, col); Grid.SetColumnSpan(e, span); g.Children.Add(e); }
        At(OrderUi.KV(L("Order"), KindText(l.Kind), primary: true), 0, 0);
        At(OrderUi.KV(L("Est. price"), Fmt.Px(l.EstPx), primary: true), 0, 1);
        At(OrderUi.KV(L("Est. fee"), Fmt.Usd(l.EstFee, 3)), 1, 0);
        At(OrderUi.KV(L("Slippage"), Fmt.F(l.SlipBps, 1) + " bp", l.SlipBps > 5 ? (Color?)T1.Orange : null), 1, 1);
        At(OrderUi.KV(L("Available after"), $"{Fmt.Usd(l.AvailAfter)} USDT", l.AvailAfter < 0 ? (Color?)T1.Down : null), 2, 0, 2);
        return Ui.V(4, head, g);
    }
}
