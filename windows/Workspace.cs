using System.Text.Json;
using System.Text.Json.Nodes;
using Microsoft.UI.Dispatching;

namespace Qh;

/// One thing in a thread, like apple/qh/QhTab.swift's Item.
sealed class Item
{
    public enum Kind { User, Reply, Tool, Diff, Notice, Error, Verify }
    public Kind K;
    public string Text = "";                 // user/reply/notice/error text; tool args; diff path
    public string Name = "", Summary = "", Output = "";
    public bool? Ok;                         // tool: null while it runs
    public int Added, Removed;
    public double Secs;                      // verify: how long the check took
}

/// One tab of a project: what the snapshot says plus its thread.
sealed class Tab
{
    public string Name = "", Purpose = "", Model = "", Provider = "", Status = "";
    public List<Item> Items = [];
    public int Tokens;
    public double? Cost;
    public List<(string Text, string Status)> Plan = [];
    public bool PlanReady;                   // plan mode finished a turn: the plan waits for approval
    public string Title => Purpose.Length > 0 ? Purpose : Name;
    public bool Busy => Status is not ("" or "idle" or "done" or "ready");
}

/// A permission qh is waiting on.
sealed record Ask(long Id, string Tab, string Tool, string Detail, string Warn, bool Guarded, string Always, JsonObject? Args = null);

/// A saved session for the sidebar (serve's "sessions").
sealed record SavedSession(string Id, string Title, int Messages, DateTimeOffset When);

/// One line of the activity log: a command, an edit, a network ask, an approval.
sealed record ActivityEntry(DateTimeOffset T, string Kind, string Text, bool Ok)
{
    public string Group => Kind switch { "cmd" => "Commands", "edit" => "Edits", "net" => "Network", "ask" => "Approvals", _ => "Other" };
}

/// An inline edit in the code view: the selected lines, the instruction, the streamed answer.
sealed class InlineEdit
{
    public long? Id;
    public string Path = "";
    public int Start, End;
    public string Instruction = "", Preview = "", Before = "", After = "";
    public bool Sent, Done;
    public string? Error;
}

/// A connect-a-model choice on the setup screen.
sealed record Source(string Id, string Label, string Kind, string Note, string Site, bool Connected, string Account);

/// A project: its `qh serve`, tabs, threads and asks. Like apple/qh/Project.swift.
sealed class Project
{
    public readonly string Path;
    public string Name => System.IO.Path.GetFileName(Path.TrimEnd('\\', '/'));
    public List<Tab> Tabs = [];
    public string? CurrentTab;
    public string State = "starting";
    public string Mode = "";
    public List<Ask> Asks = [];
    public List<(string Cmd, string About)> Commands = [];
    public List<string> Files = [];
    // W3: review, checkpoints, code
    public List<(string Path, string Before, string After)> Changes = [];
    public List<(int Turn, string Prompt, List<string> Files)> Checkpoints = [];
    public string Layout = "chat";          // chat | split | code
    public string? Page;                    // null = thread, "review", "history"
    public (string Path, int? Line, string Text)? Code;
    public string Draft = "";               // a rewind puts the prompt back here
    // saved sessions, memory, activity, inline edits
    public List<SavedSession> Sessions = [];
    public Dictionary<string, string> OpenSessions = [];   // session id → tab showing it
    public string MemoryPath = "", MemoryText = "";
    public int MemoryVersion;
    public List<(string Path, string Text)> Instructions = [];
    public List<ActivityEntry> Activity = [];
    public (int Start, int End)? Selection;     // code view: picked lines, 1-based
    public InlineEdit? Inline;
    /// serve pushes team changes itself: poll slowly
    public bool TeamPush;
    // W4: team
    public bool TeamRunning;
    public string TeamMe = "", TeamStatus = "", Chan = "#team";
    public List<(string Login, bool Online, string Status)> People = [];
    public List<(string Id, string Label, int Unread, bool Pinged)> Channels = [];
    public List<(string Who, string When, string Text)> TeamMessages = [];
    public List<(string Login, string Name, string Status, string Now, bool Waiting)> Agents = [];
    public List<(string From, string Text, bool Question)> Waiting = [];
    /// asks that arrived while the window wasn't looking: the toast/tray hooks
    public event Action<Project, Ask>? AskArrived;
    // setup screen
    public bool ShowSetup;
    public List<Source> Sources = [];
    public List<(string Id, string From)> Models = [];
    public string DefaultModel = "", SetupMessage = "";
    public bool SetupBusy, SetupError;
    readonly ServeClient serve;
    readonly DispatcherQueue ui;
    bool inlineQueued;
    /// the whole project changed (tabs, asks, setup): redraw everything
    public event Action? Changed;
    /// one item was added or changed in a tab's thread: redraw just that
    public event Action<Tab, Item, bool>? ItemChanged;

    public Project(string path, string qh, DispatcherQueue ui)
    {
        Path = path;
        this.ui = ui;
        serve = new ServeClient(path, qh, ui) { OnEvent = Handle, OnClose = why => { State = "closed: " + why; Changed?.Invoke(); } };
        serve.Start();
    }

    public Tab? Current => Tabs.FirstOrDefault(t => t.Name == CurrentTab) ?? Tabs.FirstOrDefault();

    Tab TabNamed(string? n)
    {
        n ??= Current?.Name ?? "main";
        var t = Tabs.FirstOrDefault(x => x.Name == n);
        if (t == null) { t = new Tab { Name = n }; Tabs.Add(t); }
        return t;
    }

    void Add(Tab t, Item i) { t.Items.Add(i); ItemChanged?.Invoke(t, i, true); }

    void Handle(JsonObject e)
    {
        var tabName = (string?)e["tab"];
        switch ((string?)e["type"])
        {
            case "snapshot":
                State = "ready";
                Mode = (string?)e["mode"] ?? "";
                Tabs = [];
                foreach (var t in e["tabs"]?.AsArray() ?? []) Fill(t!.AsObject());
                if ((bool?)e["needs_setup"] == true) { ShowSetup = true; LoadSetup(); }
                if ((bool?)e["team_push"] == true) TeamPush = true;
                Send("sessions");
                Send("commands");
                Send("files");
                Send("changes");
                Send("checkpoints");
                Send("team");
                break;
            case "tabs":
                Mode = (string?)e["mode"] ?? Mode;
                var list = e["tabs"]?.AsArray().Select(x => x!.AsObject()).ToList() ?? [];
                foreach (var t in list) Update(TabNamed((string?)t["name"]), t);
                var names = list.Select(t => (string?)t["name"]).ToHashSet();
                Tabs.RemoveAll(t => !names.Contains(t.Name));
                break;
            case "active":
                CurrentTab = tabName;
                break;
            case "text":
            {
                var t = TabNamed(tabName);
                var delta = (string?)e["delta"] ?? "";
                if (t.Items.LastOrDefault() is { K: Item.Kind.Reply } last) { last.Text += delta; ItemChanged?.Invoke(t, last, false); }
                else Add(t, new Item { K = Item.Kind.Reply, Text = delta });
                return;
            }
            case "tool_call":
                Add(TabNamed(tabName), new Item { K = Item.Kind.Tool, Name = (string?)e["name"] ?? "", Text = Brief(e["args"]) });
                return;
            case "tool_result":
            {
                var t = TabNamed(tabName);
                if (t.Items.LastOrDefault(i => i.K == Item.Kind.Tool && i.Ok == null) is { } it)
                {
                    it.Ok = (bool?)e["ok"] ?? true;
                    it.Summary = (string?)e["summary"] ?? "";
                    it.Output = (string?)e["output"] ?? "";
                    ItemChanged?.Invoke(t, it, false);
                }
                return;
            }
            case "diff":
                Add(TabNamed(tabName), new Item { K = Item.Kind.Diff, Text = (string?)e["path"] ?? "", Added = (int?)e["added"] ?? 0, Removed = (int?)e["removed"] ?? 0 });
                Send("changes");
                return;
            case "notice":
            case "compact":
                Add(TabNamed(tabName), new Item { K = Item.Kind.Notice, Text = (string?)e["message"] ?? "compacted the conversation" });
                return;
            case "error":
                if ((string?)e["re"] == "inline_edit" && Inline != null) { Inline.Error = (string?)e["message"] ?? "error"; break; }
                Add(TabNamed(e["re"] != null ? null : tabName), new Item { K = Item.Kind.Error, Text = (string?)e["message"] ?? "error" });
                return;
            case "usage":
                TabNamed(tabName).Cost = (double?)e["cost"];
                break;
            case "turn_done":
                TabNamed(tabName).Tokens = (int?)e["tokens"] ?? 0;
                Send("checkpoints");
                Send("sessions");
                break;
            case "confirm":
            {
                var a = new Ask((long?)e["id"] ?? -1, tabName ?? "", (string?)e["tool"] ?? "", Detail(e["args"]),
                    (string?)e["warn"] ?? "", (bool?)e["guard"] ?? false, (string?)e["always"] ?? "", e["args"] as JsonObject);
                Asks.Add(a);
                AskArrived?.Invoke(this, a);
                break;
            }
            case "team":
                if ((bool?)e["team_push"] == true) TeamPush = true;
                TeamRunning = (bool?)e["running"] ?? false;
                TeamMe = (string?)e["me"] ?? "";
                TeamStatus = (string?)e["status"] ?? "";
                Chan = (string?)e["chan"] ?? Chan;
                People = e["people"]?.AsArray().Select(x => ((string?)x!["login"] ?? "", (bool?)x["online"] ?? false, (string?)x["status"] ?? "")).ToList() ?? [];
                Channels = e["channels"]?.AsArray().Select(x => ((string?)x!["id"] ?? "", (string?)x["label"] ?? "", (int?)x["unread"] ?? 0, (bool?)x["pinged"] ?? false)).ToList() ?? [];
                TeamMessages = e["messages"]?.AsArray().Select(x => ((string?)x!["who"] ?? "", x["when"]?.ToString() ?? "", (string?)x["text"] ?? "")).ToList() ?? [];
                Agents = e["agents"]?.AsArray().Select(x => ((string?)x!["login"] ?? "", (string?)x["name"] ?? "", (string?)x["status"] ?? "", (string?)x["now"] ?? "", (bool?)x["waiting"] ?? false)).ToList() ?? [];
                Waiting = e["waiting"]?.AsArray().Select(x => ((string?)x!["from"] ?? "", (string?)x["text"] ?? "", (bool?)x["question"] ?? false)).ToList() ?? [];
                break;
            case "confirm_closed":
                Asks.RemoveAll(a => a.Id == ((long?)e["id"] ?? -1));
                break;
            case "changes":
                Changes = e["files"]?.AsArray().Select(f => ((string?)f!["path"] ?? "", (string?)f["before"] ?? "", (string?)f["after"] ?? "")).ToList() ?? [];
                break;
            case "checkpoints":
                Checkpoints = e["items"]?.AsArray().Select(c => ((int?)c!["turn"] ?? 0, (string?)c["prompt"] ?? "",
                    c["files"]?.AsArray().Select(f => (string?)f ?? "").ToList() ?? [])).ToList() ?? [];
                break;
            case "history":
                Fill(e);
                Send("sessions");
                break;
            case "sessions":
                Sessions = e["sessions"]?.AsArray().Select(x => new SavedSession((string?)x!["id"] ?? "", (string?)x["title"] ?? "",
                    (int?)x["messages"] ?? 0, DateTimeOffset.FromUnixTimeSeconds((long?)x["when"] ?? 0).ToLocalTime())).ToList() ?? [];
                OpenSessions = [];
                foreach (var o in e["open"]?.AsArray() ?? [])
                    if ((string?)o!["id"] is { } sid && (string?)o["tab"] is { } st) OpenSessions[sid] = st;
                break;
            case "verify":
                Add(TabNamed(tabName), new Item { K = Item.Kind.Verify, Name = (string?)e["cmd"] ?? "", Ok = (bool?)e["ok"] ?? false,
                    Secs = (double?)e["secs"] ?? 0, Output = (string?)e["tail"] ?? "" });
                return;
            case "plan":
            {
                var t = TabNamed(tabName);
                t.Plan = e["items"]?.AsArray().Select(x => ((string?)x!["text"] ?? "", (string?)x["status"] ?? "pending")).ToList() ?? [];
                if (t.Plan.Count == 0) t.PlanReady = false;
                break;
            }
            case "plan_ready":
                TabNamed(tabName).PlanReady = true;
                break;
            case "memory":
                MemoryPath = (string?)e["path"] ?? "";
                MemoryText = (string?)e["text"] ?? "";
                Instructions = e["instructions"]?.AsArray().Select(x => ((string?)x!["path"] ?? "", (string?)x["text"] ?? "")).ToList() ?? [];
                MemoryVersion++;
                break;
            case "activity":
                if (tabName != (Current?.Name ?? "main")) return;
                Activity = e["items"]?.AsArray().Select(x =>
                {
                    var raw = (double?)x!["t"] ?? 0;
                    return new ActivityEntry(DateTimeOffset.FromUnixTimeMilliseconds((long)(raw > 1e12 ? raw : raw * 1000)).ToLocalTime(),
                        (string?)x["kind"] ?? "", (string?)x["text"] ?? "", (bool?)x["ok"] ?? true);
                }).Reverse().ToList() ?? [];
                break;
            case "inline_delta":
                if (Inline is { } il && (il.Id == null || il.Id == (long?)e["id"]))
                {
                    il.Id = (long?)e["id"];
                    il.Preview += (string?)e["delta"] ?? "";
                    // a streamed preview redraws at most every 80 ms
                    if (!inlineQueued)
                    {
                        inlineQueued = true;
                        var tm = ui.CreateTimer();
                        tm.Interval = TimeSpan.FromMilliseconds(80);
                        tm.IsRepeating = false;
                        tm.Tick += (_, _) => { inlineQueued = false; Changed?.Invoke(); };
                        tm.Start();
                    }
                }
                return;
            case "inline_result":
                if (Inline is { } ir)
                {
                    ir.Id = (long?)e["id"];
                    ir.Before = (string?)e["before"] ?? "";
                    ir.After = (string?)e["after"] ?? "";
                    ir.Done = true;
                }
                break;
            case "inline_applied":
                Add(TabNamed(tabName), new Item { K = Item.Kind.Notice, Text = "inline edit kept: " + ((string?)e["path"] ?? "") });
                if (Code is { } c && c.Path == (string?)e["path"]) OpenFile(c.Path, Inline?.Start);
                Inline = null;
                Selection = null;
                break;
            case "rewound":
                Draft = (string?)e["prompt"] ?? "";
                Add(TabNamed(tabName), new Item { K = Item.Kind.Notice, Text = $"rewound to before message {(int?)e["turn"] + 1}" });
                break;
            case "commands":
                Commands = e["commands"]?.AsArray().Where(c => c?["sub"] == null)
                    .Select(c => ((string?)c!["cmd"] ?? "", (string?)c["about"] ?? "")).ToList() ?? [];
                return;
            case "files":
                Files = e["files"]?.AsArray().Select(f => (string?)f ?? "").ToList() ?? [];
                return;
            case "setup":
                Sources = e["sources"]?.AsArray().Select(x => new Source((string?)x!["id"] ?? "", (string?)x["label"] ?? "", (string?)x["kind"] ?? "",
                    (string?)x["note"] ?? "", (string?)x["site"] ?? "", (bool?)x["connected"] ?? false, (string?)x["account"] ?? "")).ToList() ?? [];
                DefaultModel = (string?)e["default"] ?? "";
                if ((bool?)e["needed"] == true) ShowSetup = true;
                break;
            case "setup_progress":
                SetupMessage = (string?)e["message"] ?? "";
                break;
            case "setup_result":
            {
                SetupBusy = false;
                var ok = (bool?)e["ok"] ?? false;
                SetupError = !ok;
                var step = (string?)e["step"] ?? "";
                var data = e["data"]?.AsObject();
                if (!ok) SetupMessage = (string?)e["error"] ?? "that didn't work";
                else if (step == "models")
                {
                    Models = data?["models"]?.AsArray().Select(m => ((string?)m!["id"] ?? "", (string?)m["from"] ?? "")).ToList() ?? [];
                    if (Models.Count > 0 && SetupMessage.Length == 0) SetupMessage = "pick a default model below";
                }
                else if (step == "default") { SetupMessage = $"default model: {(string?)data?["model"]}"; ShowSetup = false; }
                else
                {
                    SetupMessage = step == "chatgpt" ? $"signed in as {(string?)data?["account"]}" : $"connected: {(int?)data?["models"] ?? 0} models";
                    Send("setup_models");
                }
                break;
            }
            default:
                return;
        }
        Changed?.Invoke();
    }

    void Fill(JsonObject t)
    {
        var tab = TabNamed((string?)t["name"]);
        Update(tab, t);
        tab.Items = [];
        foreach (var m in t["messages"]?.AsArray() ?? [])
        {
            var text = (string?)m!["text"] ?? "";
            if ((string?)m["role"] == "user") { if (text.Length > 0) tab.Items.Add(new Item { K = Item.Kind.User, Text = text }); continue; }
            if (text.Length > 0) tab.Items.Add(new Item { K = Item.Kind.Reply, Text = text });
            foreach (var n in m["tools"]?.AsArray() ?? []) tab.Items.Add(new Item { K = Item.Kind.Tool, Name = (string?)n ?? "", Ok = true });
        }
    }

    static void Update(Tab tab, JsonObject t)
    {
        tab.Purpose = (string?)t["purpose"] ?? tab.Purpose;
        tab.Model = (string?)t["model"] ?? tab.Model;
        tab.Provider = (string?)t["provider"] ?? tab.Provider;
        tab.Status = t["status"]?.ToString() ?? tab.Status;
        tab.Tokens = (int?)t["tokens"] ?? tab.Tokens;
    }

    /// One line for a tool call: its command, path or query.
    static string Brief(JsonNode? a)
    {
        if (a is not JsonObject d) return "";
        foreach (var k in new[] { "cmd", "path", "pattern", "query", "url", "host", "name" })
            if (d[k] is JsonValue v && v.TryGetValue<string>(out var s)) return s;
        return d.ToJsonString();
    }

    /// What an approval card shows in full: the command, or the edit.
    static string Detail(JsonNode? a)
    {
        if (a is not JsonObject d) return "";
        if ((string?)d["cmd"] is { } c) return c;
        if ((string?)d["path"] is { } p)
        {
            if ((string?)d["old"] is { } o && (string?)d["new"] is { } n)
                return string.Join("\n", new[] { p }.Concat(o.Split('\n').Select(l => "- " + l)).Concat(n.Split('\n').Select(l => "+ " + l)));
            if ((string?)d["content"] is { } body) return p + "\n" + string.Join("\n", body.Split('\n').Take(40).Select(l => "+ " + l));
            return p;
        }
        return Brief(d);
    }

    void Send(string type, JsonObject? more = null)
    {
        var m = more ?? new JsonObject();
        m["type"] = type;
        if (Current is { } t && m["tab"] == null) m["tab"] = t.Name;
        serve.Send(m);
    }

    /// images: base64 of the file bytes, what qh's own /image and Ctrl+V attach (serve's "images")
    public void Prompt(string text, IReadOnlyList<string>? images = null)
    {
        var n = images?.Count ?? 0;
        if (Current is not { } t || (text.Trim().Length == 0 && n == 0)) return;
        text = text.Trim();
        if (text.StartsWith('/')) Add(t, new Item { K = Item.Kind.Notice, Text = "› " + text });
        else { Add(t, new Item { K = Item.Kind.User, Text = text.Length > 0 ? text + (n > 0 ? $"\n+ {n} image{(n > 1 ? "s" : "")}" : "") : $"+ {n} image{(n > 1 ? "s" : "")}" }); t.Status = "working"; }
        var msg = new JsonObject { ["text"] = text };
        if (n > 0) msg["images"] = new JsonArray(images!.Select(i => (JsonNode?)JsonValue.Create(i)).ToArray());
        Send("prompt", msg);
        Changed?.Invoke();
    }
    public void Answer(Ask a, string choice)
    {
        Asks.Remove(a);
        serve.Send(new JsonObject { ["type"] = "answer", ["id"] = a.Id, ["answer"] = choice });
        Changed?.Invoke();
    }
    public void NewTab() => Send("new_tab");
    public void Accept(string path) => Send("accept", new JsonObject { ["path"] = path });
    public void Reject(string path) => Send("reject", new JsonObject { ["path"] = path });
    public void Rewind(int turn, bool chat) => Send("rewind", new JsonObject { ["turn"] = turn, ["chat"] = chat });
    public void LoadChanges() => Send("changes");
    public void LoadTeam(string? chan = null) => Send("team", chan == null ? null : new JsonObject { ["chan"] = chan });
    public void TeamSend(string text) => Send("team_send", new JsonObject { ["chan"] = Chan, ["text"] = text });
    public bool Waits => Asks.Count > 0;
    public void LoadCheckpoints() => Send("checkpoints");
    public void LoadSessions() => Send("sessions");
    public void Resume(string id) => Send("resume", new JsonObject { ["id"] = id });
    public void FixVerify() => Send("fix_verify");
    public void ApprovePlan(string edits)
    {
        if (Current is { } t) { t.PlanReady = false; Send("plan_approve", new JsonObject { ["edits"] = edits }); Changed?.Invoke(); }
    }
    public void LoadMemory() => Send("memory");
    public void SaveMemory(string text) => Send("memory_save", new JsonObject { ["text"] = text });
    public void LoadActivity() => Send("activity");
    public void InlineAsk(string instruction)
    {
        if (Inline is not { } il || instruction.Trim().Length == 0) return;
        il.Instruction = instruction; il.Sent = true; il.Preview = ""; il.Error = null;
        Send("inline_edit", new JsonObject { ["path"] = il.Path, ["start"] = il.Start, ["end"] = il.End, ["instruction"] = instruction });
        Changed?.Invoke();
    }
    public void InlineApply() { if (Inline?.Id is { } id) Send("inline_apply", new JsonObject { ["id"] = id }); }
    public void InlineDiscard()
    {
        if (Inline?.Id is { } id) Send("inline_discard", new JsonObject { ["id"] = id });
        Inline = null;
        Changed?.Invoke();
    }

    /// A path from the thread or the tree: read it here (the app sits on the same disk) and show it.
    public bool OpenFile(string rel, int? line = null)
    {
        var colon = rel.LastIndexOf(':');
        if (line == null && colon > 1 && int.TryParse(rel[(colon + 1)..], out var n)) { line = n; rel = rel[..colon]; }
        var full = System.IO.Path.GetFullPath(System.IO.Path.Combine(Path, rel));
        if (!full.StartsWith(Path, StringComparison.OrdinalIgnoreCase) || !File.Exists(full)) return false;
        string text;
        try { text = new FileInfo(full).Length > 2_000_000 ? "(file too big to show)" : File.ReadAllText(full); } catch (Exception e) { text = e.Message; }
        var np = rel.Replace('\\', '/');
        if (Code?.Path != np) { Selection = null; Inline = null; }
        Code = (np, line, text);
        if (Layout == "chat") Layout = "split";
        Changed?.Invoke();
        return true;
    }
    public void Stop() => Send("stop");
    public static readonly string[] Modes = ["ask", "auto-edit", "auto", "plan"];
    public void SetMode(string mode) => Send("mode", new JsonObject { ["mode"] = mode });
    public void LoadSetup() { Send("setup_state"); Send("setup_models"); }
    public void ConnectKey(string provider, string key) { SetupBusy = true; SetupMessage = "checking the key…"; Send("setup_key", new JsonObject { ["provider"] = provider, ["key"] = key }); Changed?.Invoke(); }
    public void SignInChatGpt() { SetupBusy = true; SetupMessage = "opening the sign-in page…"; Send("setup_chatgpt"); Changed?.Invoke(); }
    public void SetDefault(string model) { SetupBusy = true; Send("setup_default", new JsonObject { ["model"] = model }); Changed?.Invoke(); }
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
    public event Action<Tab, Item, bool>? ItemChanged;
    public event Action<Project, Ask>? AskArrived;
    public string Space = "personal";       // personal | team

    static readonly string Dir = System.IO.Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData), "qh");
    static readonly string SettingsFile = System.IO.Path.Combine(Dir, "app.json");

    public Workspace(DispatcherQueue ui)
    {
        this.ui = ui;
        try
        {
            var s = JsonSerializer.Deserialize<Settings>(File.ReadAllText(SettingsFile));
            Recent = s?.Projects ?? [];
            compact = s?.Compact;
            ThemePref = s?.Theme ?? "system";
            DefaultLayout = s?.Layout ?? "chat";
            Reopen = s?.Reopen ?? true;
        }
        catch { }
        Recent = Recent.Where(Directory.Exists).ToList();
        if (Reopen && Recent.FirstOrDefault() is { } last) Open(last);
    }

    bool? compact;
    /// Compact = the user's explicit choice; null = follow the window width
    public bool? Compact { get => compact; set { compact = value; Save(); } }

    // app settings (the gear): appearance, the layout new projects open in, reopen the last project
    public string ThemePref = "system";     // system | light | dark
    public string DefaultLayout = "chat";   // chat | split
    public bool Reopen = true;

    record Settings(List<string> Projects, bool? Compact = null, string? Theme = null, string? Layout = null, bool? Reopen = null);

    public void Save()
    {
        try { Directory.CreateDirectory(Dir); File.WriteAllText(SettingsFile, JsonSerializer.Serialize(new Settings(Recent, compact, ThemePref, DefaultLayout, Reopen))); } catch { }
    }

    public void Open(string path)
    {
        Recent.Remove(path);
        Recent.Insert(0, path);
        Save();
        Current = Projects.FirstOrDefault(p => p.Path == path);
        if (Current == null && Qh != null)
        {
            Current = new Project(path, Qh, ui) { Layout = DefaultLayout };
            Current.Changed += () => Changed?.Invoke();
            Current.ItemChanged += (t, i, a) => ItemChanged?.Invoke(t, i, a);
            Current.AskArrived += (p, a) => AskArrived?.Invoke(p, a);
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
