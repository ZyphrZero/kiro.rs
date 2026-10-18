// KiroTray: 在后台无窗口运行 kiro-rs.exe，并提供托盘图标与菜单。
// 编译: build.cmd（使用 Windows 自带的 .NET Framework csc.exe，仅支持 C# 5 语法）
using System;
using System.Collections.Generic;
using System.Diagnostics;
using System.Drawing;
using System.IO;
using System.Net.Sockets;
using System.Runtime.InteropServices;
using System.Text;
using System.Text.RegularExpressions;
using System.Threading;
using System.Windows.Forms;
using Microsoft.Win32;

static class Program
{
    [STAThread]
    static void Main()
    {
        bool created;
        using (var mutex = new Mutex(true, "Local\\KiroTray_SingleInstance", out created))
        {
            if (!created)
            {
                MessageBox.Show("KiroTray 已在运行，请查看任务栏右下角托盘。", "KiroTray",
                    MessageBoxButtons.OK, MessageBoxIcon.Information);
                return;
            }
            Application.EnableVisualStyles();
            Application.SetCompatibleTextRenderingDefault(false);
            Application.Run(new TrayContext());
        }
    }
}

class Settings
{
    public string KiroDir = Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.UserProfile), "kiro-rs");
    public string ExeName = "kiro-rs.exe";
    public bool AutoRestart = true;
    public int LogMaxMB = 10;

    readonly string path;

    public Settings(string path)
    {
        this.path = path;
        if (!File.Exists(path)) { Save(); return; }
        foreach (var raw in File.ReadAllLines(path, Encoding.UTF8))
        {
            var line = raw.Trim();
            if (line.Length == 0 || line.StartsWith("#") || line.StartsWith(";")) continue;
            int i = line.IndexOf('=');
            if (i <= 0) continue;
            string k = line.Substring(0, i).Trim(), v = line.Substring(i + 1).Trim();
            int n;
            switch (k)
            {
                case "KiroDir": KiroDir = v; break;
                case "ExeName": ExeName = v; break;
                case "AutoRestart": AutoRestart = v == "1" || v.Equals("true", StringComparison.OrdinalIgnoreCase); break;
                case "LogMaxMB": if (int.TryParse(v, out n) && n > 0) LogMaxMB = n; break;
            }
        }
    }

    public void Save()
    {
        var sb = new StringBuilder();
        sb.AppendLine("# KiroTray 配置。升级 kiro-rs 时只需改 KiroDir 指向新目录。");
        sb.AppendLine("KiroDir=" + KiroDir);
        sb.AppendLine("ExeName=" + ExeName);
        sb.AppendLine("AutoRestart=" + (AutoRestart ? "1" : "0"));
        sb.AppendLine("LogMaxMB=" + LogMaxMB);
        File.WriteAllText(path, sb.ToString(), new UTF8Encoding(false));
    }
}

// 用 Job Object 绑定子进程：托盘进程无论怎么退出（含被任务管理器结束），kiro-rs 都会一起结束。
static class Job
{
    [DllImport("kernel32.dll", CharSet = CharSet.Unicode)]
    static extern IntPtr CreateJobObject(IntPtr a, string name);
    [DllImport("kernel32.dll")]
    static extern bool SetInformationJobObject(IntPtr job, int cls, IntPtr info, uint len);
    [DllImport("kernel32.dll")]
    static extern bool AssignProcessToJobObject(IntPtr job, IntPtr proc);

    [StructLayout(LayoutKind.Sequential)]
    struct BASIC { public long a, b; public uint LimitFlags; public UIntPtr c, d; public uint e; public UIntPtr f; public uint g, h; }
    [StructLayout(LayoutKind.Sequential)]
    struct IOC { public ulong a, b, c, d, e, f; }
    [StructLayout(LayoutKind.Sequential)]
    struct EXT { public BASIC Basic; public IOC Io; public UIntPtr p1, p2, p3, p4; }

    static IntPtr handle = IntPtr.Zero;

    public static void Add(Process p)
    {
        try
        {
            if (handle == IntPtr.Zero)
            {
                handle = CreateJobObject(IntPtr.Zero, null);
                var info = new EXT();
                info.Basic.LimitFlags = 0x2000; // JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
                int len = Marshal.SizeOf(typeof(EXT));
                IntPtr ptr = Marshal.AllocHGlobal(len);
                Marshal.StructureToPtr(info, ptr, false);
                SetInformationJobObject(handle, 9, ptr, (uint)len);
                Marshal.FreeHGlobal(ptr);
            }
            AssignProcessToJobObject(handle, p.Handle);
        }
        catch { }
    }
}

// 查找并激活 Chrome 应用模式（--app）打开的管理后台窗口
static class AdminWindow
{
    const string Title = "Kiro Admin";

    delegate bool EnumProc(IntPtr hwnd, IntPtr lParam);
    [DllImport("user32.dll")] static extern bool EnumWindows(EnumProc cb, IntPtr lParam);
    [DllImport("user32.dll")] static extern bool IsWindowVisible(IntPtr hwnd);
    [DllImport("user32.dll")] static extern bool IsIconic(IntPtr hwnd);
    [DllImport("user32.dll")] static extern bool IsZoomed(IntPtr hwnd);
    [DllImport("user32.dll")] static extern bool ShowWindow(IntPtr hwnd, int cmd);
    [DllImport("user32.dll")] static extern bool SetForegroundWindow(IntPtr hwnd);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] static extern int GetWindowText(IntPtr hwnd, StringBuilder sb, int max);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] static extern int GetClassName(IntPtr hwnd, StringBuilder sb, int max);

    // 应用模式窗口的标题就是页面标题；普通浏览器窗口标题会带 " - Google Chrome" 后缀，不会误匹配。
    // 页面还没加载完或加载失败时，标题是 "127.0.0.1:8990/admin" 这样的地址，也一并认作该窗口。
    public static IntPtr Find(string hostPort)
    {
        IntPtr found = IntPtr.Zero;
        EnumWindows(delegate(IntPtr h, IntPtr l)
        {
            if (!IsWindowVisible(h)) return true;
            var cls = new StringBuilder(64);
            GetClassName(h, cls, cls.Capacity);
            if (cls.ToString() != "Chrome_WidgetWin_1") return true;
            var sb = new StringBuilder(256);
            GetWindowText(h, sb, sb.Capacity);
            string t = sb.ToString();
            if (t == Title || t.StartsWith(hostPort + "/admin") || t.StartsWith("http://" + hostPort + "/admin"))
            {
                found = h;
                return false;
            }
            return true;
        }, IntPtr.Zero);
        return found;
    }

    public static void Activate(IntPtr hwnd)
    {
        ShowWindow(hwnd, IsIconic(hwnd) ? 9 : 5); // SW_RESTORE : SW_SHOW
        SetForegroundWindow(hwnd);
    }

    public static void Maximize(IntPtr hwnd)
    {
        if (IsZoomed(hwnd)) return;
        ShowWindow(hwnd, 3); // SW_MAXIMIZE
        SetForegroundWindow(hwnd);
    }

    public static string ChromePath()
    {
        const string key = @"SOFTWARE\Microsoft\Windows\CurrentVersion\App Paths\chrome.exe";
        foreach (var root in new[] { Registry.CurrentUser, Registry.LocalMachine })
        {
            try
            {
                using (var k = root.OpenSubKey(key))
                {
                    var v = k == null ? null : k.GetValue(null) as string;
                    if (!string.IsNullOrEmpty(v) && File.Exists(v.Trim('"'))) return v.Trim('"');
                }
            }
            catch { }
        }
        string[] dirs =
        {
            Environment.GetFolderPath(Environment.SpecialFolder.ProgramFiles),
            Environment.GetEnvironmentVariable("ProgramFiles(x86)"),
            Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData)
        };
        foreach (var d in dirs)
        {
            if (string.IsNullOrEmpty(d)) continue;
            string p = Path.Combine(d, @"Google\Chrome\Application\chrome.exe");
            if (File.Exists(p)) return p;
        }
        return null;
    }
}

class TrayContext : ApplicationContext
{
    const string RunKey = @"Software\Microsoft\Windows\CurrentVersion\Run";
    const string RunName = "KiroTray";
    static readonly Regex Ansi = new Regex(@"\x1B\[[0-9;?]*[ -/]*[@-~]", RegexOptions.Compiled);

    readonly string baseDir = AppDomain.CurrentDomain.BaseDirectory;
    readonly Settings cfg;
    readonly NotifyIcon tray = new NotifyIcon();
    readonly Icon iconOk, iconBad, iconWait;
    readonly System.Windows.Forms.Timer health = new System.Windows.Forms.Timer();
    readonly object logLock = new object();
    readonly List<DateTime> crashTimes = new List<DateTime>();
    readonly string logPath;
    readonly Control ui = new Control();

    ToolStripMenuItem miAutoStart, miAutoRestart, miStatus;
    Process proc;
    bool stopping, exiting;
    DateTime adminLaunchedAt = DateTime.MinValue;
    int port = 8990;
    string host = "127.0.0.1";

    public TrayContext()
    {
        var forceHandle = ui.Handle;
        cfg = new Settings(Path.Combine(baseDir, "KiroTray.ini"));
        Directory.CreateDirectory(Path.Combine(baseDir, "logs"));
        logPath = Path.Combine(baseDir, "logs", "kiro-rs.log");

        iconOk = LoadIcon("icon_ok.ico") ?? MakeIcon(Color.FromArgb(46, 160, 67));
        iconBad = LoadIcon("icon_bad.ico") ?? MakeIcon(Color.FromArgb(207, 34, 46));
        iconWait = LoadIcon("icon_wait.ico") ?? MakeIcon(Color.FromArgb(210, 153, 34));

        BuildMenu();
        tray.Icon = iconWait;
        tray.Text = "Kiro 反代：启动中";
        tray.Visible = true;
        tray.MouseDoubleClick += delegate(object s, MouseEventArgs e)
        {
            if (e.Button == MouseButtons.Left) OpenAdmin();
        };

        health.Interval = 5000;
        health.Tick += delegate { CheckHealth(); };
        health.Start();

        StartKiro();
    }

    void BuildMenu()
    {
        var m = new ContextMenuStrip();
        miStatus = new ToolStripMenuItem("状态：启动中") { Enabled = false };
        m.Items.Add(miStatus);
        m.Items.Add(new ToolStripSeparator());
        var miAdmin = new ToolStripMenuItem("打开管理后台", null, delegate { OpenAdmin(); });
        miAdmin.Font = new Font(miAdmin.Font, FontStyle.Bold);
        m.Items.Add(miAdmin);
        m.Items.Add("查看日志", null, delegate { OpenLog(); });
        m.Items.Add("打开 这个应用-rs 目录", null, delegate { Shell(KiroDir()); });
        m.Items.Add(new ToolStripSeparator());
        m.Items.Add("重启服务", null, delegate { Restart(); });
        miAutoRestart = new ToolStripMenuItem("崩溃自动重启", null, delegate
        {
            cfg.AutoRestart = !cfg.AutoRestart;
            miAutoRestart.Checked = cfg.AutoRestart;
            cfg.Save();
        });
        miAutoRestart.Checked = cfg.AutoRestart;
        m.Items.Add(miAutoRestart);
        miAutoStart = new ToolStripMenuItem("开机自启", null, delegate { ToggleAutoStart(); });
        miAutoStart.Checked = IsAutoStart();
        m.Items.Add(miAutoStart);
        m.Items.Add(new ToolStripSeparator());
        m.Items.Add("退出", null, delegate { ExitApp(); });
        tray.ContextMenuStrip = m;
    }

    string KiroDir() { return Environment.ExpandEnvironmentVariables(cfg.KiroDir); }

    // 从 kiro-rs 的 config.json 读取 host/port，找不到就用默认值
    void ReadKiroConfig()
    {
        try
        {
            string text = File.ReadAllText(Path.Combine(KiroDir(), "config.json"), Encoding.UTF8);
            var mp = Regex.Match(text, "\"port\"\\s*:\\s*(\\d+)");
            if (mp.Success) port = int.Parse(mp.Groups[1].Value);
            var mh = Regex.Match(text, "\"host\"\\s*:\\s*\"([^\"]+)\"");
            if (mh.Success) host = mh.Groups[1].Value;
        }
        catch { }
    }

    string BrowseHost()
    {
        return (host == "0.0.0.0" || host == "::" || host == "[::]") ? "127.0.0.1" : host;
    }

    // Chrome 按"主机名_路径"保存应用窗口的位置和最大化状态，主机名里的点会被当成层级分隔符，
    // 导致 127.0.0.1 的状态存了却读不回来（每次都按默认小窗口打开）。用 localhost 就没有这个问题。
    string AdminHost()
    {
        string h = BrowseHost();
        return h == "127.0.0.1" ? "localhost" : h;
    }

    // 用 Chrome 应用模式打开独立窗口；已打开时只把它切到前台，不重复打开
    void OpenAdmin()
    {
        string hp = AdminHost() + ":" + port;
        string url = "http://" + hp + "/admin";
        IntPtr hwnd = AdminWindow.Find(hp);
        if (hwnd != IntPtr.Zero) { AdminWindow.Activate(hwnd); return; }
        // 刚启动的窗口还没出现时，防止连点打开多个
        if ((DateTime.Now - adminLaunchedAt).TotalSeconds < 5) return;

        string chrome = AdminWindow.ChromePath();
        if (chrome == null) { Shell(url); return; }
        try
        {
            Process.Start(new ProcessStartInfo(chrome, "--start-maximized --app=\"" + url + "\"") { UseShellExecute = false });
            adminLaunchedAt = DateTime.Now;
        }
        catch { Shell(url); return; }

        // Chrome 已在运行时会忽略 --start-maximized；第一次打开（还没有保存过状态）时等窗口出现再最大化，
        // 之后 Chrome 会记住最大化，窗口一出现就是满屏
        int tries = 0;
        var t = new System.Windows.Forms.Timer { Interval = 200 };
        t.Tick += delegate
        {
            IntPtr h = AdminWindow.Find(hp);
            if (h == IntPtr.Zero && ++tries < 50) return;
            t.Stop();
            t.Dispose();
            if (h != IntPtr.Zero) AdminWindow.Maximize(h);
        };
        t.Start();
    }

    void OpenLog()
    {
        if (!File.Exists(logPath)) File.WriteAllText(logPath, "");
        Shell(logPath);
    }

    static void Shell(string target)
    {
        try { Process.Start(new ProcessStartInfo(target) { UseShellExecute = true }); }
        catch (Exception ex) { MessageBox.Show(ex.Message, "KiroTray", MessageBoxButtons.OK, MessageBoxIcon.Error); }
    }

    void StartKiro()
    {
        ReadKiroConfig();
        string exe = Path.Combine(KiroDir(), cfg.ExeName);
        if (!File.Exists(exe))
        {
            SetStatus(false, "找不到 " + exe);
            tray.ShowBalloonTip(5000, "KiroTray", "找不到 kiro-rs.exe，请修改 KiroTray.ini 里的 KiroDir。", ToolTipIcon.Error);
            return;
        }
        if (PortOpen())
        {
            SetStatus(false, "端口 " + port + " 已被占用");
            tray.ShowBalloonTip(5000, "KiroTray", "端口 " + port + " 已被占用，可能已有 kiro-rs 在运行（比如原来的 cmd 窗口），请先关掉它再点\"重启服务\"。", ToolTipIcon.Warning);
            return;
        }

        var psi = new ProcessStartInfo(exe)
        {
            WorkingDirectory = KiroDir(),
            UseShellExecute = false,
            CreateNoWindow = true,
            RedirectStandardOutput = true,
            RedirectStandardError = true,
            StandardOutputEncoding = Encoding.UTF8,
            StandardErrorEncoding = Encoding.UTF8
        };
        psi.EnvironmentVariables["NO_COLOR"] = "1";

        try
        {
            var p = new Process { StartInfo = psi, EnableRaisingEvents = true };
            p.OutputDataReceived += delegate(object s, DataReceivedEventArgs e) { WriteLog(e.Data); };
            p.ErrorDataReceived += delegate(object s, DataReceivedEventArgs e) { WriteLog(e.Data); };
            p.Exited += delegate { ui.BeginInvoke(new Action(delegate { OnExited(p); })); };
            p.Start();
            Job.Add(p);
            p.BeginOutputReadLine();
            p.BeginErrorReadLine();
            proc = p;
            WriteLog("==== KiroTray 启动 kiro-rs (PID " + p.Id + ") ====");
            SetStatus(null, "启动中");
        }
        catch (Exception ex)
        {
            SetStatus(false, "启动失败");
            WriteLog("启动失败: " + ex);
            tray.ShowBalloonTip(5000, "KiroTray", "启动失败：" + ex.Message, ToolTipIcon.Error);
        }
    }

    void OnExited(Process p)
    {
        if (p != proc) return;
        int code = -1;
        try { code = p.ExitCode; } catch { }
        proc = null;
        WriteLog("==== kiro-rs 已退出，退出码 " + code + " ====");
        if (stopping || exiting) return;

        SetStatus(false, "已退出（" + code + "）");
        if (!cfg.AutoRestart)
        {
            tray.ShowBalloonTip(5000, "KiroTray", "kiro-rs 已退出，退出码 " + code + "。", ToolTipIcon.Warning);
            return;
        }
        crashTimes.Add(DateTime.Now);
        crashTimes.RemoveAll(t => (DateTime.Now - t).TotalMinutes > 2);
        if (crashTimes.Count > 5)
        {
            tray.ShowBalloonTip(8000, "KiroTray", "kiro-rs 2 分钟内反复退出，已停止自动重启，请查看日志。", ToolTipIcon.Error);
            return;
        }
        var t2 = new System.Windows.Forms.Timer { Interval = 3000 };
        t2.Tick += delegate { t2.Stop(); t2.Dispose(); if (proc == null && !exiting && cfg.AutoRestart) StartKiro(); };
        t2.Start();
    }

    void StopKiro()
    {
        var p = proc;
        if (p == null) return;
        stopping = true;
        try
        {
            if (!p.HasExited) { p.Kill(); p.WaitForExit(5000); }
        }
        catch { }
        proc = null;
        stopping = false;
    }

    void Restart()
    {
        StopKiro();
        crashTimes.Clear();
        StartKiro();
    }

    bool PortOpen()
    {
        try
        {
            using (var c = new TcpClient())
            {
                var ar = c.BeginConnect(BrowseHost(), port, null, null);
                if (!ar.AsyncWaitHandle.WaitOne(300)) return false;
                c.EndConnect(ar);
                return true;
            }
        }
        catch { return false; }
    }

    void CheckHealth()
    {
        if (proc == null) return;
        bool alive = false;
        try { alive = !proc.HasExited; } catch { }
        if (!alive) return;
        if (PortOpen()) SetStatus(true, "运行中 " + BrowseHost() + ":" + port);
        else SetStatus(null, "启动中（端口未就绪）");
    }

    // ok: true=绿, false=红, null=黄
    void SetStatus(bool? ok, string text)
    {
        tray.Icon = ok == true ? iconOk : ok == false ? iconBad : iconWait;
        string tip = "Kiro 反代：" + text;
        tray.Text = tip.Length > 63 ? tip.Substring(0, 63) : tip;
        miStatus.Text = "状态：" + text;
    }

    void WriteLog(string line)
    {
        if (line == null) return;
        line = Ansi.Replace(line, "");
        lock (logLock)
        {
            try
            {
                var fi = new FileInfo(logPath);
                if (fi.Exists && fi.Length > (long)cfg.LogMaxMB * 1024 * 1024)
                {
                    string old = logPath + ".1";
                    if (File.Exists(old)) File.Delete(old);
                    File.Move(logPath, old);
                }
                File.AppendAllText(logPath, line + Environment.NewLine, new UTF8Encoding(false));
            }
            catch { }
        }
    }

    bool IsAutoStart()
    {
        using (var k = Registry.CurrentUser.OpenSubKey(RunKey))
            return k != null && k.GetValue(RunName) != null;
    }

    void ToggleAutoStart()
    {
        using (var k = Registry.CurrentUser.CreateSubKey(RunKey))
        {
            if (IsAutoStart()) k.DeleteValue(RunName, false);
            else k.SetValue(RunName, "\"" + Application.ExecutablePath + "\"");
        }
        miAutoStart.Checked = IsAutoStart();
    }

    void ExitApp()
    {
        exiting = true;
        health.Stop();
        StopKiro();
        tray.Visible = false;
        tray.Dispose();
        ExitThread();
    }

    Icon LoadIcon(string filename)
    {
        try
        {
            string path = Path.Combine(baseDir, filename);
            if (File.Exists(path)) return new Icon(path, 32, 32);
        }
        catch { }
        return null;
    }

    static Icon MakeIcon(Color color)
    {
        using (var bmp = new Bitmap(32, 32))
        {
            using (var g = Graphics.FromImage(bmp))
            {
                g.SmoothingMode = System.Drawing.Drawing2D.SmoothingMode.AntiAlias;
                g.TextRenderingHint = System.Drawing.Text.TextRenderingHint.AntiAliasGridFit;
                using (var b = new SolidBrush(color)) g.FillEllipse(b, 1, 1, 30, 30);
                using (var f = new Font("Segoe UI", 17, FontStyle.Bold, GraphicsUnit.Pixel))
                using (var sf = new StringFormat { Alignment = StringAlignment.Center, LineAlignment = StringAlignment.Center })
                    g.DrawString("K", f, Brushes.White, new RectangleF(0, 1, 32, 32), sf);
            }
            return Icon.FromHandle(bmp.GetHicon());
        }
    }
}
