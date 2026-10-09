using System.Text.Json.Nodes;
using Microsoft.UI.Text;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Media;
using static Depth.I18n;

namespace Depth;

/// API keys per venue (port of SettingsKeys.swift), stored by the engine in the Windows Credential
/// Manager. Secrets are write-only: the form never shows a stored secret (only the key's last four
/// characters), the typed values live only while the form is open and are dropped on save or close.
sealed class SettingsKeys
{
    string? editing;
    string key = "", secret = "", extra = "";
    string? error;
    readonly HashSet<string> testing = new();

    /// Drop the form and anything typed into it.
    public void Close() { editing = null; key = ""; secret = ""; extra = ""; error = null; }

    public FrameworkElement Build(Action rebuild)
    {
        var warn = Ui.Icon("", 16);
        warn.Foreground = T1.B(T1.Orange);
        var warnText = Ui.Text(L("Never enable withdrawals on these keys. Trading needs trade permission, transfers need transfer permission. Start with a read-only key."), 12);
        Wrap(warnText);
        var top = new Grid { ColumnSpacing = 10 };
        top.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        top.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) });
        Grid.SetColumn(warnText, 1);
        top.Children.Add(warn);
        top.Children.Add(warnText);

        var rows = new List<FrameworkElement>();
        foreach (var k in Store.Shared.State.Keys)
        {
            rows.Add(Row(k, rebuild));
            if (editing == k.Ex) rows.Add(Form(k, rebuild));
        }
        var p = Ui.V(18,
            SettingsUi.Section(null, L("Stored in the Windows Credential Manager (target \"terminal-one\"), never written to a file."), top),
            SettingsUi.Section(L("Venues"), null, rows.ToArray()));
        if (error != null && editing == null)
        {
            var e = Ui.Text(error, 11, T1.Down);
            Wrap(e);
            p.Children.Insert(1, e);
        }
        return p;
    }

    static void Wrap(TextBlock t) { t.TextWrapping = TextWrapping.Wrap; t.TextTrimming = TextTrimming.None; }

    FrameworkElement Row(KeyStatus k, Action rebuild)
    {
        var name = Ui.Text(k.Ex, 13);
        name.FontWeight = FontWeights.Medium;
        var left = Ui.H(8, T1.VenueIcon(k.Ex, 18), name);
        if (!k.Verified)
            left.Children.Add(Ui.Tip(AccountCells.Badge(L("Untested"), T1.Orange),
                L("Implemented and unit-tested, but not yet run against a live account. Test with a read-only key first, then trade the minimum size.")));

        var right = Ui.H(6);
        if (k.Configured)
        {
            var check = Ui.Icon("", 10);
            check.Foreground = T1.B(T1.Up);
            var status = Ui.V(2, Ui.H(4, check, Ui.Text($"{L("Configured")} ••••{k.Tail ?? ""}", 11, T1.Up, num: true)));
            status.VerticalAlignment = VerticalAlignment.Center;
            if (k.Test is { } t)
            {
                var m = Ui.Text(t.Msg, 10.5, t.Ok ? T1.Up : T1.Down);
                m.MaxWidth = 260;
                m.MaxLines = 2;
                m.TextWrapping = TextWrapping.Wrap;
                m.HorizontalAlignment = HorizontalAlignment.Right;
                status.Children.Add(Ui.Tip(m, t.Msg));
            }
            else if (testing.Contains(k.Ex)) status.Children.Add(Ui.Text(L("Testing…"), 10.5, T1.Mu));
            foreach (var c in status.Children.OfType<FrameworkElement>()) c.HorizontalAlignment = HorizontalAlignment.Right;
            right.Children.Add(status);

            var test = Ui.Small(new Button { Content = L("Test") });
            test.Click += (_, _) =>
            {
                testing.Add(k.Ex);
                error = Store.Shared.Call("test_keys", new() { ["ex"] = k.Ex })?["ok"]?.GetValue<bool>() == true ? null : Store.Shared.LastError;
                rebuild();
            };
            right.Children.Add(Ui.Tip(test, L("Read-only check: balances and permissions")));
            var replace = Ui.Small(new Button { Content = L("Replace") });
            replace.Click += (_, _) => { Open(k.Ex); rebuild(); };
            right.Children.Add(replace);
            right.Children.Add(RemoveButton(k.Ex, rebuild));
        }
        else
        {
            right.Children.Add(Ui.Text(L("Not configured"), 11, T1.Dim));
            var add = Ui.Small(new Button { Content = L("Add…") });
            add.Click += (_, _) => { Open(k.Ex); rebuild(); };
            right.Children.Add(add);
        }
        return SettingsUi.Row(left, right);
    }

    /// Remove asks first, in a flyout (a second ContentDialog cannot open over Settings).
    Button RemoveButton(string ex, Action rebuild)
    {
        var b = Ui.Small(new Button { Content = L("Remove"), Foreground = T1.B(T1.Down) });
        var fly = new Flyout();
        var msg = Ui.Text($"{ex}: " + L("The key is deleted from the Credential Manager and the venue disconnects from your account."), 12);
        Wrap(msg);
        msg.MaxWidth = 300;
        var cancel = new Button { Content = L("Cancel") };
        cancel.Click += (_, _) => fly.Hide();
        var yes = new Button { Content = L("Remove"), Foreground = T1.B(T1.Down) };
        yes.Click += (_, _) =>
        {
            fly.Hide();
            error = Store.Shared.Call("delete_keys", new() { ["ex"] = ex })?["ok"]?.GetValue<bool>() == true ? null : Store.Shared.LastError;
            rebuild();
        };
        var buttons = Ui.H(8, cancel, yes);
        buttons.HorizontalAlignment = HorizontalAlignment.Right;
        fly.Content = Ui.V(10, Ui.Text(L("Remove API key?"), 13, semibold: true), msg, buttons);
        b.Flyout = fly;
        return b;
    }

    /// OKX and Bitget keys are unusable without their passphrase.
    static bool NeedsExtra(string ex) => ex is "Okx" or "Bitget";

    FrameworkElement Form(KeyStatus k, Action rebuild)
    {
        var labels = k.Labels.Concat(new string?[] { null, null, null }).ToList();
        var save = new Button { Content = L("Save") };
        if (Application.Current.Resources.TryGetValue("AccentButtonStyle", out var st) && st is Style accent) save.Style = accent;
        void Enable() => save.IsEnabled = key.Trim().Length > 0 && secret.Length > 0 && (!NeedsExtra(k.Ex) || extra.Length > 0);

        var keyBox = Ui.Tabular(new TextBox { Text = key, IsSpellCheckEnabled = false, IsTextPredictionEnabled = false, FontSize = 12 });
        keyBox.TextChanged += (_, _) => { key = keyBox.Text; Enable(); };
        var secretBox = new PasswordBox { Password = secret, FontSize = 12 };
        secretBox.PasswordChanged += (_, _) => { secret = secretBox.Password; Enable(); };
        var p = Ui.V(8, Field(L(labels[0] ?? "API key"), keyBox), Field(L(labels[1] ?? "Secret"), secretBox));
        if (labels[2] is string x)
        {
            var extraBox = new PasswordBox { Password = extra, FontSize = 12 };
            extraBox.PasswordChanged += (_, _) => { extra = extraBox.Password; Enable(); };
            p.Children.Add(Field(L(x), extraBox));
        }
        var w = Ui.Icon("", 11);
        w.Foreground = T1.B(T1.Orange);
        p.Children.Add(Ui.H(5, w, Ui.Text(L("Never enable withdrawal permission."), 11, T1.Orange)));
        if (error != null)
        {
            var e = Ui.Text(error, 11, T1.Down);
            Wrap(e);
            p.Children.Add(e);
        }
        var cancel = new Button { Content = L("Cancel") };
        cancel.Click += (_, _) => { Close(); rebuild(); };
        save.Click += (_, _) => { Save(k); rebuild(); };
        Enable();
        var buttons = Ui.H(8, cancel, save);
        buttons.HorizontalAlignment = HorizontalAlignment.Right;
        p.Children.Add(buttons);
        return new Border { Child = p, Padding = new Thickness(12), CornerRadius = new CornerRadius(8), Background = T1.B(T1.Hl, 0.5), Margin = new Thickness(0, 4, 0, 4) };
    }

    static FrameworkElement Field(string label, FrameworkElement box) => Ui.V(3, Ui.Text(label, 11, T1.Mu), box);

    void Open(string ex) { Close(); editing = ex; }

    void Save(KeyStatus k)
    {
        var args = new JsonObject { ["ex"] = k.Ex, ["key"] = key.Trim(), ["secret"] = secret.Trim(), ["extra"] = extra.Trim() };
        bool ok = Store.Shared.Call("save_keys", args)?["ok"]?.GetValue<bool>() == true;
        args.Clear();
        if (ok)
        {
            testing.Add(k.Ex); // the engine tests a newly saved key right away
            Close();
        }
        else
        {
            error = Store.Shared.LastError ?? L("Failed");
            secret = "";
            extra = "";
        }
    }
}
