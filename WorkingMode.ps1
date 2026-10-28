#Requires -Version 5.1
<#
  Working Mode - a tray app that keeps the laptop working with the lid closed,
  for a set time, then puts everything back.

  While on, it switches to a dedicated "Working Mode" power plan:
    - closing the lid does nothing, the PC never sleeps or hibernates on a timer
    - the hidden 2-minute "unattended sleep" timeout is disabled too
    - turbo boost off and CPU capped at 90% (defaults) so a closed laptop runs cooler
    - Wi-Fi adapter runs at full power (no power-saving drop-outs)
    - verifies every critical setting actually applied before saying "close the lid"
    - logs a heartbeat each minute and any gap (= the PC slept) - see RESEARCH.md
  and it watches over the session:
    - timer with a warning 10 minutes before the end, "+1 hour" at any time
    - when time is up: hibernate if you're away, or just switch back
    - battery guard: hibernates before the battery runs flat
    - internet watchdog: reconnects to the same Wi-Fi/hotspot if it drops
    - crash-safe: if the app or PC dies, normal settings are restored next start

  Usage:  WorkingMode.ps1            run the tray app
          WorkingMode.ps1 -Install   copy to %LOCALAPPDATA%, add shortcuts, start
          WorkingMode.ps1 -Uninstall restore settings, remove plan and shortcuts
          WorkingMode.ps1 -SelfTest  print what the app sees, change nothing
#>
param([switch]$Install, [switch]$Uninstall, [switch]$SelfTest)

$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Windows.Forms, System.Drawing

$AppName    = 'Working Mode'
$DataDir    = Join-Path $env:LOCALAPPDATA 'WorkingMode'
$AppDir     = Join-Path $DataDir 'app'
$ConfigPath = Join-Path $DataDir 'config.json'
$StatePath  = Join-Path $DataDir 'state.json'
$LogPath    = Join-Path $DataDir 'log.txt'
$StartupLnk = Join-Path ([Environment]::GetFolderPath('Startup')) "$AppName.lnk"
$StartMenuLnk = Join-Path ([Environment]::GetFolderPath('Programs')) "$AppName.lnk"
New-Item -ItemType Directory -Force $DataDir | Out-Null

# Power setting GUIDs. Several are hidden on Modern Standby laptops, where the
# aliases (LIDACTION etc.) aren't reliable, so everything goes by GUID.
$SUB_BUTTONS     = '4f971e89-eebd-4455-a8de-9e59040e7347'
$LIDACTION       = '5ca83367-6e45-459f-a27b-476b1d01c936'  # 0 = do nothing
$SUB_SLEEP       = '238c9fa8-0aad-41ed-83f4-97be242c8f20'
$STANDBYIDLE     = '29f6c1db-86da-48c5-9fdb-f2b67b1f44da'  # sleep after (s)
$HIBERNATEIDLE   = '9d7815a6-7ee4-497e-8888-515a05f02364'  # hibernate after (s)
$UNATTENDSLEEP   = '7bc4a2f9-d8fc-4469-b07b-33eb785aaca0'  # unattended sleep timeout (s), 120 by default
$NETSTANDBY      = 'f15576e8-98b7-4186-b944-eafa664402d9'  # network in standby, 1 = enable
$SUB_VIDEO       = '7516b95f-f776-4464-8c53-06167f40cc99'
$VIDEOIDLE       = '3c0bc021-c8a8-4e07-a973-6b14cbcb2b7e'  # screen off after (s)
$SUB_PROCESSOR   = '54533251-82be-4824-96c1-47b60b740d00'
$PROCTHROTTLEMAX = 'bc5038f7-23e0-4960-96da-33abaf5935ec'  # max processor state %
$PERFBOOSTMODE   = 'be337238-0d82-4146-a960-4f3749d470c7'  # 0 = turbo off
$SUB_WIFI        = '19cbb8fa-5279-450e-9fac-8a3d5fedd0c1'
$WIFI_POWERSAVE  = '12bbebe6-58d6-4636-95bb-3217ef867c1a'  # 0 = maximum performance

Add-Type @"
using System;
using System.Runtime.InteropServices;
public static class WmNative {
    [DllImport("kernel32.dll")] public static extern uint SetThreadExecutionState(uint flags);
    [StructLayout(LayoutKind.Sequential)] struct LASTINPUTINFO { public uint cbSize; public uint dwTime; }
    [DllImport("user32.dll")] static extern bool GetLastInputInfo(ref LASTINPUTINFO info);
    public static uint IdleSeconds() {
        var i = new LASTINPUTINFO();
        i.cbSize = (uint)Marshal.SizeOf(i);
        GetLastInputInfo(ref i);
        return ((uint)Environment.TickCount - i.dwTime) / 1000;
    }
}
"@
$ES_CONTINUOUS = [uint32]'0x80000000'
$ES_SYSTEM_REQUIRED = [uint32]'0x00000001'

# ---------------------------------------------------------------- helpers

function Write-Log([string]$msg) {
    "$(Get-Date -Format 'yyyy-MM-dd HH:mm:ss')  $msg" | Add-Content $LogPath -Encoding UTF8
}

$Defaults = @{
    PlanGuid     = ''
    CpuMax       = 90                 # % max processor state while working
    TurboOff     = $true              # disable CPU boost while working (biggest heat saver)
    EndAction    = 'HibernateIfAway'  # HibernateIfAway | Hibernate | Restore
    BatteryFloor = 15                 # hibernate at this %, 0 = off
    Reconnect    = $true
}

function Read-Config {
    $c = @{}
    foreach ($k in $Defaults.Keys) { $c[$k] = $Defaults[$k] }
    if (Test-Path $ConfigPath) {
        try {
            (Get-Content $ConfigPath -Raw | ConvertFrom-Json).PSObject.Properties |
                ForEach-Object { $c[$_.Name] = $_.Value }
        } catch { Write-Log "config unreadable, using defaults: $_" }
    }
    $c
}

function Save-Config { $script:Config | ConvertTo-Json | Set-Content $ConfigPath -Encoding UTF8 }

function Invoke-PowerCfg {
    $out = & powercfg.exe @args 2>&1
    if ($LASTEXITCODE -ne 0) { throw "powercfg $($args -join ' ') failed: $out" }
    $out
}

function Get-ActiveScheme {
    if ((Invoke-PowerCfg /getactivescheme) -join ' ' -match '([0-9a-f]{8}-[0-9a-f-]{27})') { $Matches[1] }
}

function Test-Scheme([string]$guid) {
    if (-not $guid) { return $false }
    ((& powercfg.exe /list) -join "`n") -match [regex]::Escape($guid)
}

function Set-PlanValue($guid, $sub, $setting, $value) {
    foreach ($mode in '/setacvalueindex', '/setdcvalueindex') {
        & powercfg.exe $mode $guid $sub $setting $value 2>&1 | Out-Null
        if ($LASTEXITCODE -ne 0) { Write-Log "skipped $sub/$setting (not on this PC)" }
    }
}

# Creates the "Working Mode" plan once (a copy of Balanced), then (re)applies its values.
function Initialize-Plan {
    if (-not (Test-Scheme $Config.PlanGuid)) {
        $out = Invoke-PowerCfg /duplicatescheme SCHEME_BALANCED
        if (($out -join ' ') -notmatch '([0-9a-f]{8}-[0-9a-f-]{27})') { throw "Couldn't create the power plan: $out" }
        $Config.PlanGuid = $Matches[1]
        Invoke-PowerCfg /changename $Config.PlanGuid $AppName 'Lid closed keeps working. Managed by the Working Mode tray app.' | Out-Null
        Save-Config
        Write-Log "created plan $($Config.PlanGuid)"
    }
    $g = $Config.PlanGuid
    Set-PlanValue $g $SUB_BUTTONS   $LIDACTION       0
    Set-PlanValue $g $SUB_SLEEP     $STANDBYIDLE     0
    Set-PlanValue $g $SUB_SLEEP     $HIBERNATEIDLE   0
    Set-PlanValue $g $SUB_SLEEP     $UNATTENDSLEEP   0
    Set-PlanValue $g 'fea3413e-7e05-4911-9a71-700331f1c294' $NETSTANDBY 1   # "no subgroup"
    Set-PlanValue $g $SUB_VIDEO     $VIDEOIDLE       600
    Set-PlanValue $g $SUB_PROCESSOR $PROCTHROTTLEMAX ([int]$Config.CpuMax)
    Set-PlanValue $g $SUB_PROCESSOR $PERFBOOSTMODE   $(if ($Config.TurboOff) { 0 } else { 2 })
    Set-PlanValue $g $SUB_WIFI      $WIFI_POWERSAVE  0
}

# Reads a value back (powercfg /qh also shows hidden settings). Returns @(ac, dc) or $null.
function Get-PlanValue($guid, $sub, $setting) {
    $text = (& powercfg.exe /qh $guid $sub $setting 2>$null) -join "`n"
    $ac = [regex]::Match($text, 'Current AC Power Setting Index: 0x([0-9a-f]+)')
    $dc = [regex]::Match($text, 'Current DC Power Setting Index: 0x([0-9a-f]+)')
    if ($ac.Success -and $dc.Success) { @([Convert]::ToInt32($ac.Groups[1].Value, 16), [Convert]::ToInt32($dc.Groups[1].Value, 16)) }
}

# Honest status: names of the critical settings that did NOT stick.
function Test-Plan {
    $g = $Config.PlanGuid
    $failed = @()
    foreach ($c in @(@('lid action', $SUB_BUTTONS, $LIDACTION), @('sleep timer', $SUB_SLEEP, $STANDBYIDLE),
                     @('unattended sleep', $SUB_SLEEP, $UNATTENDSLEEP), @('Wi-Fi power saving', $SUB_WIFI, $WIFI_POWERSAVE))) {
        $v = Get-PlanValue $g $c[1] $c[2]
        if (-not $v -or $v[0] -ne 0 -or $v[1] -ne 0) { $failed += $c[0] }
    }
    $failed
}

# The phone hotspot profile is often saved as "connect manually", so Windows never rejoins it.
function Get-ProfileMode([string]$ssid) {
    $m = netsh.exe wlan show profile name="$ssid" | Select-String 'Connection mode\s*:\s*(.+)$' | Select-Object -First 1
    if ($m) { $m.Matches[0].Groups[1].Value.Trim() }
}

function Get-Ssid {
    $m = netsh.exe wlan show interfaces | Select-String '^\s*SSID\s*:\s*(.+)$' | Select-Object -First 1
    if ($m) { $m.Matches[0].Groups[1].Value.Trim() }
}

# Same HTTP probe Windows uses. Ping isn't used: phone hotspots/proxies often block it.
function Test-Online {
    foreach ($url in 'http://www.msftconnecttest.com/connecttest.txt', 'http://connectivitycheck.gstatic.com/generate_204') {
        try {
            $req = [System.Net.WebRequest]::Create($url)
            $req.Timeout = 4000
            $req.Proxy = [System.Net.WebRequest]::GetSystemWebProxy()
            $req.Proxy.Credentials = [System.Net.CredentialCache]::DefaultCredentials
            $resp = $req.GetResponse(); $resp.Close()
            return $true
        } catch {}
    }
    $false
}

function Get-Battery {
    $p = [System.Windows.Forms.SystemInformation]::PowerStatus
    if ($p.BatteryChargeStatus -band [System.Windows.Forms.BatteryChargeStatus]::NoSystemBattery) { return $null }
    [pscustomobject]@{ Percent = [int]($p.BatteryLifePercent * 100); OnAC = $p.PowerLineStatus -eq 'Online' }
}

function Format-Span([TimeSpan]$t) {
    if ($t.TotalMinutes -lt 1) { return 'under a minute' }
    if ($t.TotalHours -ge 1) { return '{0}h {1:00}m' -f [int][math]::Floor($t.TotalHours), $t.Minutes }
    '{0} min' -f [int][math]::Ceiling($t.TotalMinutes)
}

function New-Shortcut([string]$path, [string]$vbs) {
    $s = (New-Object -ComObject WScript.Shell).CreateShortcut($path)
    $s.TargetPath = "$env:WINDIR\System32\wscript.exe"
    $s.Arguments = "`"$vbs`""
    $s.WorkingDirectory = Split-Path $vbs
    $s.IconLocation = "$env:WINDIR\System32\powercpl.dll,0"
    $s.Description = 'Keep working with the lid closed'
    $s.Save()
}

# Puts the normal plan back. Used by Stop, crash recovery and uninstall.
function Restore-Previous([string]$prev) {
    if (-not (Test-Scheme $prev) -or $prev -eq $Config.PlanGuid) { $prev = 'SCHEME_BALANCED' }
    Invoke-PowerCfg /setactive $prev | Out-Null
    Write-Log "restored plan $prev"
}

# ---------------------------------------------------------------- one-shot modes

$script:Config = Read-Config

if ($SelfTest) {
    $b = Get-Battery
    [pscustomobject]@{
        ActivePlan      = Get-ActiveScheme
        WorkingPlan     = if (Test-Scheme $Config.PlanGuid) { $Config.PlanGuid } else { '(not created yet)' }
        Battery         = if ($b) { "$($b.Percent)% $(if ($b.OnAC) {'plugged in'} else {'on battery'})" } else { 'none' }
        'Lid AC/DC (now)'        = (Get-PlanValue SCHEME_CURRENT $SUB_BUTTONS $LIDACTION) -join '/'
        'Unattended s AC/DC'     = (Get-PlanValue SCHEME_CURRENT $SUB_SLEEP $UNATTENDSLEEP) -join '/'
        'WiFi saving AC/DC'      = (Get-PlanValue SCHEME_CURRENT $SUB_WIFI $WIFI_POWERSAVE) -join '/'
        WiFi            = Get-Ssid
        WiFiProfileMode = Get-ProfileMode (Get-Ssid)
        Online          = Test-Online
        IdleSeconds     = [WmNative]::IdleSeconds()
        HibernateOn     = ((& powercfg.exe /a) -join "`n") -match '(?m)^\s+Hibernate\s*$'
        StartsWithWin   = Test-Path $StartupLnk
    } | Format-List
    exit
}

if ($Install) {
    New-Item -ItemType Directory -Force $AppDir | Out-Null
    Copy-Item (Join-Path $PSScriptRoot 'WorkingMode.ps1'), (Join-Path $PSScriptRoot 'WorkingMode.vbs') $AppDir -Force
    $vbs = Join-Path $AppDir 'WorkingMode.vbs'
    New-Shortcut $StartMenuLnk $vbs
    New-Shortcut $StartupLnk $vbs
    Start-Process wscript.exe "`"$vbs`""
    Write-Host "Installed. Look for the Working Mode icon in the tray (click ^ if it's hidden)."
    exit
}

if ($Uninstall) {
    $m = $null
    if ([System.Threading.Mutex]::TryOpenExisting('Local\WorkingModeTray', [ref]$m)) {
        [System.Windows.Forms.MessageBox]::Show('Working Mode is still running. Right-click its tray icon > Exit, then run Uninstall again.', $AppName) | Out-Null
        exit 1
    }
    if ((Get-ActiveScheme) -eq $Config.PlanGuid) { Restore-Previous ((Get-Content $StatePath -Raw -ErrorAction SilentlyContinue | ConvertFrom-Json).Previous) }
    if (Test-Scheme $Config.PlanGuid) { Invoke-PowerCfg /delete $Config.PlanGuid | Out-Null }
    Remove-Item $StartupLnk, $StartMenuLnk -ErrorAction SilentlyContinue
    Remove-Item $DataDir -Recurse -Force -ErrorAction SilentlyContinue
    Write-Host 'Working Mode removed and normal power settings restored.'
    exit
}

# ---------------------------------------------------------------- tray app

$created = $false
$Mutex = New-Object System.Threading.Mutex($true, 'Local\WorkingModeTray', [ref]$created)
if (-not $created) {
    [System.Windows.Forms.MessageBox]::Show('Working Mode is already running - look for its icon in the tray.', $AppName) | Out-Null
    exit
}

$script:State       = $null    # @{ Previous; EndsAt (or $null = until stopped); Warned }
$script:HibernateAt = $null    # set while a hibernate countdown is running
$script:Ssid        = $null
$script:Online      = $true
$script:OfflineTicks = 0
$script:BatteryAlerted = $false
$script:Tick        = 0
$script:LastTick    = $null

function New-TrayIcon([bool]$active) {
    $bmp = New-Object System.Drawing.Bitmap 32, 32
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.SmoothingMode = 'AntiAlias'
    $fill = if ($active) { [System.Drawing.Color]::FromArgb(34, 160, 90) } else { [System.Drawing.Color]::FromArgb(120, 120, 120) }
    $g.FillEllipse((New-Object System.Drawing.SolidBrush $fill), 1, 1, 30, 30)
    $white = New-Object System.Drawing.SolidBrush ([System.Drawing.Color]::White)
    if ($active) {
        $g.FillPolygon($white, [System.Drawing.Point[]]@((New-Object System.Drawing.Point 12, 8), (New-Object System.Drawing.Point 24, 16), (New-Object System.Drawing.Point 12, 24)))
    } else {
        $g.FillRectangle($white, 10, 9, 4, 14); $g.FillRectangle($white, 18, 9, 4, 14)
    }
    $g.Dispose()
    [System.Drawing.Icon]::FromHandle($bmp.GetHicon())
}
$IconOn  = New-TrayIcon $true
$IconOff = New-TrayIcon $false

$Tray = New-Object System.Windows.Forms.NotifyIcon
$Tray.Visible = $true
$Menu = New-Object System.Windows.Forms.ContextMenuStrip
$Tray.ContextMenuStrip = $Menu

function Show-Note([string]$title, [string]$text, [string]$kind = 'Info') {
    $Tray.ShowBalloonTip(6000, $title, $text, [System.Windows.Forms.ToolTipIcon]$kind)
    Write-Log "$title - $text"
}

function Invoke-Safe([scriptblock]$block) {
    try { & $block } catch {
        Write-Log "ERROR $($_.Exception.Message)"
        Show-Note $AppName $_.Exception.Message 'Error'
    }
}

function Save-State {
    if ($script:State) {
        @{ Previous = $State.Previous; EndsAt = if ($State.EndsAt) { $State.EndsAt.ToString('o') } else { $null } } |
            ConvertTo-Json | Set-Content $StatePath -Encoding UTF8
    } else {
        Remove-Item $StatePath -ErrorAction SilentlyContinue
    }
}

function Update-Tray {
    if ($script:State) {
        $Tray.Icon = $IconOn
        $left = if ($State.EndsAt) { "$(Format-Span ($State.EndsAt - (Get-Date))) left" } else { 'until you stop it' }
        $Tray.Text = "$AppName`: ON - $left"
    } else {
        $Tray.Icon = $IconOff
        $Tray.Text = "$AppName`: off"
    }
}

function Start-Working([double]$hours) {
    Initialize-Plan
    $prev = Get-ActiveScheme
    if ($script:State) { $prev = $State.Previous }
    Invoke-PowerCfg /setactive $Config.PlanGuid | Out-Null
    [WmNative]::SetThreadExecutionState($ES_CONTINUOUS -bor $ES_SYSTEM_REQUIRED) | Out-Null
    $script:State = @{ Previous = $prev; EndsAt = $null; Warned = $false }
    if ($hours -gt 0) { $State.EndsAt = (Get-Date).AddHours($hours) }
    $script:Ssid = Get-Ssid
    $script:HibernateAt = $null
    $script:BatteryAlerted = $false
    $script:LastTick = $null
    Save-State; Update-Tray
    $failed = @(Test-Plan)
    if ($failed.Count) {
        Show-Note 'Working Mode is on, but NOT safe to close the lid' "These settings didn't apply: $($failed -join ', '). See the log." 'Warning'
        return
    }
    $until = if ($State.EndsAt) { "until $($State.EndsAt.ToString('HH:mm'))" } else { 'until you turn it off' }
    $net = if ($Ssid) { " Staying on '$Ssid'." } else { '' }
    if ($Ssid -and (Get-ProfileMode $Ssid) -match 'manual') {
        $net += ' Tip: this network is set to connect manually - fix it in Settings so drops recover.'
    }
    Show-Note 'Working Mode is on' "You can close the lid - the PC keeps working $until.$net"
}

function Stop-Working([string]$why = 'Normal power settings are back.') {
    if (-not $script:State) { return }
    Restore-Previous $State.Previous
    [WmNative]::SetThreadExecutionState($ES_CONTINUOUS) | Out-Null
    $script:State = $null
    $script:HibernateAt = $null
    Save-State; Update-Tray
    Show-Note 'Working Mode is off' $why
}

function Add-Time([double]$hours) {
    $base = if ($State.EndsAt -and $State.EndsAt -gt (Get-Date)) { $State.EndsAt } else { Get-Date }
    $State.EndsAt = $base.AddHours($hours)
    $State.Warned = $false
    $script:HibernateAt = $null
    Save-State; Update-Tray
    Show-Note 'Working Mode extended' "Now runs until $($State.EndsAt.ToString('HH:mm'))."
}

function Start-HibernateCountdown([string]$why) {
    $script:HibernateAt = (Get-Date).AddSeconds(60)
    Show-Note 'Hibernating in 1 minute' "$why Open the Working Mode menu to cancel."
}

function Complete-Timer {
    $away = [WmNative]::IdleSeconds() -ge 300
    switch ($Config.EndAction) {
        'Restore'   { Stop-Working "Time's up. Normal power settings are back." }
        'Hibernate' { Start-HibernateCountdown "Time's up." }
        default {
            if ($away) { Start-HibernateCountdown "Time's up and nobody is using the PC." }
            else { Stop-Working "Time's up. You're here, so I only switched back to normal settings." }
        }
    }
}

function Invoke-Tick {
    $script:Tick++
    if ($script:HibernateAt -and (Get-Date) -ge $HibernateAt) {
        Stop-Working 'Hibernating now.'
        Start-Process shutdown.exe '/h'
        return
    }
    if (-not $script:State) { return }

    if ($State.EndsAt -and -not $HibernateAt) {
        $left = $State.EndsAt - (Get-Date)
        if ($left.TotalSeconds -le 0) { Complete-Timer; Update-Tray; return }
        if ($left.TotalMinutes -le 10 -and -not $State.Warned) {
            $State.Warned = $true
            Show-Note 'Working Mode ends in 10 minutes' 'Open the menu and choose "+1 hour" to keep going.'
        }
    }

    $b = Get-Battery
    if ($b -and $Config.BatteryFloor -gt 0) {
        if ($b.OnAC) { $script:BatteryAlerted = $false }
        elseif ($b.Percent -le $Config.BatteryFloor -and -not $BatteryAlerted) {
            $script:BatteryAlerted = $true
            Start-HibernateCountdown "Battery is at $($b.Percent)%."
        }
    }

    # A gap between ticks means the PC was asleep/suspended - exactly what Working Mode must prevent.
    $now = Get-Date
    if ($script:LastTick -and ($now - $LastTick).TotalSeconds -gt 90) {
        Write-Log "GAP: no activity for $([int]($now - $LastTick).TotalSeconds)s - the PC was likely asleep"
    }
    $script:LastTick = $now

    if ($Tick % 4 -eq 0) {   # every minute
        $wasOnline = $script:Online
        $script:Online = Test-Online
        $b = Get-Battery
        Write-Log ("alive  online={0}  battery={1}  idle={2}s" -f $Online, $(if ($b) { "$($b.Percent)%$(if ($b.OnAC) { '+AC' })" } else { '-' }), [WmNative]::IdleSeconds())
        if ($Online) {
            if (-not $wasOnline) { Show-Note 'Back online' "Internet is working again$(if ($Ssid) { " on '$Ssid'" })." }
            $script:OfflineTicks = 0
        } else {
            $script:OfflineTicks++
            if ($OfflineTicks -eq 2 -and $Config.Reconnect -and $Ssid) {
                netsh.exe wlan connect name="$Ssid" | Out-Null
                Show-Note 'Internet dropped' "Reconnecting to '$Ssid'..." 'Warning'
            }
        }
    }
    Update-Tray
}

# ---- menu (rebuilt every time it opens, so it always shows live state)

function Add-Item($parent, [string]$text, $tag = $null, [scriptblock]$onClick = $null, [bool]$checked = $false) {
    $item = New-Object System.Windows.Forms.ToolStripMenuItem $text
    $item.Tag = $tag
    $item.Checked = $checked
    if ($onClick) { $item.add_Click($onClick) } else { $item.Enabled = $false }
    $items = if ($parent -is [System.Windows.Forms.ToolStripMenuItem]) { $parent.DropDownItems } else { $parent.Items }
    [void]$items.Add($item)
    $item
}
function Add-Separator($parent) { [void]$parent.Items.Add((New-Object System.Windows.Forms.ToolStripSeparator)) }

$OnStart     = { param($s, $e) Invoke-Safe { Start-Working ([double]$s.Tag) } }
$OnAdd       = { param($s, $e) Invoke-Safe { Add-Time 1 } }
$OnStop      = { param($s, $e) Invoke-Safe { Stop-Working } }
$OnCancelHib = { param($s, $e) Invoke-Safe {
    $script:HibernateAt = $null
    # if the timer already ran out, cancelling means "keep going" - give it another hour
    if ($State -and $State.EndsAt -and $State.EndsAt -le (Get-Date)) { Add-Time 1 }
    else { Show-Note 'Hibernate cancelled' 'Working Mode keeps running.' }
} }
$OnCpu       = { param($s, $e) Invoke-Safe { $Config.CpuMax = [int]$s.Tag; Save-Config; if ($State) { Initialize-Plan; Invoke-PowerCfg /setactive $Config.PlanGuid | Out-Null } } }
$OnTurbo     = { param($s, $e) Invoke-Safe { $Config.TurboOff = -not $Config.TurboOff; Save-Config; if ($State) { Initialize-Plan; Invoke-PowerCfg /setactive $Config.PlanGuid | Out-Null } } }
$OnEnd       = { param($s, $e) $Config.EndAction = [string]$s.Tag; Save-Config }
$OnBattery   = { param($s, $e) $Config.BatteryFloor = [int]$s.Tag; Save-Config }
$OnReconnect = { param($s, $e) $Config.Reconnect = -not $Config.Reconnect; Save-Config }
$OnStartup   = { param($s, $e) Invoke-Safe {
    if (Test-Path $StartupLnk) { Remove-Item $StartupLnk } else { New-Shortcut $StartupLnk (Join-Path $PSScriptRoot 'WorkingMode.vbs') }
} }
$OnLog       = { param($s, $e) if (Test-Path $LogPath) { Start-Process notepad.exe $LogPath } }
$OnExit      = { param($s, $e) Invoke-Safe { Stop-Working; $Tray.Visible = $false; [System.Windows.Forms.Application]::Exit() } }

function Build-Menu {
    $Menu.Items.Clear()

    if ($State) {
        $head = if ($State.EndsAt) { "ON - $(Format-Span ($State.EndsAt - (Get-Date))) left (until $($State.EndsAt.ToString('HH:mm')))" } else { 'ON - until you turn it off' }
    } else { $head = 'Off - the PC sleeps normally' }
    $h = Add-Item $Menu $head
    $h.Font = New-Object System.Drawing.Font($Menu.Font, [System.Drawing.FontStyle]::Bold)

    $b = Get-Battery
    $bat = if ($b) { "Battery $($b.Percent)%$(if ($b.OnAC) { ', plugged in' } else { ', on battery' })" } else { 'No battery' }
    $ssidNow = Get-Ssid
    $net = if (-not $Online) { 'offline' } elseif ($ssidNow) { "online via $ssidNow" } else { 'online' }
    [void](Add-Item $Menu "$bat  |  $net")
    Add-Separator $Menu

    if ($HibernateAt) {
        [void](Add-Item $Menu "Cancel hibernate ($([int]($HibernateAt - (Get-Date)).TotalSeconds)s)" $null $OnCancelHib)
    }
    if ($State) {
        [void](Add-Item $Menu '+1 hour' $null $OnAdd)
        [void](Add-Item $Menu 'Stop now' $null $OnStop)
    } else {
        foreach ($o in @(@('Start for 1 hour', 1), @('Start for 2 hours', 2), @('Start for 4 hours', 4), @('Start for 8 hours', 8), @('Start until I turn it off', 0))) {
            [void](Add-Item $Menu $o[0] $o[1] $OnStart)
        }
    }
    Add-Separator $Menu

    $set = Add-Item $Menu 'Settings' $null { }
    $cpu = Add-Item $set 'CPU limit (cooler when the lid is closed)' $null { }
    foreach ($o in @(@(100, '100% - full speed'), @(90, '90% (recommended)'), @(80, '80% - cooler'), @(70, '70% - coolest, slower'))) {
        [void](Add-Item $cpu $o[1] $o[0] $OnCpu ([int]$Config.CpuMax -eq $o[0]))
    }
    [void](Add-Item $set 'Turn off turbo boost (much cooler)' $null $OnTurbo ([bool]$Config.TurboOff))
    $end = Add-Item $set "When time is up" $null { }
    foreach ($o in @(@('HibernateIfAway', "Hibernate if I'm away, otherwise just switch back (recommended)"), @('Hibernate', 'Always hibernate'), @('Restore', 'Only switch back to normal settings'))) {
        [void](Add-Item $end $o[1] $o[0] $OnEnd ($Config.EndAction -eq $o[0]))
    }
    $bf = Add-Item $set 'Battery safety' $null { }
    foreach ($o in @(@(10, 'Hibernate at 10%'), @(15, 'Hibernate at 15% (recommended)'), @(25, 'Hibernate at 25%'), @(0, 'Off'))) {
        [void](Add-Item $bf $o[1] $o[0] $OnBattery ([int]$Config.BatteryFloor -eq $o[0]))
    }
    [void](Add-Item $set 'Reconnect Wi-Fi if the internet drops' $null $OnReconnect ([bool]$Config.Reconnect))
    [void](Add-Item $set 'Start with Windows' $null $OnStartup (Test-Path $StartupLnk))
    [void](Add-Item $set 'Open log' $null $OnLog)

    [void](Add-Item $Menu 'Exit' $null $OnExit)
}

$Menu.add_Opening({ param($s, $e) Invoke-Safe { Build-Menu }; $e.Cancel = $false })

# left click opens the same menu as right click
$ShowMenu = [System.Windows.Forms.NotifyIcon].GetMethod('ShowContextMenu', [System.Reflection.BindingFlags]'Instance,NonPublic')
$Tray.add_MouseUp({ param($s, $e) if ($e.Button -eq 'Left') { $ShowMenu.Invoke($Tray, $null) } })

$Timer = New-Object System.Windows.Forms.Timer
$Timer.Interval = 15000
$Timer.add_Tick({ Invoke-Safe { Invoke-Tick } })

# ---- crash recovery: pick up where we left off, or put normal settings back

Invoke-Safe {
    $saved = if (Test-Path $StatePath) { Get-Content $StatePath -Raw | ConvertFrom-Json } else { $null }
    $endsAt = if ($saved -and $saved.EndsAt) { [datetime]$saved.EndsAt } else { $null }
    if ($saved -and (-not $endsAt -or $endsAt -gt (Get-Date))) {
        Initialize-Plan
        Invoke-PowerCfg /setactive $Config.PlanGuid | Out-Null
        [WmNative]::SetThreadExecutionState($ES_CONTINUOUS -bor $ES_SYSTEM_REQUIRED) | Out-Null
        $script:State = @{ Previous = $saved.Previous; EndsAt = $endsAt; Warned = $false }
        $script:Ssid = Get-Ssid
        Show-Note 'Working Mode resumed' 'It was on when the app last closed, so it is back on.'
    } elseif ($saved -or ((Get-ActiveScheme) -eq $Config.PlanGuid)) {
        Restore-Previous $(if ($saved) { $saved.Previous })
        Remove-Item $StatePath -ErrorAction SilentlyContinue
        Show-Note $AppName 'Restored normal power settings left over from last time.'
    }
}

Update-Tray
Build-Menu
$Timer.Start()
Write-Log 'app started'
[System.Windows.Forms.Application]::Run()
$Timer.Stop()
$Tray.Dispose()
$Mutex.ReleaseMutex()
