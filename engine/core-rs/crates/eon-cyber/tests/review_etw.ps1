# Native Windows acceptance. Only enable EON's payload-free provider in a
# disposable, bounded ETL session. Requires permission to start an ETW trace.
param([Parameter(Mandatory)][string]$AuditExe)
$ErrorActionPreference = 'Stop'
if (-not $IsWindows) { throw 'Requires Windows and PowerShell 7.' }
$exe = (Get-Item -LiteralPath $AuditExe).FullName
$provider = [guid]'87a9f157-e3c2-4589-93a0-299a3d325e72'
$session = 'eon-audit-' + [guid]::NewGuid().ToString('N')
$scratch = (New-Item -ItemType Directory -Path (Join-Path ([IO.Path]::GetTempPath()) $session)).FullName
$etl = Join-Path $scratch 'review.etl'
$reader = Join-Path $scratch 'review_etw_reader.exe'
$children = [Collections.Generic.List[Diagnostics.Process]]::new()

function Invoke-Review([bool]$optIn) {
    $start = [Diagnostics.ProcessStartInfo]::new($exe)
    $start.UseShellExecute = $false
    $start.RedirectStandardInput = $true
    $start.RedirectStandardOutput = $true
    $start.ArgumentList.Add('rust')
    if ($optIn) { $start.ArgumentList.Add('--etw') }
    $child = [Diagnostics.Process]::Start($start)
    # Keep both process handles until after readback. Windows cannot reuse
    # either PID while its process object is retained.
    $children.Add($child)
    try {
        $child.StandardInput.WriteLine('fn example() {}')
        $child.StandardInput.Close()
        $reply = $child.StandardOutput.ReadToEndAsync()
        if (-not $child.WaitForExit(10000)) { throw 'Audit CLI timed out.' }
        if ($child.ExitCode -ne 0) { throw 'Audit CLI failed.' }
        $report = $reply.GetAwaiter().GetResult() | ConvertFrom-Json
        if ($optIn -and $report.etw.status -ne 'submitted') { throw 'Provider not enabled.' }
        if (-not $optIn -and $report.PSObject.Properties['etw']) { throw 'Unexpected opt-in.' }
        return $child.Id
    } finally {
        if (-not $child.HasExited) { $child.Kill(); $child.WaitForExit() }
    }
}

function Build-Reader {
    if (-not (Get-Command cl.exe -CommandType Application -ErrorAction SilentlyContinue)) {
        $vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
        $vs = & $vswhere -latest -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
        if ($LASTEXITCODE -ne 0 -or -not $vs) { throw 'Requires MSVC and the Windows SDK.' }
        & (Join-Path $vs 'Common7\Tools\Launch-VsDevShell.ps1') -Arch amd64 -SkipAutomaticLocation
    }
    $source = Join-Path $PSScriptRoot 'review_etw_reader.c'
    $object = Join-Path $scratch 'review_etw_reader.obj'
    & cl.exe /nologo /W4 "/Fo$object" "/Fe$reader" $source advapi32.lib
    if ($LASTEXITCODE -ne 0) { throw 'Cannot build the ETL reader.' }
}

$started = $false
try {
    Build-Reader
    # Observe every keyword and level for this provider so the opt-out check
    # cannot overlook an event merely because it has a different descriptor.
    logman create trace $session -p $provider.ToString('B') '0xffffffffffffffff' '0xff' -o $etl -max 4 -ets
    if ($LASTEXITCODE -ne 0) { throw 'Cannot start private acceptance trace.' }
    $started = $true
    $without = Invoke-Review $false
    $with = Invoke-Review $true
    logman stop $session -ets
    if ($LASTEXITCODE -ne 0) { throw 'Cannot flush acceptance trace.' }
    $started = $false
    & $reader $etl $with $without
    if ($LASTEXITCODE -ne 0) { throw 'Private ETL verification failed.' }
    Write-Output 'PASS: independently read one payload-free opted-in completion and no opted-out event from ETW.'
} finally {
    if ($started) { logman stop $session -ets }
    foreach ($child in $children) { $child.Dispose() }
    Remove-Item -LiteralPath $scratch -Recurse -Force
}
