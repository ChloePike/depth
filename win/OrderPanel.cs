using System.Globalization;
using System.Text.Json.Nodes;
using System.Text.RegularExpressions;
using Microsoft.UI;
using Microsoft.UI.Text;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Controls.Primitives;
using Windows.UI;
using Colors = Microsoft.UI.Colors;
using VirtualKey = Windows.System.VirtualKey;
using static Depth.I18n;

namespace Depth;

/// Order types of the panel. limit / market / stop / trailing are venue orders; scaled and twap
/// are client-side algorithms built from limit orders (work on every venue).
enum OrderKind { Limit, Market, Stop, Trailing, Scaled, Twap }

/// Order entry for the current base's USDT perp (port of OrderPanel.swift). Every price comes from
/// the trade venue's own book (trade.bid/ask); the composite (header.price) never prices an order.
/// Inputs are rebuilt only on structural changes (FormKey); live numbers update in place (Live).
sealed class OrderPanel : Grid
{
    public const double W = 300;
    /// "Transfer" buttons raise this with the venue (the wallet port subscribes; hidden until then).
    public static event Action<string>? TransferRequested;

    bool close;
    OrderKind kind = OrderKind.Limit;
    /// text of every input, by key
    readonly Dictionary<string, string> v = new()
    {
        ["price"] = "", ["qty"] = "", ["tp"] = "", ["sl"] = "", ["trigger"] = "", ["callback"] = "1.0", ["activation"] = "",
        ["scaledFrom"] = "", ["scaledTo"] = "", ["twapMinutes"] = "15", ["twapSlices"] = "10", ["twapSlip"] = "10", ["twapLimit"] = "",
    };
    readonly Dictionary<string, TextBox> boxes = new();
    double pct;
    bool bboOn, bboQueue;
    int bboLevel = 1;
    bool tpslOn, trigLast;
    /// side of the last Buy/Sell click: Enter in a field repeats it
    string? lastPos;
    string? inlineError;
    /// limit time in force: gtc, ioc, fok, post (post-only / ALO)
    string tif = "gtc";
    bool triggerLast, stopLimit;
    int scaledCount = 5;
    double scaledSkew;
    bool scaledPost = true;
    /// run on the venue when it has a native TWAP (default): keeps going with the app closed
    bool twapNative = true, twapRandom;
    /// a confirm dialog is open: buttons and Enter do nothing
    bool busy;

    readonly StackPanel form = Ui.V(14), facts = new();
    readonly Border headerHost = new(), jobsHost = new(), summaryHost = new(), errorHost = new(), pendingHost = new(), accountHost = new();
    readonly Dictionary<Border, string> hostKeys = new();
    readonly List<Action> acctUpd = new();
    TextBlock availText = new(), pctText = new(), factsL = new(), factsR = new();
    TextBlock? twapNote, skewText;
    Slider? slider;
    DropDownButton? routeBtn;
    bool syncing;
    string formKey = "", posKey = "";
    string? sym, venue, clickKey;
    int gen;

    AppState S => Store.Shared.State;
    TradeCtx T => S.Trade;
    double? P(string key) => OrderNum.Parse(v[key]);
    bool UseBbo => kind == OrderKind.Limit && bboOn && T.BboLevels.Count > 0;
    /// a typed limit price is sent: limit (not BBO) and stop-limit
    bool TypedPrice => (kind == OrderKind.Limit && !UseBbo) || (kind == OrderKind.Stop && stopLimit);
    string KindArg => UseBbo ? "bbo" : kind.ToString().ToLowerInvariant();
    double? Mid => T.Bid is double b && T.Ask is double a ? (b + a) / 2 : T.Bid ?? T.Ask;
    /// reference price for size and cost: the typed limit, else the venue's own mid
    double RefPx => kind switch
    {
        OrderKind.Stop => (stopLimit ? P("price") : null) ?? P("trigger") ?? Mid ?? 0,
        OrderKind.Scaled => P("scaledFrom") is double a && P("scaledTo") is double b ? (a + b) / 2 : Mid ?? 0,
        _ => (TypedPrice ? P("price") : null) ?? Mid ?? 0,
    };
    double Lev => Math.Max(T.Lev ?? 1, 1);
    double QtyV => P("qty") ?? 0;
    IEnumerable<PositionRow> Positions => S.Positions.Where(p => p.Symbol == T.Symbol && (S.Route.Smart || p.Ex == T.Venue));
    double Held(string side) => Positions.Where(p => p.Side == side).Sum(p => p.Qty);
    /// 100% of the slider: margin x leverage when opening, the larger held side when closing
    double MaxQty => close ? Math.Max(Held("long"), Held("short")) : RefPx > 0 ? (T.Available ?? 0) * Lev / RefPx : 0;
    bool CanTrade => T.HasKey || S.Route.Smart;
    /// a TP/SL is attached only where its section is shown (see OrderPanel.Submit)
    bool TpSlShown => !close && (kind == OrderKind.Limit || kind == OrderKind.Market);

    public OrderPanel()
    {
        var main = Ui.V(14, headerHost, form, jobsHost, summaryHost, errorHost, pendingHost);
        // the buttons sit between the jobs and the summary
        main.Children.Insert(3, facts);
        var col = Ui.V(8, OrderUi.Card(main), accountHost);
        col.Width = W;
        // cards size to their content; the column scrolls as a whole when the window is short
        Children.Add(new ScrollViewer
        {
            Content = col, VerticalScrollBarVisibility = ScrollBarVisibility.Hidden,
            HorizontalScrollMode = ScrollMode.Disabled, HorizontalScrollBarVisibility = ScrollBarVisibility.Disabled,
        });
        Loaded += (_, _) => { Store.Shared.Changed += OnChanged; OnChanged(); };
        Unloaded += (_, _) => Store.Shared.Changed -= OnChanged;
    }

    // MARK: state

    void OnChanged()
    {
        var s = S;
        var t = T;
        if (Store.Shared.Dirty("prefs", "lang")) gen++;
        if (sym != null && t.Symbol != sym)
        {
            v["price"] = t.Bid is double b ? OrderNum.Px(b, t.Tick) : "";
            v["qty"] = v["tp"] = v["sl"] = "";
            pct = 0;
            inlineError = null;
            gen++;
        }
        sym = t.Symbol;
        if (venue != null && t.Venue != venue && ((kind == OrderKind.Stop && !t.Caps.Stop) || (kind == OrderKind.Trailing && !t.Caps.Trailing))) kind = OrderKind.Limit;
        venue = t.Venue;
        if (!t.BboLevels.Contains(bboLevel)) bboLevel = t.BboLevels.FirstOrDefault(1);
        // a level clicked in the venue's own book becomes the limit price
        var bc = s.BookClick;
        var ck = bc == null ? "" : bc.Ex + "|" + bc.Px.ToString("R", CultureInfo.InvariantCulture);
        if (clickKey != null && ck != clickKey && bc != null && bc.Ex == t.Venue)
        {
            kind = OrderKind.Limit;
            bboOn = false;
            Set("price", OrderNum.Px(bc.Px, t.Tick));
        }
        clickKey = ck;
        if (v["price"].Length == 0 && t.Bid is double bid) Set("price", OrderNum.Px(bid, t.Tick));
        // TP/SL waiting for a fill: set_tpsl is a Call, so it runs after this handler
        var pk = string.Join(",", s.Positions.Select(p => p.Id + "|" + p.Qty.ToString("R", CultureInfo.InvariantCulture)));
        if (pk != posKey)
        {
            posKey = pk;
            if (OrderPendingTpSl.Items.Count > 0) DispatcherQueue.TryEnqueue(() =>
            {
                if (OrderPendingTpSl.Check(Store.Shared.State.Positions, Store.Shared.State.Orders) is string e) { inlineError = "TP/SL: " + e; Live(); }
            });
        }
        if (FormKey() != formKey) Build();
        Live();
    }

    string FormKey()
    {
        var t = T;
        return $"{kind}|{close}|{bboOn}|{bboQueue}|{bboLevel}|{tif}|{tpslOn}|{stopLimit}|{twapNative}|{t.Venue}|{t.Caps.Fok}{t.Caps.Stop}{t.Caps.Trailing}|" +
               $"{string.Join(",", t.BboLevels)}|{(t.NativeTwap == null ? "-" : string.Join(",", t.NativeTwap))}|{CanTrade}|{t.HasKey}|{S.Base}|{T1.IsDark}|{gen}";
    }

    /// Programmatic input change: the field and its box (if shown).
    void Set(string key, string text)
    {
        v[key] = text;
        if (boxes.TryGetValue(key, out var b) && b.Text != text) b.Text = text;
    }

    void Fail(string e, string? focus = null)
    {
        inlineError = e;
        if (focus != null && boxes.TryGetValue(focus, out var b)) b.Focus(FocusState.Programmatic);
        Live();
    }

    // MARK: inputs (rebuilt on structural changes)

    void Build()
    {
        formKey = FormKey();
        boxes.Clear();
        twapNote = skewText = null;
        form.Children.Clear();
        var t = T;

        var side = Ui.Segmented(new[] { (false, L("Open")), (true, L("Close")) }, close, c =>
        {
            if (c == close) return;
            close = c;
            pct = 0;
            v["qty"] = "";
            inlineError = null;
            Build();
            Live();
        }, stretch: true);
        side.HorizontalAlignment = HorizontalAlignment.Stretch;
        form.Children.Add(side);
        form.Children.Add(TypeTabs());
        form.Children.Add(OrderUi.Rule());

        // order
        var avbl = Ui.H(6, Ui.Text(L("Avbl"), 13, T1.Mu), availText = Ui.Text("–", 13, num: true));
        if (t.HasKey && TransferRequested != null)
        {
            var tr = Ui.Flat(Ui.Icon("", 12));
            tr.Foreground = T1.B(T1.Accent);
            tr.Click += (_, _) => TransferRequested?.Invoke(T.Venue);
            avbl.Children.Add(Ui.Tip(tr, L("Move funds between accounts on this venue")));
        }
        form.Children.Add(avbl);
        foreach (var e in KindFields()) form.Children.Add(e);
        form.Children.Add(OrderUi.Field(L("Size"), Box("qty"), S.Base));
        slider = new Slider { Minimum = 0, Maximum = 100, StepFrequency = 25, SnapsTo = SliderSnapsTo.StepValues, IsThumbToolTipEnabled = false, Value = Math.Clamp(pct * 100, 0, 100) };
        slider.ValueChanged += (_, e) =>
        {
            if (syncing) return;
            pct = e.NewValue / 100;
            Set("qty", pct > 0 ? OrderNum.Qty(MaxQty * pct, T.Step) : "");
            Live();
        };
        pctText = Ui.Text("", 12, T1.Mu, num: true);
        pctText.Width = 38;
        pctText.TextAlignment = TextAlignment.Right;
        form.Children.Add(Ui.Tip(Ui.Spread(slider, pctText), close ? L("Share of the position") : L("Share of available margin × leverage")));
        form.Children.Add(OrderUi.Rule());
        if (TpSlShown) form.Children.Add(TpSlSection());

        // buttons + side facts (the facts panel sits after the jobs list, as in the macOS layout)
        facts.Children.Clear();
        facts.Spacing = 8;
        var (l1, l2) = close ? (L("Close Short"), L("Close Long")) : (L("Open Long"), L("Open Short"));
        var b1 = Ui.Tip(OrderUi.Big(l1, T1.Up, CanTrade, () => Submit(close ? "short" : "long")), close ? L("Buy to reduce the short position") : L("Open or add to a long position"));
        var b2 = Ui.Tip(OrderUi.Big(l2, T1.Down, CanTrade, () => Submit(close ? "long" : "short")), close ? L("Sell to reduce the long position") : L("Open or add to a short position"));
        var row = new Grid { ColumnSpacing = 8 };
        row.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) });
        row.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) });
        Grid.SetColumn(b2, 1);
        row.Children.Add(b1);
        row.Children.Add(b2);
        factsL = Ui.Text("", 11, T1.Mu, num: true);
        factsR = Ui.Text("", 11, T1.Mu, num: true);
        factsR.TextAlignment = TextAlignment.Right;
        factsL.TextTrimming = factsR.TextTrimming = TextTrimming.None;
        facts.Children.Add(row);
        facts.Children.Add(Ui.Spread(factsL, factsR));
        hostKeys.Clear();
    }

    TextBox Box(string key, string placeholder = "")
    {
        var b = OrderUi.NumBox(v[key], placeholder);
        b.TextChanged += (_, _) =>
        {
            v[key] = b.Text;
            if (key == "qty" && b.FocusState != FocusState.Unfocused)
            {
                var m = MaxQty;
                pct = m > 0 ? Math.Min(QtyV / m, 1) : 0;
            }
            Live();
        };
        // Enter repeats the last side; a held key's auto-repeat is ignored (one order per press)
        b.KeyDown += (_, e) =>
        {
            if (e.Key != VirtualKey.Enter) return;
            e.Handled = true;
            if (!e.KeyStatus.WasKeyDown) Enter();
        };
        boxes[key] = b;
        return b;
    }

    /// Limit / Market / Stop as a segmented control; Trailing / Scaled / TWAP behind a menu button.
    Grid TypeTabs()
    {
        var c = T.Caps;
        var main = new List<(OrderKind, string)> { (OrderKind.Limit, L("Limit")), (OrderKind.Market, L("Market")) };
        if (c.Stop) main.Add((OrderKind.Stop, L("Stop")));
        var more = new List<(OrderKind, string)>();
        if (c.Trailing) more.Add((OrderKind.Trailing, L("Trailing Stop")));
        more.Add((OrderKind.Scaled, L("Scaled")));
        more.Add((OrderKind.Twap, L("TWAP")));
        bool inMore = more.Any(m => m.Item1 == kind);
        var seg = Ui.Segmented(main, kind, SetKind);
        seg.Opacity = inMore ? 0.6 : 1;
        var f = new MenuFlyout();
        foreach (var (k, name) in more) f.Items.Add(Ui.Check(name, kind == k, () => SetKind(k)));
        var label = inMore ? (kind == OrderKind.Trailing ? L("Trailing") : more.First(m => m.Item1 == kind).Item2) : L("More");
        var btn = Ui.Small(new DropDownButton { Content = label, Flyout = f });
        if (inMore) btn.Foreground = T1.B(T1.Accent);
        return Ui.Spread(seg, btn);
    }

    void SetKind(OrderKind k)
    {
        if (k == kind) return;
        kind = k;
        Build();
        Live();
    }

    /// The kind-specific inputs between "Avbl" and the size.
    List<FrameworkElement> KindFields()
    {
        var t = T;
        var o = new List<FrameworkElement>();
        switch (kind)
        {
            case OrderKind.Limit:
            {
                o.Add(PriceField());
                var opts = new List<(string, string)> { ("gtc", L("GTC · good till cancelled")), ("ioc", L("IOC · fill what's there, cancel the rest")) };
                if (t.Caps.Fok) opts.Add(("fok", L("FOK · fill completely or cancel")));
                opts.Add(("post", L("Post only (ALO) · maker or cancel")));
                var f = new MenuFlyout();
                foreach (var (k, name) in opts) f.Items.Add(Ui.Check(name, tif == k, () => { tif = k; Build(); Live(); }));
                var short_ = UseBbo ? "GTC" : tif switch { "ioc" => "IOC", "fok" => "FOK", "post" => L("Post only"), _ => "GTC" };
                var btn = Ui.Small(new DropDownButton { Content = short_, Flyout = f, IsEnabled = !UseBbo, Background = T1.Clear, BorderThickness = new Thickness(0) });
                Ui.Tip(btn, UseBbo ? L("BBO orders are good till cancelled") : L("How long the order may rest on the book"));
                o.Add(Ui.Spread(Ui.Text(L("Time in force"), 13, T1.Mu), btn));
                break;
            }
            case OrderKind.Market:
                o.Add(PriceField());
                break;
            case OrderKind.Stop:
            {
                var by = new ComboBox { FontSize = 12, MinWidth = 0, VerticalAlignment = VerticalAlignment.Center };
                by.Items.Add(L("Mark"));
                by.Items.Add(L("Last"));
                by.SelectedIndex = triggerLast ? 1 : 0;
                by.SelectionChanged += (_, _) => { if (by.SelectedIndex >= 0) triggerLast = by.SelectedIndex == 1; };
                Ui.Tip(by, L("Which price must reach the trigger: mark (harder to spike) or last trade"));
                o.Add(OrderUi.Field(L("Trigger price"), Box("trigger"), "USDT", by));
                o.Add(OrderUi.Switch(L("Limit order when triggered"), stopLimit, on => { stopLimit = on; Build(); Live(); }));
                if (stopLimit) o.Add(OrderUi.Field(L("Limit price"), Box("price"), "USDT"));
                o.Add(OrderUi.Note(L("Fires once the trigger is reached: above the price it acts as a buy stop / sell take-profit, below it as a sell stop / buy take-profit.")));
                break;
            }
            case OrderKind.Trailing:
            {
                var f = new MenuFlyout();
                foreach (var p in new[] { "0.5", "1.0", "2.0", "5.0" }) { var it = new MenuFlyoutItem { Text = p + "%" }; it.Click += (_, _) => Set("callback", p); f.Items.Add(it); }
                var presets = Ui.Small(new DropDownButton { Content = "", Flyout = f, Background = T1.Clear, BorderThickness = new Thickness(0) });
                o.Add(OrderUi.Field(L("Callback rate"), Box("callback"), "%", presets));
                o.Add(OrderUi.Field(L("Activation price (optional)"), Box("activation"), "USDT"));
                o.Add(OrderUi.Note(t.Venue == "Bybit"
                    ? L("Bybit trails an open position: use it from Close. It closes the whole position at market once price retraces by the callback from its best level.")
                    : L("A market order once price retraces by the callback from its best level since activation.")));
                break;
            }
            case OrderKind.Scaled:
            {
                o.Add(OrderUi.Pair(OrderUi.Field(L("From price"), Box("scaledFrom"), ""), OrderUi.Field(L("To price"), Box("scaledTo"), "")));
                var count = Ui.Text(scaledCount.ToString(CultureInfo.InvariantCulture), 13, num: true);
                count.MinWidth = 22;
                count.TextAlignment = TextAlignment.Center;
                Button minus = Ui.Small(new Button { Content = "−" }), plus = Ui.Small(new Button { Content = "+" });
                void Step(int d)
                {
                    scaledCount = Math.Clamp(scaledCount + d, 2, 50);
                    count.Text = scaledCount.ToString(CultureInfo.InvariantCulture);
                    minus.IsEnabled = scaledCount > 2;
                    plus.IsEnabled = scaledCount < 50;
                }
                minus.Click += (_, _) => Step(-1);
                plus.Click += (_, _) => Step(1);
                Step(0);
                o.Add(Ui.Spread(Ui.Text(L("Orders"), 13, T1.Mu), Ui.H(4, minus, count, plus)));
                skewText = Ui.Text(SkewName(), 13);
                var sk = new Slider { Minimum = -1, Maximum = 1, StepFrequency = 0.25, SnapsTo = SliderSnapsTo.StepValues, Value = scaledSkew, IsThumbToolTipEnabled = false };
                sk.ValueChanged += (_, e) => { scaledSkew = e.NewValue; if (skewText != null) skewText.Text = SkewName(); };
                o.Add(Ui.V(4, Ui.Spread(Ui.Text(L("Size distribution"), 13, T1.Mu), skewText), sk));
                o.Add(OrderUi.Switch(L("Post only (maker or cancel)"), scaledPost, on => scaledPost = on));
                break;
            }
            case OrderKind.Twap:
            {
                bool nat = t.NativeTwap != null && twapNative;
                if (t.NativeTwap is { Count: 2 } r)
                    o.Add(Ui.Tip(OrderUi.Switch(I18n.F(L("Run on %@ (keeps running when Depth is closed)"), t.Venue), twapNative, on => { twapNative = on; Build(); Live(); }),
                        I18n.F(L("%@ slices the order itself; duration %d to %d minutes."), t.Venue, r[0], r[1])));
                if (nat)
                {
                    o.Add(OrderUi.Field(L("Duration (min)"), Box("twapMinutes"), ""));
                    if (t.Venue == "Binance")
                    {
                        o.Add(OrderUi.Field(L("Price limit (optional)"), Box("twapLimit"), "USDT"));
                        o.Add(OrderUi.Note(L("Binance runs the TWAP: at least 1,000 USDT notional, 5 minutes to 24 hours. Cancel it here or in the Binance app.")));
                    }
                    else
                    {
                        o.Add(OrderUi.Switch(L("Randomize slice timing"), twapRandom, on => twapRandom = on));
                        o.Add(OrderUi.Note(L("Hyperliquid runs the TWAP: a slice every 30 seconds, at most 3% slippage each. Cancel it here or on Hyperliquid.")));
                    }
                }
                else
                {
                    o.Add(OrderUi.Pair(OrderUi.Field(L("Duration (min)"), Box("twapMinutes"), ""), OrderUi.Field(L("Slices"), Box("twapSlices"), "")));
                    o.Add(OrderUi.Pair(OrderUi.Field(L("Max slippage (bp)"), Box("twapSlip"), ""), OrderUi.Field(L("Price limit (optional)"), Box("twapLimit"), "")));
                    var n = OrderUi.Note("");
                    twapNote = n.Children.OfType<TextBlock>().First();
                    o.Add(n);
                }
                break;
            }
        }
        return o;
    }

    string SkewName() => scaledSkew < -0.05 ? L("Larger first") : scaledSkew > 0.05 ? L("Larger last") : L("Equal");

    string TwapGap()
    {
        if (P("twapMinutes") is not double m || !double.TryParse(v["twapSlices"], NumberStyles.Float, CultureInfo.InvariantCulture, out var n) || !(m > 0) || !(n > 0)) return "–";
        var sec = m * 60 / n;
        return sec >= 60 ? Fmt.F(sec / 60, 1) + " min" : Fmt.F(sec, 0) + " s";
    }

    /// Price input; BBO lives inside the field: on, the field becomes the level menu.
    FrameworkElement PriceField()
    {
        var t = T;
        bool bboAvail = kind == OrderKind.Limit && t.BboLevels.Count > 0;
        if (kind == OrderKind.Market)
            return OrderUi.Field(L("Price"), OrderUi.NumBox("", L("Market price"), enabled: false), "");
        if (UseBbo)
        {
            var f = new MenuFlyout();
            foreach (var q in new[] { false, true })
            {
                f.Items.Add(Ui.Header(q ? L("Queue") : L("Counterparty")));
                foreach (var lv in t.BboLevels) f.Items.Add(Ui.Check(BboName(q, lv), bboQueue == q && bboLevel == lv, () => { bboQueue = q; bboLevel = lv; Build(); Live(); }));
            }
            var pick = new DropDownButton { Content = BboName(bboQueue, bboLevel), Flyout = f, HorizontalAlignment = HorizontalAlignment.Stretch, HorizontalContentAlignment = HorizontalAlignment.Left };
            Ui.Tip(pick, bboQueue ? L("Own side of the book: rests as a maker at the best price.") : L("Opposite side of the book: fills like a taker at the price the venue sees on arrival."));
            return OrderUi.Field(L("Price"), pick, "", BboButton());
        }
        return OrderUi.Field(L("Price"), Box("price"), "USDT", bboAvail ? BboButton() : null);
    }

    ToggleButton BboButton()
    {
        var b = Ui.Small(new ToggleButton { Content = "BBO", IsChecked = bboOn });
        b.Click += (_, _) => { bboOn = b.IsChecked == true; Build(); Live(); };
        return Ui.Tip(b, L("Priced by the venue's book when the order arrives (Counterparty: opposite side; Queue: own side; number: book level)"));
    }

    static string BboName(bool queue, int level) => $"{(queue ? L("Queue") : L("Counterparty"))} {level}";

    StackPanel TpSlSection()
    {
        var p = Ui.V(10, Ui.Tip(OrderUi.Switch(L("TP/SL"), tpslOn, on => { tpslOn = on; Build(); Live(); }), L("Attach a take-profit / stop-loss to the position this order opens")));
        if (!tpslOn) return p;
        var trig = Ui.Segmented(new[] { (false, L("Mark")), (true, L("Last")) }, trigLast, x => trigLast = x, stretch: true);
        trig.HorizontalAlignment = HorizontalAlignment.Stretch;
        p.Children.Add(Ui.Spread(Ui.Text(L("Trigger"), 13, T1.Mu), trig));
        p.Children.Add(OrderUi.Field(L("Take profit"), Box("tp"), "USDT"));
        p.Children.Add(OrderUi.Field(L("Stop loss"), Box("sl"), "USDT"));
        p.Children.Add(OrderUi.Caption(L("Applies to the whole position once the order fills.")));
        return p;
    }

    // MARK: live values

    void Live()
    {
        var s = S;
        var t = T;
        availText.Text = t.Available is double a ? $"{Fmt.Usd(a)} USDT" : "–";
        pctText.Text = Fmt.F(pct * 100, 0) + "%";
        if (slider != null) { syncing = true; slider.Value = Math.Clamp(pct * 100, 0, 100); syncing = false; }
        if (twapNote != null) twapNote.Text = I18n.F(L("One IOC order every %@ at the venue's best price ± your slippage; slices wait while the price is beyond your limit. Keep this pair open while it runs."), TwapGap());
        factsL.Text = Facts(close ? "short" : "long");
        factsR.Text = Facts(close ? "long" : "short");
        if (routeBtn != null) ToolTipService.SetToolTip(routeBtn, $"{L("Push")} {Ms(t.PushMs)} · {L("RTT")} {Ms(t.RttMs)}");

        Put(headerHost, $"{t.Mode}|{t.Lev}|{t.HasKey}|{t.Verified}|{s.Route.Smart}|{t.Venue}|{gen}", Header);
        summaryHost.Child = Summary();
        Put(errorHost, inlineError ?? "", () => inlineError is string e ? OrderUi.Note(e, T1.Red, OrderUi.ErrorGlyph) : null);
        var nowMs = DateTimeOffset.UtcNow.ToUnixTimeMilliseconds();
        // running jobs, plus finished ones for a minute; venue-run jobs drop off 10 min after their end time
        var jobs = s.Algos.Where(j => (!j.Cancelled && j.Status != "done" && !(j.IsNative == true && nowMs > j.EndMs + 600_000)) || nowMs - j.EndMs < 60_000).ToList();
        Put(jobsHost, string.Join(";", jobs.Select(j => $"{j.Id}|{j.Done}|{j.Sent}|{j.Status}|{j.Cancelled}|{(int)(JobFrac(j, nowMs) * 100)}")) + gen, () => jobs.Count == 0 ? null : AlgoJobs(jobs, nowMs));
        var waiting = OrderPendingTpSl.Items.Where(p => p.Symbol == t.Symbol).ToList();
        Put(pendingHost, string.Join(",", waiting.Select(p => p.Id)) + gen, () => waiting.Count == 0 ? null : Pending(waiting));
        var b = s.Balances.FirstOrDefault(x => x.Ex == t.Venue);
        Put(accountHost, $"{t.Venue}|{b != null}|{b?.UniMmr != null}|{b?.MmRate != null}|{b?.MaintMargin != null}|{b?.AdjEquity != null}|{t.HasKey}|{t.Error}|{gen}", Account);
        foreach (var u in acctUpd) u();
    }

    /// Replace a host's content only when its key changed (keeps buttons alive between polls).
    void Put(Border host, string key, Func<UIElement?> build)
    {
        if (hostKeys.TryGetValue(host, out var k) && k == key) return;
        hostKeys[host] = key;
        var c = build();
        host.Child = c;
        host.Visibility = c == null ? Visibility.Collapsed : Visibility.Visible;
    }

    string Facts(string pos) => close
        ? $"{L("Max")} {Fmt.Qty(Held(pos))} {S.Base}"
        : $"{L("Cost")} {Fmt.Usd(QtyV * RefPx / Lev)} USDT\n{L("Max")} {Fmt.Qty(MaxQty)} {S.Base}";

    static string Ms(double? v) => v is double x ? Fmt.F(x, 0) + "ms" : "–";

    /// Venue and its account mode / leverage (mode read-only: changed on the exchange).
    UIElement Header()
    {
        var t = T;
        var mode = Ui.Small(new Button { Content = t.Mode == null ? "–" : t.Mode == "hedge" ? L("Hedge") : L("One-Way"), IsHitTestVisible = false });
        var lev = Ui.Small(new Button { Content = t.Lev is double l ? OrderUi.G(l) + "x" : "–", IsEnabled = t.HasKey });
        lev.Click += (_, _) => { if (!busy && XamlRoot != null) _ = LeverageSheet.Show(XamlRoot); };
        Ui.Tip(lev, L("Change leverage for this symbol"));
        var left = Ui.H(6, Ui.Tip(mode, L("Position mode for this symbol (change it on the exchange)")), lev);
        if (!t.Verified)
        {
            var w = Ui.Icon(OrderUi.WarnGlyph, 13);
            w.Foreground = T1.B(T1.Orange);
            left.Children.Add(Ui.Tip(w, L("Order placement on this venue is not yet verified on a live account: start with the minimum size.")));
        }
        var f = new MenuFlyout();
        var r1 = new MenuFlyoutItem { Text = L("Order routing…") };
        r1.Click += (_, _) => MainWindow.Current.OpenSettings("trading");
        var r2 = new MenuFlyoutItem { Text = L("API keys…") };
        r2.Click += (_, _) => MainWindow.Current.OpenSettings("keys");
        f.Items.Add(r1);
        f.Items.Add(r2);
        object content = S.Route.Smart ? L("Smart") : Ui.H(4, T1.VenueIcon(t.Venue, 13), Ui.Text(t.Venue, 12));
        routeBtn = Ui.Small(new DropDownButton { Content = content, Flyout = f, Background = T1.Clear, BorderThickness = new Thickness(0) });
        return Ui.Spread(left, routeBtn);
    }

    UIElement Summary()
    {
        var t = T;
        double notional = QtyV * RefPx;
        bool taker = kind is OrderKind.Market or OrderKind.Twap or OrderKind.Trailing || (kind == OrderKind.Stop && !stopLimit) || (UseBbo && !bboQueue)
            || (kind == OrderKind.Limit && (tif == "ioc" || tif == "fok"));
        double rate = taker ? t.Fee.Taker : t.Fee.Maker;
        var p = Ui.V(6, Ui.Spread(Ui.Text($"{(taker ? L("Taker") : L("Maker"))} {Fmt.F(rate * 100, 3)}%", 11, T1.Mu, num: true),
            Ui.Text(notional > 0 ? $"{Fmt.Usd(notional)} USDT · {L("fee")} ≈ {Fmt.Usd(notional * rate, 3)}" : "", 11, T1.Mu, num: true)));
        if (t.Lev == null && t.HasKey && !close) p.Children.Add(OrderUi.Note(L("Leverage unknown: max size and cost assume 1x."), T1.Orange, OrderUi.WarnGlyph));
        if (QtyV > 0 && t.MinQty is double m && QtyV < m)
            p.Children.Add(OrderUi.Note($"{L("Below the minimum size")} {Fmt.Qty(m)} {S.Base}", T1.Orange, OrderUi.WarnGlyph));
        else if (notional > 0 && t.MinNotional is double mn && notional < mn)
            p.Children.Add(OrderUi.Note($"{L("Below the minimum order value")} {Fmt.Usd(mn)} USDT", T1.Orange, OrderUi.WarnGlyph));
        return p;
    }

    static double JobFrac(AlgoJobRow j, long now) => Math.Clamp((double)(now - j.StartedMs) / Math.Max(j.EndMs - j.StartedMs, 1), 0, 1);

    /// Running TWAP jobs: progress, status and cancel.
    UIElement AlgoJobs(List<AlgoJobRow> jobs, long now)
    {
        var p = OrderUi.Section(L("Running algorithms"));
        foreach (var j in jobs)
        {
            var top = Ui.H(6, Ui.Text($"TWAP {(j.Buy ? L("buy") : L("sell"))} {Fmt.Qty(j.Total)}", 13, num: true, semibold: true), Ui.Text($"{j.Ex} · {j.Symbol}", 11, T1.Mu));
            FrameworkElement right = new Grid();
            if (!j.Cancelled && j.Status != "done")
            {
                var c = Ui.Flat(Ui.Text(L("Cancel"), 12, T1.Red));
                long id = j.Id;
                c.Click += (_, _) =>
                {
                    if (Store.Shared.Call("cancel_algo", new JsonObject { ["id"] = id })?["ok"]?.GetValue<bool>() != true) Fail(Store.Shared.LastError ?? L("Failed"));
                };
                right = c;
            }
            var row = Ui.V(4, Ui.Spread(top, right));
            if (j.IsNative == true)
            {
                var frac = JobFrac(j, now);
                row.Children.Add(new ProgressBar { Minimum = 0, Maximum = 1, Value = frac });
                row.Children.Add(Ui.Text($"{L(j.Status)} · {(frac >= 1 ? L("time elapsed: check fills in Trade History") : I18n.F(L("%.0f%% of the time"), frac * 100))}", 11, T1.Mu));
            }
            else
            {
                row.Children.Add(new ProgressBar { Minimum = 0, Maximum = Math.Max(j.Slices, 1), Value = j.Done });
                bool warn = j.Status.StartsWith("waiting") || j.Status.StartsWith("failed");
                row.Children.Add(Ui.Text($"{j.Done}/{j.Slices} · {Fmt.Qty(j.Sent)} {L("sent")} · {L(j.Status)}", 11, warn ? T1.Orange : T1.Mu, num: true));
            }
            p.Children.Add(row);
        }
        return p;
    }

    UIElement Pending(List<OrderPendingTpSl.Item> items)
    {
        var p = OrderUi.Section(L("TP/SL waiting for fill"));
        foreach (var it in items)
        {
            var x = Ui.Flat(Ui.Icon("", 11));
            x.Foreground = T1.B(T1.Red);
            x.Click += (_, _) => { OrderPendingTpSl.Items.Remove(it); Live(); };
            p.Children.Add(Ui.Spread(Ui.H(6, T1.VenueIcon(it.Ex, 12), Ui.Text($"TP {Fmt.Px(it.Tp)} · SL {Fmt.Px(it.Sl)}", 12, num: true)), Ui.Tip(x, L("Forget this TP/SL"))));
        }
        return p;
    }

    /// Account card under the order form (Binance: Account / Portfolio Margin Info / balance).
    UIElement? Account()
    {
        acctUpd.Clear();
        var t = T;
        BalanceRow? Bal() => S.Balances.FirstOrDefault(x => x.Ex == T.Venue);
        var keys = Ui.Small(new Button { Content = L("Add API keys") });
        keys.Click += (_, _) => MainWindow.Current.OpenSettings("keys");
        if (Bal() is not BalanceRow b)
        {
            if (t.HasKey) return null;
            var k = Ui.Icon("", 13);
            k.Foreground = T1.B(T1.Orange);
            return OrderUi.Card(Ui.V(8, Ui.H(6, k, Ui.Text(L("No API key for this venue"), 13, T1.Orange)), keys));
        }
        var title = Ui.Text(L("Account"), 14, semibold: true);
        FrameworkElement head = title;
        if (TransferRequested != null)
        {
            var tr = Ui.Flat(Ui.H(4, Ui.Icon("", 12), Ui.Text(L("Transfer"), 12)));
            tr.Click += (_, _) => TransferRequested?.Invoke(T.Venue);
            head = Ui.Spread(title, Ui.Tip(tr, L("Move funds between this venue's accounts")));
        }
        var p = Ui.V(9, head, OrderUi.Rule());
        void Val(string k, Func<BalanceRow, (string, Color?)> f, string? tip = null, bool big = false)
        {
            var tb = Ui.Text("", big ? 16 : 13, num: true, semibold: big);
            acctUpd.Add(() => { if (Bal() is BalanceRow x) { var (s, c) = f(x); tb.Text = s; Ui.Fg(tb, c); } });
            p.Children.Add(Ui.Tip(Ui.Spread(Ui.Text(k, 13, T1.Mu), tb), tip));
        }
        if (b.UniMmr != null)
            Val(L("UniMMR"), x => (Fmt.Num(x.UniMmr ?? 0, 2), x.UniMmr < 1.5 ? T1.Red : x.UniMmr < 3 ? T1.Orange : T1.Green), L("Portfolio Margin: the whole account is liquidated when UniMMR falls to 1.05"), true);
        else if (b.MmRate != null)
            Val(L("Margin ratio"), x => (Fmt.F((x.MmRate ?? 0) * 100, 2) + "%", x.MmRate > 0.7 ? T1.Red : x.MmRate > 0.4 ? T1.Orange : T1.Green), L("Maintenance margin / margin balance; liquidation at 100%"), true);
        if (b.MaintMargin != null) Val(L("Maintenance margin"), x => ($"{Fmt.Usd(x.MaintMargin)} USD", null));
        if (b.AdjEquity != null) Val(L("Adjusted equity"), x => ($"{Fmt.Usd(x.AdjEquity)} USD", null));
        Val(L("Equity"), x => ($"{Fmt.Usd(x.Equity)} USD", null));
        Val(L("Unrealized PnL"), _ =>
        {
            var u = S.Positions.Where(q => q.Ex == T.Venue).Sum(q => q.Upnl);
            return ($"{Fmt.Signed(u)} USD", u > 0 ? T1.Up : u < 0 ? T1.Down : (Color?)null);
        });
        if (!t.HasKey) p.Children.Add(keys);
        if (t.Error is string e) p.Children.Add(OrderUi.Note(e, T1.Red, OrderUi.ErrorGlyph));
        return OrderUi.Card(p);
    }

    // MARK: actions

    void Enter()
    {
        if (busy) return;
        if (lastPos is string p) Submit(p); else Fail(L("Press Buy or Sell once; Enter then repeats that side."));
    }

    /// Preview first (validates and routes), then the confirm dialog, or place directly when
    /// confirmations are off. Scaled and TWAP are always confirmed.
    async void Submit(string pos)
    {
        if (busy) return;
        inlineError = null;
        lastPos = pos;
        var s = S;
        var t = T;
        double q = QtyV;
        if (!(q > 0)) { Fail(L("Enter a size"), "qty"); return; }
        var args = new JsonObject { ["pos"] = pos, ["close"] = close, ["kind"] = KindArg, ["qty"] = q, ["tif"] = tif };
        string word = pos == "long" ? (close ? L("buy") : L("long")) : (close ? L("sell") : L("short"));
        string Word = pos == "long" ? (close ? L("Buy") : L("Long")) : (close ? L("Sell") : L("Short"));
        switch (kind)
        {
            case OrderKind.Scaled:
            {
                if (P("scaledFrom") is not double a || P("scaledTo") is not double b || !(a > 0) || !(b > 0)) { Fail(L("Enter both prices")); return; }
                args["from"] = a;
                args["to"] = b;
                args["count"] = scaledCount;
                args["skew"] = scaledSkew;
                args["post_only"] = scaledPost;
                await Algo("place_scaled", args, L("Place scaled orders?"),
                    I18n.F(L("%d %@ limit orders from %@ to %@, %@ %@ in total on %@."), scaledCount, word, Fmt.Px(a), Fmt.Px(b), Fmt.Qty(q), s.Base, t.Venue));
                return;
            }
            case OrderKind.Twap:
            {
                if (P("twapMinutes") is not double m || !(m > 0)
                    || !int.TryParse(v["twapSlices"], NumberStyles.AllowLeadingSign, CultureInfo.InvariantCulture, out var n) || n <= 0)
                { Fail(L("Enter a duration and a number of slices")); return; }
                args["minutes"] = m;
                args["slices"] = n;
                args["max_slip_bps"] = P("twapSlip") ?? 10;
                double? lim = P("twapLimit") is double l && l > 0 ? l : null;
                if (lim is double lv) args["limit"] = lv;
                bool nat = t.NativeTwap != null && twapNative;
                args["native"] = nat;
                args["randomize"] = twapRandom;
                if (nat && t.NativeTwap is { Count: 2 } r && (m < r[0] || m > r[1]))
                { Fail(I18n.F(L("%@ TWAP duration must be %d to %d minutes"), t.Venue, r[0], r[1])); return; }
                var limText = lim is double x ? ", " + L("limit") + " " + Fmt.Px(x) : "";
                await Algo("start_twap", args, L("Start TWAP?"), nat
                    ? I18n.F(L("%@ %@ %@ over %@ min, run by %@%@."), Word, Fmt.Qty(q), s.Base, Fmt.Num(m, 0), t.Venue, limText)
                    : I18n.F(L("%@ %@ %@ on %@ in %d slices over %@ min, at most %@ bp slippage per slice%@."), Word, Fmt.Qty(q), s.Base, t.Venue, n, Fmt.Num(m, 0), v["twapSlip"], limText));
                return;
            }
            case OrderKind.Stop:
                if (P("trigger") is not double tr || !(tr > 0)) { Fail(L("Enter a trigger price"), "trigger"); return; }
                args["trigger"] = tr;
                args["trigger_by"] = triggerLast ? "last" : "mark";
                break;
            case OrderKind.Trailing:
                if (P("callback") is not double cb || !(cb >= 0.1 && cb <= 10)) { Fail(L("Callback must be 0.1% to 10%"), "callback"); return; }
                args["callback_pct"] = cb;
                if (P("activation") is double act && act > 0) args["activation"] = act;
                break;
        }
        if (TypedPrice)
        {
            if (P("price") is not double px || !(px > 0)) { Fail(L("Enter a price"), "price"); return; }
            args["price"] = px;
        }
        if (UseBbo) { args["bbo_queue"] = bboQueue; args["bbo_level"] = bboLevel; }
        double? tpV = null, slV = null;
        // macOS also attaches a TP/SL left on from Limit/Market to Stop/Trailing orders, where the section is
        // hidden; here only a visible TP/SL is attached
        if (tpslOn && TpSlShown)
        {
            tpV = P("tp");
            slV = P("sl");
            if (tpV == null && slV == null) { Fail(L("Enter a take-profit or stop-loss price, or turn TP/SL off")); return; }
            if (OrderNum.TpSlCheck(pos == "long", RefPx, L("the order price"), tpV, slV) is string e) { Fail(e); return; }
        }
        var plan = Store.Shared.Call<RoutePlan>("preview", (JsonObject)args.DeepClone());
        if (plan == null) { Fail(Store.Shared.LastError ?? L("Preview failed")); return; }
        if (plan.Legs.Count == 0)
        {
            Fail(plan.ExcludedVenues.Count == 0 ? L("No venue can take this order") : string.Join("\n", plan.ExcludedVenues.Select(x => $"{x.Ex}: {x.Reason}")));
            return;
        }
        var tk = new OrderTicket(pos, close, t.Symbol, s.Base, args, plan, tpV, slV, trigLast ? "last" : "mark");
        if (s.Prefs.Confirm)
        {
            if (XamlRoot == null) return;
            busy = true;
            try { await OrderConfirm.Show(XamlRoot, tk); } finally { busy = false; }
            Live();
        }
        else if (OrderConfirm.Place(tk) is string err) Fail(err);
        else Live();
    }

    /// A scaled or TWAP request: always confirmed, whatever the preference.
    async Task Algo(string op, JsonObject args, string title, string msg)
    {
        if (XamlRoot == null) return;
        var body = Ui.Text(msg + (T.Verified ? "" : "\n" + L("This venue is untested: start with the minimum size.")), 13);
        body.TextWrapping = TextWrapping.Wrap;
        body.TextTrimming = TextTrimming.None;
        var d = Dialogs.New(XamlRoot, body, L("Confirm"), L("Cancel"));
        d.Title = title;
        // Enter cancels: these start several orders over time
        d.DefaultButton = ContentDialogButton.Close;
        busy = true;
        try
        {
            if (await Dialogs.Show(d) == ContentDialogResult.Primary && Store.Shared.Call(op, args)?["ok"]?.GetValue<bool>() != true)
                inlineError = Store.Shared.LastError ?? L("Failed");
        }
        finally { busy = false; }
        Live();
    }
}

/// Pre-validation and input formatting shared by the order views.
static class OrderNum
{
    static readonly CultureInfo Inv = CultureInfo.InvariantCulture;
    static readonly Regex grouped = new(@"^[+-]?\d{1,3}(,\d{3})+(\.\d*)?$");

    /// Invariant number: "." decimal, optional exponent. macOS strips every comma; here commas are
    /// accepted only as thousands groups ("1,234.5"), so a decimal comma ("0,5") is refused, not read as 5.
    public static double? Parse(string s)
    {
        s = s.Trim();
        if (s.Contains(','))
        {
            if (!grouped.IsMatch(s)) return null;
            s = s.Replace(",", "");
        }
        return double.TryParse(s, NumberStyles.AllowLeadingSign | NumberStyles.AllowDecimalPoint | NumberStyles.AllowExponent, Inv, out var d) && double.IsFinite(d) ? d : null;
    }

    public static int? Decimals(double? step) => step is double s && s > 0 ? Math.Max(0, (int)Math.Ceiling(-Math.Log10(s) - 1e-9)) : null;

    /// price for an input box: tick decimals when the venue rules are known, else Fmt.Px's
    public static string Px(double v, double? tick)
    {
        var a = Math.Abs(v);
        int d = Decimals(tick) ?? (a >= 10_000 ? 1 : a >= 100 ? 2 : a >= 1 ? 4 : 6);
        return Fmt.F(v, d);
    }

    /// size floored to the venue's step (never rounds up past the max); 6 decimals when unknown
    public static string Qty(double v, double? step)
    {
        if (step is double s && s > 0 && Decimals(s) is int d) return Fmt.F(Math.Floor(v / s + 1e-9) * s, d);
        var o = Fmt.F(Math.Floor(v * 1e6) / 1e6, 6);
        return o.Contains('.') ? o.TrimEnd('0').TrimEnd('.') : o;
    }

    /// TP/SL against a reference price (mark for a position, the order price for a new order):
    /// TP above / SL below for longs, the reverse for shorts.
    public static string? TpSlCheck(bool lng, double refPx, string refName, double? tp, double? sl)
    {
        if (tp is double a && (a <= 0 || (lng ? a <= refPx : a >= refPx)))
            return $"{(lng ? L("Take-profit must be above") : L("Take-profit must be below"))} {refName} ({Fmt.Px(refPx)})";
        if (sl is double b && (b <= 0 || (lng ? b >= refPx : b <= refPx)))
            return $"{(lng ? L("Stop-loss must be below") : L("Stop-loss must be above"))} {refName} ({Fmt.Px(refPx)})";
        return null;
    }
}

/// TP/SL attached to an opening order: applied with set_tpsl (whole position) once the position on
/// that venue grows past its size at placement.
/// ponytail: checked from the order panel's position updates (like macOS); move into Rust if
/// orders can fill while the panel is not loaded.
static class OrderPendingTpSl
{
    public sealed class Item
    {
        public Guid Id = Guid.NewGuid();
        public string Ex = "", Symbol = "", Pos = "", Trigger = "mark";
        public double BaseQty;
        public double? Tp, Sl;
        public DateTime Until;
    }

    public static readonly List<Item> Items = new();

    static double Held(List<PositionRow> ps, Item it) => ps.FirstOrDefault(p => p.Ex == it.Ex && p.Symbol == it.Symbol && p.Side == it.Pos)?.Qty ?? 0;

    public static void Add(string ex, string symbol, string pos, List<PositionRow> positions, double? tp, double? sl, string trigger)
    {
        var it = new Item { Ex = ex, Symbol = symbol, Pos = pos, Tp = tp, Sl = sl, Trigger = trigger, Until = DateTime.UtcNow.AddSeconds(120) };
        it.BaseQty = Held(positions, it);
        Items.Add(it);
    }

    /// Fires the grown ones, drops expired ones; returns the last set_tpsl error. Never from a Changed handler.
    public static string? Check(List<PositionRow> positions, List<OrderRow> orders)
    {
        var fire = new List<Item>();
        foreach (var it in Items.ToList())
        {
            if (Held(positions, it) > it.BaseQty * (1 + 1e-9) + 1e-12) { fire.Add(it); Items.Remove(it); }
            // a resting limit keeps it alive; otherwise give up after the window
            else if (DateTime.UtcNow > it.Until && !orders.Any(o => o.Ex == it.Ex && o.Symbol == it.Symbol)) Items.Remove(it);
        }
        string? err = null;
        foreach (var it in fire)
        {
            var a = new JsonObject { ["ex"] = it.Ex, ["symbol"] = it.Symbol, ["pos"] = it.Pos, ["trigger"] = it.Trigger };
            if (it.Tp is double tp) a["tp"] = tp;
            if (it.Sl is double sl) a["sl"] = sl;
            if (Store.Shared.Call("set_tpsl", a)?["ok"]?.GetValue<bool>() != true) err = Store.Shared.LastError ?? L("failed");
        }
        return err;
    }
}

/// Leverage for the current symbol on the trade venue: slider up to the venue's maximum, then a
/// confirmed change on the exchange.
static class LeverageSheet
{
    public static async Task Show(XamlRoot root)
    {
        var st = Store.Shared;
        var t0 = st.State.Trade;
        double lev = t0.Lev ?? 10;
        st.Call("leverage_max");
        bool syncing = false;
        var box = OrderUi.NumBox(OrderUi.G(Math.Round(lev, MidpointRounding.AwayFromZero)), "");
        box.Width = 70;
        var body = Ui.V(12);
        var d = Dialogs.New(root, body, "", L("Cancel"));
        d.Title = L("Leverage");
        var info = Ui.V(6);
        var cur = Ui.Text("", 13, num: true);
        info.Children.Add(Ui.Spread(Ui.Text(L("Symbol"), 13, T1.Mu), Ui.Text($"{t0.Symbol} · {t0.Venue}", 13)));
        info.Children.Add(Ui.Spread(Ui.Text(L("Current"), 13, T1.Mu), cur));
        body.Children.Add(info);
        body.Children.Add(Ui.Spread(Ui.Text(L("Leverage"), 13), Ui.H(6, box, Ui.Text("x", 13, T1.Mu))));
        var range = Ui.V(8);
        body.Children.Add(range);
        var foot = Ui.Text(L("Changes the setting on the exchange for this symbol (both sides). Higher leverage moves the liquidation price closer; the venue may refuse it for a large position."), 11, T1.Mu);
        foot.TextWrapping = TextWrapping.Wrap;
        foot.TextTrimming = TextTrimming.None;
        body.Children.Add(foot);
        var err = Ui.V(0);
        body.Children.Add(err);
        body.Width = 380;
        Slider? slider = null;
        double? shownMax = -1;

        void Sync()
        {
            var t = st.State.Trade;
            var maxL = Math.Max(t.MaxLev ?? 0, 1);
            var r = Math.Round(lev, MidpointRounding.AwayFromZero);
            cur.Text = t.Lev is double l ? OrderUi.G(l) + "x" : "–";
            d.PrimaryButtonText = I18n.F(L("Set %gx").Replace("%g", "%@"), OrderUi.G(r));
            d.IsPrimaryButtonEnabled = lev >= 1 && !(t.MaxLev != null && lev > maxL) && r != (t.Lev ?? 0);
            if (slider != null && !syncing) { syncing = true; slider.Value = Math.Clamp(lev, 1, maxL); syncing = false; }
        }

        void Range()
        {
            var t = st.State.Trade;
            if (t.MaxLev == shownMax) return;
            shownMax = t.MaxLev;
            range.Children.Clear();
            slider = null;
            if (t.MaxLev is not double) { range.Children.Add(Ui.H(8, new ProgressRing { IsActive = true, Width = 14, Height = 14 }, Ui.Text(L("Reading the venue's limit…"), 13, T1.Mu))); return; }
            var maxL = Math.Max(t.MaxLev ?? 0, 1);
            slider = new Slider { Minimum = 1, Maximum = maxL, StepFrequency = 1, SnapsTo = SliderSnapsTo.StepValues, Value = Math.Clamp(lev, 1, maxL) };
            slider.ValueChanged += (_, e) =>
            {
                if (syncing) return;
                lev = e.NewValue;
                syncing = true;
                box.Text = OrderUi.G(Math.Round(lev, MidpointRounding.AwayFromZero));
                syncing = false;
                Sync();
            };
            range.Children.Add(Ui.Spread(Ui.H(6, Ui.Text("1x", 11, T1.Mu), slider), Ui.Text(OrderUi.G(maxL) + "x", 11, T1.Mu)));
            slider.Width = 280;
            var presets = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 4 };
            foreach (var pv in new[] { 1.0, 2, 3, 5, 10, 20, 50, 100, 125 }.Where(x => x <= maxL))
            {
                var b = Ui.Small(new Button { Content = OrderUi.G(pv) + "x" });
                b.Click += (_, _) => { lev = pv; syncing = true; box.Text = OrderUi.G(pv); syncing = false; Sync(); };
                presets.Children.Add(b);
            }
            range.Children.Add(new ScrollViewer { Content = presets, HorizontalScrollBarVisibility = ScrollBarVisibility.Hidden, HorizontalScrollMode = ScrollMode.Enabled, VerticalScrollMode = ScrollMode.Disabled });
        }

        box.TextChanged += (_, _) =>
        {
            if (syncing) return;
            if (OrderNum.Parse(box.Text) is double x) lev = x;
            Sync();
        };
        d.PrimaryButtonClick += (_, a) =>
        {
            var n = (int)Math.Round(lev, MidpointRounding.AwayFromZero);
            if (st.Call("set_leverage", new JsonObject { ["leverage"] = n })?["ok"]?.GetValue<bool>() != true)
            {
                a.Cancel = true;
                err.Children.Clear();
                err.Children.Add(OrderUi.Note(st.LastError ?? L("Failed"), T1.Red, OrderUi.ErrorGlyph));
            }
        };
        void OnChanged() { if (st.Dirty("trade")) { Range(); Sync(); } }
        Range();
        Sync();
        st.Changed += OnChanged;
        try { await Dialogs.Show(d); } finally { st.Changed -= OnChanged; }
    }
}

/// Small builders of the order views (macOS: OrderField, OrderKV, OrderNote, PanelSection, glassCard).
static class OrderUi
{
    public const string WarnGlyph = "", ErrorGlyph = "", InfoGlyph = "";

    /// "%g": 10 -> "10", 12.5 -> "12.5"
    public static string G(double v) => v.ToString("0.######", CultureInfo.InvariantCulture);

    public static Border Card(UIElement content) => new()
    {
        Child = content, Padding = new Thickness(14), CornerRadius = new CornerRadius(10), BorderThickness = new Thickness(1),
        Background = T1.B(T1.Bg), BorderBrush = T1.B(T1.Line),
    };

    public static Border Rule() => new() { Height = 1, Background = T1.B(T1.Line) };

    public static StackPanel Section(string title) => Ui.V(10, Ui.Text(title, 12, T1.Mu, semibold: true));

    public static TextBlock Caption(string text)
    {
        var t = Ui.Text(text, 11, T1.Mu);
        t.TextWrapping = TextWrapping.Wrap;
        t.TextTrimming = TextTrimming.None;
        return t;
    }

    public static TextBox NumBox(string text, string placeholder, bool enabled = true) =>
        Ui.Tabular(new TextBox { Text = text, PlaceholderText = placeholder, TextAlignment = TextAlignment.Right, IsEnabled = enabled, MinWidth = 0, IsSpellCheckEnabled = false });

    /// Label above; input, unit and an optional accessory in a row.
    public static StackPanel Field(string label, FrameworkElement input, string unit, FrameworkElement? acc = null)
    {
        var g = new Grid { ColumnSpacing = 6 };
        g.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) });
        g.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        g.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        g.Children.Add(input);
        if (unit.Length > 0) { var u = Ui.Text(unit, 13, T1.Mu); Grid.SetColumn(u, 1); g.Children.Add(u); }
        if (acc != null) { Grid.SetColumn(acc, 2); g.Children.Add(acc); }
        return Ui.V(5, Ui.Text(label, 13, T1.Mu), g);
    }

    /// Two equal columns.
    public static Grid Pair(FrameworkElement a, FrameworkElement b)
    {
        var g = new Grid { ColumnSpacing = 8 };
        g.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) });
        g.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) });
        Grid.SetColumn(b, 1);
        g.Children.Add(a);
        g.Children.Add(b);
        return g;
    }

    /// Label left, switch right. set runs on user toggles only (the initial value is set before the handler).
    public static Grid Switch(string label, bool on, Action<bool> set)
    {
        var s = new ToggleSwitch { IsOn = on, OnContent = "", OffContent = "", MinWidth = 0, HorizontalAlignment = HorizontalAlignment.Right };
        s.Toggled += (_, _) => set(s.IsOn);
        var t = Ui.Text(label, 13);
        t.TextWrapping = TextWrapping.Wrap;
        t.TextTrimming = TextTrimming.None;
        return Ui.Spread(t, s);
    }

    /// Icon + wrapped caption text.
    public static Grid Note(string text, Color? c = null, string glyph = InfoGlyph)
    {
        var col = c ?? T1.Mu;
        var g = new Grid { ColumnSpacing = 6 };
        g.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        g.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) });
        var i = Ui.Icon(glyph, 11);
        i.Foreground = T1.B(col);
        i.VerticalAlignment = VerticalAlignment.Top;
        i.Margin = new Thickness(0, 2, 0, 0);
        var t = Caption(text);
        t.Foreground = T1.B(col);
        t.VerticalAlignment = VerticalAlignment.Top;
        Grid.SetColumn(t, 1);
        g.Children.Add(i);
        g.Children.Add(t);
        return g;
    }

    /// Compact "label  value" line; value secondary unless a color is given.
    public static Grid KV(string k, string v, Color? c = null, bool primary = false)
    {
        var val = Ui.Text(v, 12, c ?? (primary ? null : (Color?)T1.Mu), num: true);
        return Ui.Spread(Ui.Text(k, 12, T1.Mu), val);
    }

    /// "label  value": label secondary, value primary (Form row).
    public static Grid Row(string k, string v, Color? c = null) => Ui.Spread(Ui.Text(k, 13, T1.Mu), Ui.Text(v, 13, c, num: true));

    /// Large green / red action button.
    public static Button Big(string title, Color c, bool enabled, Action a)
    {
        var b = new Button
        {
            Content = Ui.Text(title, 14, Colors.White, semibold: true), Background = T1.B(c), HorizontalAlignment = HorizontalAlignment.Stretch,
            HorizontalContentAlignment = HorizontalAlignment.Center, Height = 40, IsEnabled = enabled, CornerRadius = new CornerRadius(6),
        };
        // lightweight styling: keep the tint on hover / press (the template swaps the background)
        b.Resources["ButtonBackgroundPointerOver"] = T1.B(c, 0.85);
        b.Resources["ButtonBackgroundPressed"] = T1.B(c, 0.7);
        b.Click += (_, _) => a();
        return b;
    }
}
