using Microsoft.UI.Dispatching;
using Microsoft.Windows.AppNotifications;
using Microsoft.Windows.AppNotifications.Builder;

namespace Qh;

/// Toasts for permissions when the window isn't in front, with Allow once / Deny right on the toast,
/// like apple/qh/Notify.swift. Clicking the toast itself brings the window up on that tab.
static class Notify
{
    static bool ok;
    static Workspace? ws;
    static Action? focus;

    public static void Start(Workspace w, DispatcherQueue ui, Action bringToFront)
    {
        ws = w;
        focus = bringToFront;
        try
        {
            AppNotificationManager.Default.NotificationInvoked += (_, a) => ui.TryEnqueue(() => Invoked(a.Arguments));
            AppNotificationManager.Default.Register();
            ok = true;
        }
        catch { ok = false; } // toasts off (e.g. notifications disabled): the in-window card still works
    }

    public static void Stop() { if (ok) try { AppNotificationManager.Default.Unregister(); } catch { } }

    public static void Ask(Project p, Ask a)
    {
        if (!ok) return;
        var key = $"{p.Path}|{a.Id}";
        var b = new AppNotificationBuilder()
            .AddArgument("ask", key)
            .AddText($"{p.Name}: qh wants to {a.Tool}")
            .AddText(a.Detail.Length > 180 ? a.Detail[..180] + "…" : a.Detail)
            .AddButton(new AppNotificationButton("Allow once").AddArgument("ask", key).AddArgument("choice", "yes"))
            .AddButton(new AppNotificationButton("Deny").AddArgument("ask", key).AddArgument("choice", "no"))
            .SetTag(a.Id.ToString()).SetGroup(Group(p));
        try { AppNotificationManager.Default.Show(b.BuildNotification()); } catch { }
    }

    public static void Clear(Project p, long id)
    {
        if (ok) _ = AppNotificationManager.Default.RemoveByTagAndGroupAsync(id.ToString(), Group(p));
    }

    static string Group(Project p) => ((uint)p.Path.GetHashCode()).ToString();

    static void Invoked(IDictionary<string, string> args)
    {
        if (ws == null || !args.TryGetValue("ask", out var key)) { focus?.Invoke(); return; }
        var bar = key.LastIndexOf('|');
        var (path, id) = (key[..bar], long.Parse(key[(bar + 1)..]));
        var p = ws.Projects.FirstOrDefault(x => x.Path == path);
        var a = p?.Asks.FirstOrDefault(x => x.Id == id);
        if (p == null || a == null) { focus?.Invoke(); return; }
        if (args.TryGetValue("choice", out var c)) p.Answer(a, c);
        else { p.CurrentTab = a.Tab; ws.Open(p.Path); focus?.Invoke(); }
    }
}
