using System.Text.Json;
using System.Text.Json.Nodes;
using Microsoft.UI.Dispatching;

namespace Qh;

/// One tab of a project, as the snapshot describes it.
sealed class Tab
{
    public string Name = "", Purpose = "", Model = "", Status = "", Mode = "";
    public List<(string Role, string Text)> Messages = [];
    public string Title => Purpose.Length > 0 ? Purpose : Name;
    public bool Busy => Status is "thinking" or "running" or "busy";
}

/// A project: its `qh serve`, tabs and the current one. Like apple/qh/Project.swift.
sealed class Project
{
    public readonly string Path;
    public string Name => System.IO.Path.GetFileName(Path.TrimEnd('\\', '/'));
    public List<Tab> Tabs = [];
    public string? CurrentTab;
    public string State = "starting";
    public string Mode = "";
    public bool NeedsSetup;
    readonly ServeClient serve;
    public event Action? Changed;

    public Project(string path, string qh, DispatcherQueue ui)
    {
        Path = path;
        serve = new ServeClient(path, qh, ui) { OnEvent = Handle, OnClose = why => { State = "closed: " + why; Changed?.Invoke(); } };
        serve.Start();
    }

    public Tab? Current => Tabs.FirstOrDefault(t => t.Name == CurrentTab) ?? Tabs.FirstOrDefault();

    void Handle(JsonObject e)
    {
        switch ((string?)e["type"])
        {
            case "snapshot":
                State = "ready";
                Mode = (string?)e["mode"] ?? "";
                NeedsSetup = (bool?)e["needs_setup"] ?? false;
                Tabs = e["tabs"]?.AsArray().Select(t => ParseTab(t!.AsObject())).ToList() ?? [];
                break;
            case "tabs":
                // tab list without history: keep the messages we already have
                var old = Tabs.ToDictionary(t => t.Name);
                Tabs = e["tabs"]?.AsArray().Select(t =>
                {
                    var n = ParseTab(t!.AsObject());
                    if (n.Messages.Count == 0 && old.TryGetValue(n.Name, out var o)) n.Messages = o.Messages;
                    return n;
                }).ToList() ?? Tabs;
                Mode = (string?)e["mode"] ?? Mode;
                break;
            case "active":
                CurrentTab = (string?)e["tab"];
                break;
            case "turn_done":
                serve.Send(new JsonObject { ["type"] = "snapshot" });
                break;
            default:
                return;
        }
        Changed?.Invoke();
    }

    static Tab ParseTab(JsonObject t) => new()
    {
        Name = (string?)t["name"] ?? "",
        Purpose = (string?)t["purpose"] ?? "",
        Model = (string?)t["model"] ?? "",
        Status = t["status"]?.ToString() ?? "",
        Messages = t["messages"]?.AsArray().Select(m => ((string?)m!["role"] ?? "", (string?)m["text"] ?? "")).ToList() ?? [],
    };

    public void Prompt(string text)
    {
        if (Current is not { } t || text.Trim().Length == 0) return;
        serve.Send(new JsonObject { ["type"] = "prompt", ["tab"] = t.Name, ["text"] = text });
    }
    public void NewTab() => serve.Send(new JsonObject { ["type"] = "new_tab" });
    public void Stop() { if (Current is { } t) serve.Send(new JsonObject { ["type"] = "stop", ["tab"] = t.Name }); }
    public void Close() => serve.Stop();
}

/// The open projects and the remembered ones. Settings live in %LOCALAPPDATA%\qh\app.json
/// (an unpackaged app has no ApplicationData).
sealed class Workspace
{
    public List<string> Recent = [];
    public List<Project> Projects = [];
    public Project? Current;
    public readonly string? Qh = FindQh();
    readonly DispatcherQueue ui;
    public event Action? Changed;

    static readonly string Dir = System.IO.Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData), "qh");
    static readonly string SettingsFile = System.IO.Path.Combine(Dir, "app.json");

    public Workspace(DispatcherQueue ui)
    {
        this.ui = ui;
        try { Recent = JsonSerializer.Deserialize<Settings>(File.ReadAllText(SettingsFile))?.Projects ?? []; } catch { }
        Recent = Recent.Where(Directory.Exists).ToList();
        if (Recent.FirstOrDefault() is { } last) Open(last);
    }

    record Settings(List<string> Projects);

    void Save()
    {
        try { Directory.CreateDirectory(Dir); File.WriteAllText(SettingsFile, JsonSerializer.Serialize(new Settings(Recent))); } catch { }
    }

    public void Open(string path)
    {
        Recent.Remove(path);
        Recent.Insert(0, path);
        Save();
        Current = Projects.FirstOrDefault(p => p.Path == path);
        if (Current == null && Qh != null)
        {
            Current = new Project(path, Qh, ui);
            Current.Changed += () => Changed?.Invoke();
            Projects.Add(Current);
        }
        Changed?.Invoke();
    }

    public void Forget(string path)
    {
        Recent.Remove(path);
        if (Projects.FirstOrDefault(p => p.Path == path) is { } p) { p.Close(); Projects.Remove(p); }
        if (Current?.Path == path) Current = Projects.FirstOrDefault();
        Save();
        Changed?.Invoke();
    }

    public void Shutdown() { foreach (var p in Projects) p.Close(); }

    /// qh's installer puts it in %LOCALAPPDATA%\qh\bin; else whatever PATH has.
    static string? FindQh()
    {
        var own = System.IO.Path.Combine(Dir, "bin", "qh.exe");
        if (File.Exists(own)) return own;
        foreach (var d in (Environment.GetEnvironmentVariable("PATH") ?? "").Split(';'))
        {
            var c = System.IO.Path.Combine(d.Trim(), "qh.exe");
            if (d.Length > 0 && File.Exists(c)) return c;
        }
        return null;
    }
}
