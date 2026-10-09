using System.Globalization;
using System.Text.Json.Nodes;
using Microsoft.UI.Text;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Controls.Primitives;
using Microsoft.UI.Xaml.Input;
using Microsoft.UI.Xaml.Media;
using Windows.UI;
using Rectangle = Microsoft.UI.Xaml.Shapes.Rectangle;
using VirtualKey = Windows.System.VirtualKey;
using static Depth.I18n;

namespace Depth;

// Account tables (port of AccountTables.swift): positions, open orders, TP/SL, history.

/// Dense sortable table: a header row of column titles (click to sort, again to reverse) over rows
/// with the same star columns (each at least its width, spare width shared). With an id, rows select
/// (Ctrl+click adds) and right-click opens `menu` for the selection.
sealed class AccountTable<T> : IAccountView
{
    /// Head is the English title (translated when drawn)
    public sealed record Col(string Head, double W, bool Right, Func<T, FrameworkElement> Cell, Func<T, IComparable?>? Key = null);

    // ponytail: rows are plain elements, not a virtualized list; capped so a heavy 7-day history stays
    // cheap. Upgrade path: ItemsRepeater with an IElementFactory if the cap hides rows people need.
    const int MaxRows = 500;

    readonly Col[] cols;
    readonly Func<T, string>? id;
    readonly Func<List<T>, IEnumerable<MenuFlyoutItemBase>>? menu;
    readonly string[] sections;
    readonly Func<AppState, IEnumerable<T>> source;
    readonly Func<string> emptyText;
    readonly Grid root = new();
    readonly Grid head;
    readonly StackPanel body = new();
    readonly HashSet<string> sel = new();
    readonly Dictionary<string, Grid> rowEls = new();
    List<T> rows = new();
    int sortCol;
    bool desc;

    public FrameworkElement View => root;

    public AccountTable(Col[] cols, Func<AppState, IEnumerable<T>> source, string[] sections, Func<string> emptyText,
        int sortCol, bool desc, Func<T, string>? id = null, Func<List<T>, IEnumerable<MenuFlyoutItemBase>>? menu = null)
    {
        this.cols = cols;
        this.source = source;
        this.sections = sections;
        this.emptyText = emptyText;
        this.sortCol = sortCol;
        this.desc = desc;
        this.id = id;
        this.menu = menu;
        head = Row();
        head.Height = 28;
        var inner = new Grid();
        inner.RowDefinitions.Add(new RowDefinition { Height = GridLength.Auto });
        inner.RowDefinitions.Add(new RowDefinition { Height = new GridLength(1) });
        inner.RowDefinitions.Add(new RowDefinition { Height = new GridLength(1, GridUnitType.Star) });
        inner.Children.Add(head);
        var line = AccountCells.Rule();
        Grid.SetRow(line, 1);
        inner.Children.Add(line);
        var vs = new ScrollViewer { Content = body, HorizontalScrollMode = ScrollMode.Disabled, HorizontalScrollBarVisibility = ScrollBarVisibility.Disabled };
        Grid.SetRow(vs, 2);
        inner.Children.Add(vs);
        // wide tables scroll sideways; the header scrolls with the rows
        root.Children.Add(new ScrollViewer
        {
            Content = inner,
            HorizontalScrollMode = ScrollMode.Enabled, HorizontalScrollBarVisibility = ScrollBarVisibility.Auto,
            VerticalScrollMode = ScrollMode.Disabled, VerticalScrollBarVisibility = ScrollBarVisibility.Disabled,
        });
    }

    Grid Row()
    {
        var g = new Grid { Padding = new Thickness(14, 0, 14, 0), ColumnSpacing = 10, Background = T1.Clear };
        foreach (var c in cols) g.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(c.W, GridUnitType.Star), MinWidth = c.W });
        return g;
    }

    public void Update(bool force)
    {
        if (!force && !Store.Shared.Dirty(sections)) return;
        rows = source(Store.Shared.State).ToList();
        if (id != null) sel.IntersectWith(rows.Select(id));
        if (force) Header();
        Render();
    }

    void Header()
    {
        head.Children.Clear();
        for (int i = 0; i < cols.Length; i++)
        {
            var c = cols[i];
            int k = i;
            var t = Ui.Text(L(c.Head) + (k == sortCol && c.Key != null ? (desc ? " ↓" : " ↑") : ""), 11, T1.Mu);
            FrameworkElement cell = t;
            if (c.Key != null)
            {
                var b = Ui.Flat(t);
                b.Padding = new Thickness(0);
                b.Click += (_, _) => { desc = k == sortCol && !desc; sortCol = k; Header(); Render(); };
                cell = b;
            }
            cell.HorizontalAlignment = c.Right ? HorizontalAlignment.Right : HorizontalAlignment.Left;
            cell.VerticalAlignment = VerticalAlignment.Center;
            Grid.SetColumn(cell, i);
            head.Children.Add(cell);
        }
    }

    void Render()
    {
        body.Children.Clear();
        rowEls.Clear();
        if (rows.Count == 0) { body.Children.Add(AccountCells.Empty(emptyText(), null)); return; }
        IEnumerable<T> sorted = rows;
        if (sortCol >= 0 && sortCol < cols.Length && cols[sortCol].Key is { } key)
            sorted = desc ? rows.OrderByDescending(key, Comparer<IComparable?>.Default) : rows.OrderBy(key, Comparer<IComparable?>.Default);
        foreach (var r in sorted.Take(MaxRows))
        {
            var g = Row();
            g.MinHeight = 32;
            for (int i = 0; i < cols.Length; i++)
            {
                var cell = cols[i].Cell(r);
                if (cols[i].Right) cell.HorizontalAlignment = HorizontalAlignment.Right;
                cell.VerticalAlignment = VerticalAlignment.Center;
                Grid.SetColumn(cell, i);
                g.Children.Add(cell);
            }
            if (id != null)
            {
                var rid = id(r);
                rowEls[rid] = g;
                if (sel.Contains(rid)) g.Background = T1.B(T1.Accent, 0.18);
                g.Tapped += (_, _) =>
                {
                    if (!Ui.IsDown(VirtualKey.Control)) sel.Clear();
                    if (!sel.Remove(rid)) sel.Add(rid);
                    Paint();
                };
                if (menu != null)
                    g.RightTapped += (_, e) =>
                    {
                        if (!sel.Contains(rid)) { sel.Clear(); sel.Add(rid); Paint(); }
                        var picked = rows.Where(x => sel.Contains(id(x))).ToList();
                        var f = new MenuFlyout();
                        foreach (var it in menu(picked)) f.Items.Add(it);
                        if (f.Items.Count > 0) f.ShowAt(g, new FlyoutShowOptions { Position = e.GetPosition(g) });
                        e.Handled = true;
                    };
            }
            body.Children.Add(g);
        }
        if (rows.Count > MaxRows)
            body.Children.Add(new TextBlock { Text = $"+{rows.Count - MaxRows}", FontSize = 11, Foreground = T1.B(T1.Dim), Margin = new Thickness(14, 6, 14, 6) });
    }

    void Paint()
    {
        foreach (var (k, g) in rowEls) g.Background = sel.Contains(k) ? T1.B(T1.Accent, 0.18) : T1.Clear;
    }
}

static class AccountTables
{
    static Button CancelButton(Action a)
    {
        var b = Ui.Small(new Button { Content = L("Cancel") });
        b.Click += (_, _) => a();
        return b;
    }

    static void CancelOrder(OrderRow o) => AccountPanel.Act("cancel", new() { ["ex"] = o.Ex, ["id"] = o.Id });
    static void CancelTpSl(TpSlRow t) => AccountPanel.Act("cancel_tpsl", new() { ["ex"] = t.Ex, ["id"] = t.Id });

    public static AccountTable<OrderRow> Orders()
    {
        var cols = new AccountTable<OrderRow>.Col[]
        {
            new("Time", 120, false, o => AccountCells.Time(o.Ts), o => o.Ts),
            new("Symbol", 160, false, o => AccountCells.Symbol(o.Ex, o.Symbol), o => o.Symbol),
            new("Side", 90, false, o => AccountCells.Side(o.Side, o.Pos), o => o.Side),
            new("Type", 100, false, o =>
            {
                var p = Ui.H(4, AccountCells.Txt(o.Kind));
                if (o.ReduceOnly) p.Children.Add(AccountCells.Badge(L("Reduce"), T1.Mu));
                return p;
            }, o => o.Kind),
            new("Price", 90, true, o => AccountCells.Num(Fmt.Px(o.Price)), o => o.Price),
            new("Amount", 80, true, o => AccountCells.Num(Fmt.Qty(o.Qty)), o => o.Qty),
            new("Filled", 80, true, o =>
            {
                var p = Ui.H(4, AccountCells.Num(Fmt.Qty(o.Filled), o.Filled > 0 ? null : T1.Mu));
                if (o.Qty > 0) p.Children.Add(AccountCells.Num("(" + Fmt.Num(o.Filled / o.Qty * 100, 1) + "%)", T1.Mu));
                return p;
            }, o => o.Filled),
            new("Value", 90, true, o => AccountCells.Num(Fmt.Usd(o.Price * o.Qty), T1.Mu)),
            new("", 70, true, o => CancelButton(() => CancelOrder(o))),
        };
        return new(cols, s => s.Orders.OrderByDescending(o => o.Ts).ThenByDescending(o => o.Id, StringComparer.Ordinal), new[] { "orders" },
            () => L("No open orders"), 0, true, o => $"{o.Ex}|{o.Id}",
            sel => new MenuFlyoutItemBase[]
            {
                AccountCells.Item(sel.Count == 1 ? L("Cancel Order") : L("Cancel Selected Orders"), () => sel.ForEach(CancelOrder)),
                AccountCells.Item(L("Copy Order ID"), () => AccountCells.Copy(string.Join("\n", sel.Select(o => o.Id)))),
            });
    }

    public static AccountTable<TpSlRow> TpSl()
    {
        var cols = new AccountTable<TpSlRow>.Col[]
        {
            new("Symbol", 170, false, t => AccountCells.Symbol(t.Ex, t.Symbol), t => t.Symbol),
            new("Position", 80, false, t => AccountCells.Side(t.Pos), t => t.Pos),
            new("Type", 100, false, t => AccountCells.Badge(t.TakeProfit ? L("Take Profit") : L("Stop Loss"), t.TakeProfit ? T1.Up : T1.Down)),
            new("Trigger Price", 100, true, t => AccountCells.Num(Fmt.Px(t.TriggerPx)), t => t.TriggerPx),
            new("Trigger", 90, false, t => AccountCells.Txt(L(t.Trigger == "last" ? "Last price" : "Mark price"), T1.Mu)),
            new("Amount", 100, true, t => t.Qty is double q ? AccountCells.Num(Fmt.Qty(q)) : AccountCells.Txt(L("Entire position"), T1.Mu)),
            new("", 70, true, t => CancelButton(() => CancelTpSl(t))),
        };
        return new(cols, s => s.Tpsl.OrderBy(t => t.Symbol, StringComparer.Ordinal).ThenBy(t => t.Ex, StringComparer.Ordinal).ThenBy(t => t.Id, StringComparer.Ordinal),
            new[] { "tpsl" }, () => L("No TP/SL orders"), 0, false, t => $"{t.Ex}|{t.Id}",
            sel => new MenuFlyoutItemBase[] { AccountCells.Item(sel.Count == 1 ? L("Cancel TP/SL") : L("Cancel Selected TP/SL"), () => sel.ForEach(CancelTpSl)) });
    }

    static Color StatusColor(string s)
    {
        var u = s.ToUpperInvariant();
        if (u.Contains("FILLED") && !u.Contains("PARTIAL")) return T1.Fg;
        if (u.Contains("CANCEL") || u.Contains("EXPIRE")) return T1.Dim;
        if (u.Contains("REJECT")) return T1.Down;
        return T1.Mu;
    }

    public static AccountTable<HistOrder> OrderHistory()
    {
        var cols = new AccountTable<HistOrder>.Col[]
        {
            new("Time", 120, false, o => AccountCells.Time(o.Ts), o => o.Ts),
            new("Symbol", 160, false, o => AccountCells.Symbol(o.Ex, o.Symbol), o => o.Symbol),
            new("Side", 60, false, o => AccountCells.Side(o.Side), o => o.Side),
            new("Type", 80, false, o => AccountCells.Txt(o.Kind), o => o.Kind),
            new("Price", 90, true, o => AccountCells.Num(o.Price > 0 ? Fmt.Px(o.Price) : "—", o.Price > 0 ? null : T1.Dim), o => o.Price),
            new("Avg. Price", 90, true, o => AccountCells.Num(o.Avg > 0 ? Fmt.Px(o.Avg) : "—", o.Avg > 0 ? null : T1.Dim), o => o.Avg),
            new("Filled / Amount", 130, true, o => AccountCells.Num($"{Fmt.Qty(o.Filled)} / {Fmt.Qty(o.Qty)}"), o => o.Filled),
            new("Status", 100, true, o => AccountCells.Txt(o.Status, StatusColor(o.Status)), o => o.Status),
        };
        return new(cols, s => s.History.Orders, new[] { "history" }, () => L("No orders in the last 7 days"), 0, true);
    }

    public static AccountTable<FillRow> Fills()
    {
        var cols = new AccountTable<FillRow>.Col[]
        {
            new("Time", 120, false, f => AccountCells.Time(f.Ts), f => f.Ts),
            new("Symbol", 160, false, f => AccountCells.Symbol(f.Ex, f.Symbol), f => f.Symbol),
            new("Side", 60, false, f => AccountCells.Side(f.Side), f => f.Side),
            new("Price", 90, true, f => AccountCells.Num(Fmt.Px(f.Price)), f => f.Price),
            new("Amount", 80, true, f => AccountCells.Num(Fmt.Qty(f.Qty)), f => f.Qty),
            new("Value", 100, true, f => AccountCells.Num(Fmt.Usd(f.Price * f.Qty)), f => f.Price * f.Qty),
            new("Fee", 80, true, f => AccountCells.Num(Fmt.Num(f.Fee, 4), T1.Mu), f => f.Fee),
            new("Realized PnL", 100, true, f => f.Realized is double r && r != 0 ? AccountCells.Num(Fmt.Signed(r), AccountCells.PnlColor(r)) : AccountCells.Num("—", T1.Dim),
                f => f.Realized ?? 0),
        };
        return new(cols, s => s.History.Fills, new[] { "history" }, () => L("No trades in the last 7 days"), 0, true);
    }

    public static AccountTable<ClosedRow> Closed()
    {
        var cols = new AccountTable<ClosedRow>.Col[]
        {
            new("Closed", 120, false, c => AccountCells.Time(c.Ts), c => c.Ts),
            new("Symbol", 160, false, c => AccountCells.Symbol(c.Ex, c.Symbol), c => c.Symbol),
            new("Side", 60, false, c => c.Long is bool l ? AccountCells.Side(l ? "long" : "short") : AccountCells.Txt("—", T1.Dim),
                c => c.Long is bool l ? (l ? "long" : "short") : ""),
            new("Amount", 90, true, c => AccountCells.Num(Fmt.Qty(c.Qty)), c => c.Qty ?? 0),
            new("Entry", 90, true, c => AccountCells.Num(Fmt.Px(c.Entry))),
            new("Exit", 90, true, c => AccountCells.Num(Fmt.Px(c.Exit))),
            new("Realized PnL", 110, true, c => AccountCells.Num(Fmt.Signed(c.Pnl), AccountCells.PnlColor(c.Pnl)), c => c.Pnl),
        };
        return new(cols, s => s.History.Closed, new[] { "history" }, () => L("No closed positions in the last 7 days"), 0, true);
    }

    /// History table under the "last 7 days" bar with its age and Refresh.
    public static IAccountView History<T>(AccountTable<T> table) => new HistoryView<T>(table);

    sealed class HistoryView<T> : IAccountView
    {
        readonly AccountTable<T> table;
        readonly Grid root = new();
        readonly TextBlock title = Ui.Text("", 11, T1.Dim), age = Ui.Text("", 11, T1.Dim, num: true);
        readonly StackPanel right = Ui.H(6);
        public FrameworkElement View => root;

        public HistoryView(AccountTable<T> t)
        {
            table = t;
            root.RowDefinitions.Add(new RowDefinition { Height = new GridLength(26) });
            root.RowDefinitions.Add(new RowDefinition { Height = new GridLength(1, GridUnitType.Star) });
            var bar = Ui.Spread(Ui.H(8, title, age), right);
            bar.Padding = new Thickness(12, 0, 12, 0);
            root.Children.Add(bar);
            Grid.SetRow(t.View, 1);
            root.Children.Add(t.View);
        }

        public void Update(bool force)
        {
            var st = Store.Shared;
            if (force || st.Dirty("history"))
            {
                var h = st.State.History;
                title.Text = L("Last 7 days, fetched when opened");
                right.Children.Clear();
                if (h.Loading)
                {
                    right.Children.Add(new ProgressRing { IsActive = true, Width = 12, Height = 12, MinWidth = 0, MinHeight = 0 });
                    right.Children.Add(Ui.Text(L("Loading…"), 11, T1.Mu));
                }
                else
                {
                    var b = Ui.Flat(Ui.H(5, Ui.Icon("", 11), Ui.Text(L("Refresh"), 11)));
                    b.Click += (_, _) => Store.Shared.Call("load_history");
                    right.Children.Add(b);
                }
                Tick();
            }
            table.Update(force);
        }

        public void Tick()
        {
            var at = Store.Shared.State.History.UpdatedMs;
            age.Text = at is long a ? $"· {L("updated")} {Math.Max(0, (DateTimeOffset.UtcNow.ToUnixTimeMilliseconds() - a) / 1000)}s {L("ago")}" : "";
        }
    }
}

/// Positions as hand-laid rows (port of PositionList): full-row side tint, aligned numeric columns,
/// close-by-market/limit inline, "Close All" in the close column's header. Rows are kept per position
/// and updated in place, so the close inputs survive the 2 Hz refresh.
sealed class PositionsView : IAccountView
{
    // minimum widths: size, entry, mark, liq, margin, pnl, close, tp/sl, funding; spare width is shared
    static readonly double[] W = { 130, 92, 92, 92, 116, 140, 268, 130, 140 };
    const double SymbolW = 210, ReverseW = 78;

    readonly Grid root = new();
    readonly Grid head = Cols();
    readonly StackPanel list = new();
    readonly Dictionary<string, PosRow> rows = new();
    readonly TextBlock empty = Ui.Text("", 13, T1.Dim);
    List<PositionRow> current = new();
    string order = "";
    public FrameworkElement View => root;

    public PositionsView()
    {
        var inner = new Grid();
        inner.RowDefinitions.Add(new RowDefinition { Height = GridLength.Auto });
        inner.RowDefinitions.Add(new RowDefinition { Height = new GridLength(1) });
        inner.RowDefinitions.Add(new RowDefinition { Height = new GridLength(1, GridUnitType.Star) });
        head.Height = 30;
        inner.Children.Add(head);
        var line = AccountCells.Rule();
        Grid.SetRow(line, 1);
        inner.Children.Add(line);
        var vs = new ScrollViewer { Content = list, HorizontalScrollMode = ScrollMode.Disabled, HorizontalScrollBarVisibility = ScrollBarVisibility.Disabled };
        Grid.SetRow(vs, 2);
        inner.Children.Add(vs);
        root.Children.Add(new ScrollViewer
        {
            Content = inner,
            HorizontalScrollMode = ScrollMode.Enabled, HorizontalScrollBarVisibility = ScrollBarVisibility.Auto,
            VerticalScrollMode = ScrollMode.Disabled, VerticalScrollBarVisibility = ScrollBarVisibility.Disabled,
        });
        empty.HorizontalAlignment = HorizontalAlignment.Center;
        empty.Margin = new Thickness(0, 24, 0, 0);
    }

    /// symbol (fixed) | 9 star columns with minimums | reverse (fixed)
    static Grid Cols()
    {
        var g = new Grid { Padding = new Thickness(14, 0, 14, 0) };
        g.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(SymbolW) });
        foreach (var w in W) g.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star), MinWidth = w });
        g.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(ReverseW) });
        return g;
    }

    static void Put(Grid g, FrameworkElement e, int col, bool right = true)
    {
        e.HorizontalAlignment = right ? HorizontalAlignment.Right : HorizontalAlignment.Left;
        e.VerticalAlignment = VerticalAlignment.Center;
        Grid.SetColumn(e, col);
        g.Children.Add(e);
    }

    void Header()
    {
        head.Children.Clear();
        Put(head, Ui.Text(L("Symbol"), 11, T1.Mu), 0, false);
        string[] heads = { L("Size"), L("Entry"), L("Mark"), L("Liq. Price"), L("Margin"), L("PnL (ROE%)") };
        for (int i = 0; i < heads.Length; i++) Put(head, Ui.Text(heads[i], 11, T1.Mu), i + 1);
        var closeAll = Ui.Flat(Ui.Text(L("Close All Positions"), 11, T1.Orange));
        closeAll.IsEnabled = current.Count > 0;
        closeAll.Click += async (_, _) =>
        {
            var ps = current.ToList();
            if (ps.Count == 0) return;
            var msg = string.Join("\n", ps.Select(p => $"{p.Ex} {p.Symbol} {L(p.IsLong ? "Long" : "Short")} {Fmt.Qty(p.Qty)}"));
            if (!await ModalDialog.Confirm(L("Close all positions at market?"), msg, L("Close All"))) return;
            foreach (var p in ps) Store.Shared.Call("close_position", new() { ["ex"] = p.Ex, ["symbol"] = p.Symbol, ["side"] = p.Side, ["qty"] = p.Qty });
        };
        Ui.Tip(closeAll, L("Market-close every position listed here (asks to confirm)"));
        var close = Ui.H(8, Ui.Text(L("Close Position"), 11, T1.Mu), closeAll);
        close.Margin = new Thickness(18, 0, 0, 0);
        Put(head, close, 7, false);
        Put(head, Ui.Text(L("TP / SL"), 11, T1.Mu), 8);
        Put(head, Ui.Text(L("Est. Funding"), 11, T1.Mu), 9);
    }

    public void Update(bool force)
    {
        var st = Store.Shared;
        if (!force && !st.Dirty("positions", "tpsl", "base", "binance_pm")) return;
        var s = st.State;
        // stable order: pushes reorder the engine's vector; ties broken by venue then side
        var cur = s.Base + "USDT";
        bool hide = AccountPanel.HideOthers;
        var ps = s.Positions.Where(p => !hide || p.Symbol == cur)
            .OrderBy(p => p.Symbol, StringComparer.Ordinal).ThenBy(p => p.Ex, StringComparer.Ordinal).ThenBy(p => p.Side, StringComparer.Ordinal).ToList();
        bool hadAny = current.Count > 0;
        current = ps;
        if (force || hadAny != (ps.Count > 0)) Header();
        if (force) { rows.Clear(); order = ""; }
        var ids = string.Join(";", ps.Select(p => p.Id));
        if (ids != order)
        {
            order = ids;
            list.Children.Clear();
            foreach (var gone in rows.Keys.Except(ps.Select(p => p.Id)).ToList()) rows.Remove(gone);
            empty.Text = L("No open positions");
            if (ps.Count == 0) list.Children.Add(empty);
            foreach (var p in ps)
            {
                if (!rows.TryGetValue(p.Id, out var r)) rows[p.Id] = r = new PosRow();
                list.Children.Add(r.Root);
                list.Children.Add(AccountCells.Rule());
            }
        }
        foreach (var p in ps) rows[p.Id].Set(p, s);
    }

    public void Tick() { foreach (var r in rows.Values) r.Tick(); }

    /// One position row; its controls are built once and their values updated on every refresh.
    sealed class PosRow
    {
        public readonly Grid Root = new() { Height = 50, Background = T1.Clear };
        readonly Grid cells = Cols();
        readonly Rectangle bar = new() { Width = 3 };
        readonly Rectangle fade = new() { Width = 260 };
        readonly Border symbol = new();
        readonly TextBlock sizeUsd = Num(), sizeQty = Num(10.5), entry = Num(), mark = Num(), liq = Num(), margin = Num(), mode = Num(10.5),
            pnl = Num(), roe = Num(10.5), tp = Num(), slash = Num(), sl = Num(), rate = Num(), countdown = Num(), est = Num(10.5);
        readonly TextBox px = Field(82), qty = Field(72);
        readonly StackPanel tpsl;
        readonly StackPanel funding;
        PositionRow p = new();
        string symKey = "";
        bool pxEdited, qtyEdited, seeding;

        static TextBlock Num(double size = 12)
        {
            var t = Ui.Text("", size, num: true);
            t.HorizontalAlignment = HorizontalAlignment.Right;
            return t;
        }

        static TextBox Field(double w)
        {
            var t = new TextBox { Width = w, FontSize = 12, MinHeight = 0, Padding = new Thickness(6, 3, 6, 3), TextAlignment = TextAlignment.Right, VerticalAlignment = VerticalAlignment.Center };
            return Ui.Tabular(t);
        }

        public PosRow()
        {
            var bg = Ui.H(0, bar, fade);
            bg.HorizontalAlignment = HorizontalAlignment.Left;
            Root.Children.Add(bg);
            Root.Children.Add(cells);

            Put(cells, symbol, 0, false);
            Put(cells, Ui.V(1, sizeUsd, sizeQty), 1);
            Put(cells, entry, 2);
            Put(cells, mark, 3);
            Put(cells, liq, 4);
            Put(cells, Ui.V(1, margin, mode), 5);
            var share = Ui.V(1, pnl, roe);
            Put(cells, share, 6);

            // Market | Limit, then the limit price and the size to close (defaults: mark, whole position)
            var market = Ui.Flat(Ui.Text(L("Market"), 11.5, T1.Orange));
            var limit = Ui.Flat(Ui.Text(L("Limit"), 11.5, T1.Orange));
            market.Click += (_, _) => { var q = CloseQty(); if (q > 0) Market(p, q); };
            limit.Click += (_, _) =>
            {
                var q = CloseQty();
                if (q > 0 && AccountCells.Parse(px.Text) is double x && x > 0) Limit(p, x, q);
            };
            px.TextChanged += (_, _) => { if (!seeding) pxEdited = true; };
            qty.TextChanged += (_, _) => { if (!seeding) qtyEdited = true; };
            Ui.Tip(px, L("Limit close price"));
            var close = Ui.H(6, market, Ui.Text("|", 11, T1.Dim), limit, px, qty);
            close.Margin = new Thickness(18, 0, 0, 0);
            Put(cells, close, 7, false);

            var edit = Ui.Flat(Ui.Icon("", 12));
            edit.Foreground = T1.B(T1.Mu);
            edit.Click += (_, _) => AccountPanel.OpenTpSl(p);
            Ui.Tip(edit, L("Whole-position or staged TP/SL"));
            tpsl = Ui.H(4, Ui.H(3, tp, slash, sl), edit);
            Put(cells, tpsl, 8);

            funding = Ui.V(1, Ui.H(4, rate, countdown), est);
            ((StackPanel)funding.Children[0]).HorizontalAlignment = HorizontalAlignment.Right;
            Put(cells, funding, 9);

            var rev = Ui.Small(new Button { Content = L("Reverse") });
            rev.Click += (_, _) => Reverse(p);
            Ui.Tip(rev, L("Close at market and open the same size on the other side (asks to confirm)"));
            Put(cells, rev, 10);

            var mf = new MenuFlyout();
            mf.Items.Add(AccountCells.Item(L("Close at Market…"), () => Market(p, p.Qty)));
            mf.Items.Add(AccountCells.Item(L("Limit Close in Order Panel"), () => AccountPanel.RaisePrefillClose(p)));
            mf.Items.Add(AccountCells.Item(L("TP/SL…"), () => AccountPanel.OpenTpSl(p)));
            mf.Items.Add(new MenuFlyoutSeparator());
            mf.Items.Add(AccountCells.Item(L("Copy Symbol"), () => AccountCells.Copy(p.Symbol)));
            Root.ContextFlyout = mf;
        }

        double CloseQty() => Math.Min(AccountCells.Parse(qty.Text) ?? 0, p.Qty);

        static int PriceDp(double v) => v >= 10_000 ? 1 : v >= 100 ? 2 : v >= 1 ? 4 : 6;
        static int QtyDp(double v) => v >= 1000 ? 1 : v >= 1 ? 3 : 5;

        public void Set(PositionRow n, AppState s)
        {
            p = n;
            var side = p.IsLong ? T1.Up : T1.Down;
            bar.Fill = T1.B(side);
            var lg = new LinearGradientBrush { StartPoint = new Windows.Foundation.Point(0, 0.5), EndPoint = new Windows.Foundation.Point(1, 0.5) };
            lg.GradientStops.Add(new GradientStop { Color = Color.FromArgb(41, side.R, side.G, side.B), Offset = 0 });
            lg.GradientStops.Add(new GradientStop { Color = Color.FromArgb(0, side.R, side.G, side.B), Offset = 1 });
            fade.Fill = lg;
            var key = $"{p.Side}|{p.Lev}|{s.Lang}|{T1.IsDark}|{side}";
            if (key != symKey)
            {
                symKey = key;
                symbol.Child = AccountCells.Symbol(p.Ex, p.Symbol, AccountCells.Badge(L(p.IsLong ? "Long" : "Short") + (p.Lev > 0 ? $" {(int)p.Lev}x" : ""), side));
            }
            sizeUsd.Text = $"{Fmt.Usd(p.Qty * p.Mark)} USDT";
            Ui.Fg(sizeUsd, side);
            sizeQty.Text = $"{Fmt.Qty(p.Qty)} {p.Base}";
            Ui.Fg(sizeQty, T1.Mu);
            entry.Text = Fmt.Px(p.Entry);
            mark.Text = Fmt.Px(p.Mark);
            if (p.Liq is double l) { liq.Text = Fmt.Px(l); Ui.Fg(liq, T1.Orange); Ui.Tip(liq, null); }
            else
            {
                liq.Text = "—";
                Ui.Fg(liq, T1.Dim);
                bool pm = s.BinancePm && p.Ex == "Binance";
                Ui.Tip(liq, pm ? L("Portfolio Margin liquidates on the whole account's uniMMR; the exchange gives no per-position liquidation price.") : L("No liquidation price reported"));
            }
            margin.Text = $"{Fmt.Usd(p.Margin)} USDT";
            mode.Text = p.Cross is bool c ? (c ? L("Cross") : L("Isolated")) : "";
            Ui.Fg(mode, T1.Mu);
            double roeV = p.Margin > 0 ? p.Upnl / p.Margin : 0;
            pnl.Text = $"{Fmt.Signed(p.Upnl)} USDT";
            roe.Text = Fmt.Signed(roeV * 100) + "%";
            Ui.Fg(pnl, AccountCells.PnlColor(p.Upnl));
            Ui.Fg(roe, AccountCells.PnlColor(p.Upnl));

            // the inputs follow the live mark / size until the user types in them
            seeding = true;
            if (!pxEdited) px.Text = p.Mark.ToString("F" + PriceDp(p.Mark), AccountCells.Inv);
            if (!qtyEdited) qty.Text = p.Qty.ToString("F" + QtyDp(p.Qty), AccountCells.Inv);
            seeding = false;
            Ui.Tip(qty, $"{L("Size to close")} ({p.Base})");

            var mine = s.Tpsl.Where(t => t.Ex == p.Ex && t.Symbol == p.Symbol && t.Pos == p.Side).ToList();
            var tps = mine.Where(t => t.TakeProfit).Select(t => t.TriggerPx).Order().ToList();
            var sls = mine.Where(t => !t.TakeProfit).Select(t => t.TriggerPx).Order().ToList();
            if (mine.Count == 0)
            {
                tp.Text = "—"; Ui.Fg(tp, T1.Dim);
                slash.Text = sl.Text = "";
                Ui.Tip(tpsl, null);
            }
            else
            {
                tp.Text = tps.Count == 0 ? "—" : string.Join(", ", tps.Select(x => Fmt.Px(x)));
                Ui.Fg(tp, tps.Count == 0 ? T1.Dim : T1.Up);
                slash.Text = "/";
                Ui.Fg(slash, T1.Dim);
                sl.Text = sls.Count == 0 ? "—" : string.Join(", ", sls.Select(x => Fmt.Px(x)));
                Ui.Fg(sl, sls.Count == 0 ? T1.Dim : T1.Down);
                Ui.Tip(tpsl, string.Join("\n", mine.Select(t => $"{(t.TakeProfit ? "TP" : "SL")} {Fmt.Px(t.TriggerPx)} ({t.Trigger}) · {(t.Qty is double q ? Fmt.Qty(q) : L("entire position"))}")));
            }

            // venue rate per settlement and countdown; underneath, the estimated payment at the next one
            if (p.FundingRate is double r)
            {
                rate.Text = Fmt.F(r * 100, 4, true) + "%";
                Ui.Fg(rate, r >= 0 ? T1.Up : T1.Down);
                Ui.Fg(countdown, T1.Mu);
                est.Text = p.FundingEst is double e ? $"{L("next")} {Fmt.Signed(e)} USDT" : "";
                Ui.Fg(est, (p.FundingEst ?? 0) >= 0 ? T1.Up : T1.Down);
                Ui.Tip(funding, p.FundingIntervalH is double h ? $"{L("Settles every")} {h.ToString("G", CultureInfo.InvariantCulture)}h" : null);
            }
            else
            {
                rate.Text = "—";
                Ui.Fg(rate, T1.Dim);
                est.Text = "";
                Ui.Tip(funding, L("Funding is shown for Binance, Bybit and Hyperliquid positions"));
            }
            Tick();
        }

        public void Tick()
        {
            countdown.Text = p.FundingRate != null && p.NextFundingMs is long n
                ? Fmt.Hms(Math.Max(0, (n - DateTimeOffset.UtcNow.ToUnixTimeMilliseconds()) / 1000)) : "";
        }

        static string SideName(PositionRow p) => L(p.IsLong ? "Long" : "Short");

        static async void Market(PositionRow p, double q)
        {
            var msg = $"{p.Ex} {p.Symbol} {SideName(p)} {Fmt.Qty(q)} {p.Base} ≈ {Fmt.Usd(q * p.Mark, 0)} USDT\n{L("A market order fills against the book; slippage applies.")}";
            if (!await ModalDialog.Confirm(L("Close position at market?"), msg, L("Close at Market"))) return;
            AccountPanel.Act("close_position", new() { ["ex"] = p.Ex, ["symbol"] = p.Symbol, ["side"] = p.Side, ["qty"] = q });
        }

        static async void Limit(PositionRow p, double x, double q)
        {
            var msg = $"{p.Ex} {p.Symbol} {SideName(p)} {Fmt.Qty(q)} {p.Base} @ {Fmt.Px(x)}";
            if (!await ModalDialog.Confirm(L("Place limit close?"), msg, L("Place Limit Close"))) return;
            AccountPanel.Act("close_position", new() { ["ex"] = p.Ex, ["symbol"] = p.Symbol, ["side"] = p.Side, ["qty"] = q, ["price"] = x });
        }

        static async void Reverse(PositionRow p)
        {
            var other = L(p.IsLong ? "Short" : "Long");
            var msg = $"{p.Ex} {p.Symbol}: {L("close")} {SideName(p)} {Fmt.Qty(p.Qty)} {p.Base}, {L("then open")} {other} {Fmt.Qty(p.Qty)} {p.Base}\n{L("Two market orders; if the second fails you are left flat.")}";
            if (!await ModalDialog.Confirm(L("Reverse position?"), msg, L("Reverse"))) return;
            AccountPanel.Act("reverse_position", new() { ["ex"] = p.Ex, ["symbol"] = p.Symbol, ["side"] = p.Side });
        }
    }
}
