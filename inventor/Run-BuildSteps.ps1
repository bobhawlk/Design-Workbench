<#
.SYNOPSIS
  Build SolidCraft designs from an inv2sc build-steps.json, without inv2sc itself.

.DESCRIPTION
  The same step runner as `inv2sc convert`, in PowerShell: it starts SolidCraft's MCP server
  (`solidcraft-cli mcp`), runs each design's steps, binds the ids SolidCraft returns and uses
  them in later steps, tries a step's alternatives when it fails, and saves each design.
  Use it where inv2sc isn't built yet, or to rebuild from the steps inv2sc wrote.

.PARAMETER Steps
  The build-steps.json inv2sc wrote (with --dry-run, or by a convert run).

.PARAMETER Connect
  HOST:PORT of a running SolidCraft with its control channel on: build there, visibly.

.PARAMETER Profile
  The command profile (profiles\solidcraft.json): which SolidCraft command each step's op uses.
#>
param(
  [Parameter(Mandatory = $true)][string]$Steps,
  [string]$Profile,
  [string]$SolidCraftCli,
  [string]$Connect,
  [string]$Log
)
$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [Text.Encoding]::UTF8
$here = Split-Path -Parent $MyInvocation.MyCommand.Path
if (-not $Profile) {
  foreach ($p in (Join-Path $here 'solidcraft.json'), (Join-Path $here '..\profiles\solidcraft.json')) { if (Test-Path -LiteralPath $p) { $Profile = $p; break } }
}
if (-not $Profile) { throw 'No command profile: pass -Profile path\to\solidcraft.json' }
$profileJson = Get-Content -LiteralPath $Profile -Raw | ConvertFrom-Json
$plan = Get-Content -LiteralPath $Steps -Raw | ConvertFrom-Json
if ($plan.schema -ne 'lcw-build-steps/1') { throw "$Steps is not an inv2sc build-steps file" }

if (-not $SolidCraftCli) {
  $SolidCraftCli = $env:SOLIDCRAFT_CLI
  if (-not $SolidCraftCli) {
    $found = Get-Command solidcraft-cli -ErrorAction SilentlyContinue
    if ($found) { $SolidCraftCli = $found.Source }
    else { foreach ($c in 'C:\Program Files\SolidCraft\solidcraft-cli.exe', "$env:LOCALAPPDATA\Programs\SolidCraft\solidcraft-cli.exe") { if (Test-Path -LiteralPath $c) { $SolidCraftCli = $c; break } } }
  }
}
if (-not $SolidCraftCli -or -not (Test-Path -LiteralPath $SolidCraftCli)) { throw 'solidcraft-cli.exe not found: pass -SolidCraftCli (it is installed next to SolidCraft.exe)' }

$outDir = Split-Path -Parent ([IO.Path]::GetFullPath($Steps))
$args = @('mcp')
if ($Connect) { $args += @('--connect', $Connect) } else { $args += '--headless' }
$args += @('--allow-path', "`"$outDir`"")
$psi = New-Object Diagnostics.ProcessStartInfo
$psi.FileName = $SolidCraftCli
$psi.Arguments = ($args -join ' ')
$psi.UseShellExecute = $false
$psi.RedirectStandardInput = $true
$psi.RedirectStandardOutput = $true
$psi.StandardOutputEncoding = [Text.UTF8Encoding]::new($false)
$psi.CreateNoWindow = $true
$proc = [Diagnostics.Process]::Start($psi)
$stdin = $proc.StandardInput
$stdout = $proc.StandardOutput
$script:nextId = 1
$logLines = [Collections.Generic.List[string]]::new()
function LogLine($m) { Write-Host $m; $logLines.Add($m) }

function Rpc($method, $params) {
  $id = $script:nextId; $script:nextId++
  $msg = @{ jsonrpc = '2.0'; id = $id; method = $method; params = $params } | ConvertTo-Json -Depth 64 -Compress
  $stdin.WriteLine($msg); $stdin.Flush()
  while ($true) {
    $line = $stdout.ReadLine()
    if ($null -eq $line) { throw 'SolidCraft''s MCP server stopped' }
    try { $r = $line | ConvertFrom-Json } catch { continue }
    if ($r.id -ne $id -or $r.method) { continue }
    if ($r.error) { throw $r.error.message }
    return $r.result
  }
}
function Tool($name, $arguments) {
  $r = Rpc 'tools/call' @{ name = $name; arguments = $arguments }
  $text = ($r.content | Where-Object type -eq 'text' | ForEach-Object text) -join "`n"
  $value = $null
  try { $value = $text | ConvertFrom-Json } catch { $value = $text }
  if ($r.isError) { throw ($(if ($value.error) { $value.error } else { $text })) }
  return $value
}

$null = Rpc 'initialize' @{ protocolVersion = '2025-06-18'; capabilities = @{}; clientInfo = @{ name = 'Run-BuildSteps'; version = '0.1.0' } }
$stdin.WriteLine((@{ jsonrpc = '2.0'; method = 'notifications/initialized'; params = @{} } | ConvertTo-Json -Compress)); $stdin.Flush()

$vars = @{}
function Resolve-Vars($v) {
  if ($null -eq $v) { return $null }
  if ($v -is [string]) {
    if ($v.StartsWith('${') -and $v.EndsWith('}')) {
      $n = $v.Substring(2, $v.Length - 3)
      if ($vars.ContainsKey($n)) { return $vars[$n] }
      throw "needs $n"
    }
    return $v
  }
  if ($v -is [Array]) { return , @($v | ForEach-Object { Resolve-Vars $_ }) }
  if ($v -is [Management.Automation.PSCustomObject]) {
    $o = [ordered]@{}
    foreach ($p in $v.PSObject.Properties) { $o[$p.Name] = Resolve-Vars $p.Value }
    return $o
  }
  return $v
}
function Pointer($obj, [string]$ptr) {
  $cur = $obj
  foreach ($part in ($ptr.Trim('/') -split '/')) {
    if ($part -eq '') { continue }
    if ($null -eq $cur) { return $null }
    if ($cur -is [Array]) { $i = 0; if (-not [int]::TryParse($part, [ref]$i) -or $i -ge $cur.Count) { return $null }; $cur = $cur[$i] }
    else { $cur = $cur.$part }
  }
  return $cur
}
function Run-Op($op, $params) {
  $o = $profileJson.ops.$op
  if (-not $o) { throw "the profile has no op $op" }
  $mapped = [ordered]@{}
  foreach ($k in $params.Keys) {
    if ($null -eq $params[$k]) { continue }
    $name = $k; if ($o.params.$k) { $name = $o.params.$k }
    $mapped[$name] = $params[$k]
  }
  Tool 'execute' @{ command = $o.command; params = $mapped; verbose = $false }
}

$designs = @($plan.parts) + @($plan.assembly)
$failed = 0
foreach ($design in $designs) {
  LogLine "== $($design.name)"
  $stopped = $false
  foreach ($step in $design.steps) {
    if ($stopped -and $step.tool -ne 'save') { continue }
    $soft = $step.on_error -eq 'continue'
    try {
      foreach ($n in @($step.needs)) { if ($n -and -not $vars.ContainsKey($n)) { throw "needs $n" } }
      $args2 = Resolve-Vars $step.args
      if ($step.tool -eq 'auto_limits') {
        $f = $args2.found
        if ($f.min -ne $null -and $f.max -ne $null -and ($f.min_by -isnot [string] -or $f.max_by -isnot [string])) {
          $null = Run-Op 'set_limits' ([ordered]@{ joint = $args2.joint; index = 0; min = $f.min; max = $f.max })
        }
        continue
      }
      $result = $null
      try {
        if ($step.op) { $result = Run-Op $step.op $args2 } else { $result = Tool $step.tool $args2 }
      } catch {
        $err = $_.Exception.Message
        $done = $false
        foreach ($alt in @($step.alternatives)) {
          if (-not $alt) { continue }
          try { $result = Run-Op $alt.op (Resolve-Vars $alt.args); $done = $true; LogLine "   $($step.label): used $($alt.op) instead"; break } catch { }
        }
        if (-not $done) { throw $err }
      }
      if ($step.bind) {
        foreach ($p in $step.bind.PSObject.Properties) {
          foreach ($ptr in @($p.Value)) { $v = Pointer $result $ptr; if ($null -ne $v) { $vars[$p.Name] = $v; break } }
        }
      }
    } catch {
      $msg = $_.Exception.Message
      if ($msg -like 'needs *') { LogLine "   skipped $($step.label): $msg"; continue }
      LogLine "   FAILED $($step.label): $msg"
      if (-not $soft) { $stopped = $true; $failed++ }
    }
  }
}
try { $stdin.Close() } catch { }
try { if (-not $proc.WaitForExit(5000)) { $proc.Kill() } } catch { }
if ($Log) { [IO.File]::WriteAllLines($Log, $logLines) }
if ($failed) { LogLine "$failed design(s) stopped early; see the messages above"; exit 1 }
LogLine 'All designs built.'
