# Tests install.ps1 against fake releases served from a temp dir by python -m http.server (Windows only).
# It runs install.ps1 with pwsh and, when present, Windows PowerShell 5.1, and restores the user Path afterwards.
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$script = Join-Path $root 'install.ps1'
$tempRoot = if ($env:RUNNER_TEMP) { $env:RUNNER_TEMP } else { [IO.Path]::GetTempPath() }
$work = Join-Path $tempRoot ('pi-install-test-' + [Guid]::NewGuid().ToString('N'))
$site = Join-Path $work 'site'
$envKey = [Microsoft.Win32.Registry]::CurrentUser.CreateSubKey('Environment')
$savedPath = $envKey.GetValue('Path', $null, [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
$savedLocalAppData = $env:LOCALAPPDATA
$server = $null

function New-FakeRelease([string]$Tag, [switch]$Corrupt) {
    $rel = Join-Path $site "download\$Tag"
    New-Item -ItemType Directory -Force -Path $rel | Out-Null
    $lines = foreach ($target in 'x86_64-pc-windows-msvc', 'aarch64-pc-windows-msvc') {
        $pkg = Join-Path $work "pkg\pi-$Tag-$target"
        New-Item -ItemType Directory -Force -Path $pkg | Out-Null
        Set-Content -LiteralPath (Join-Path $pkg 'pi.exe') -Value "fake pi $Tag" -NoNewline
        $zip = Join-Path $rel "pi-$Tag-$target.zip"
        Compress-Archive -Path $pkg -DestinationPath $zip -Force
        "$((Get-FileHash -LiteralPath $zip -Algorithm SHA256).Hash.ToLowerInvariant())  pi-$Tag-$target.zip"
        if ($Corrupt) { Add-Content -LiteralPath $zip -Value 'x' }
    }
    Set-Content -LiteralPath (Join-Path $rel 'SHA256SUMS') -Value $lines
}

function Invoke-Install([string]$Shell, [string]$Dir, [string]$Version, [switch]$Iex) {
    $ErrorActionPreference = 'Continue'
    $env:PI_INSTALL_DIR = $Dir
    $env:PI_VERSION = $Version
    if ($Iex) {
        $out = & $Shell -NoProfile -ExecutionPolicy Bypass -Command "Get-Content -Raw -LiteralPath '$script' | Invoke-Expression" 2>&1
    } else {
        $out = & $Shell -NoProfile -ExecutionPolicy Bypass -File $script 2>&1
    }
    [pscustomobject]@{ Code = $LASTEXITCODE; Output = ($out | Out-String) }
}

function Assert-Installed($Result, [string]$Exe, [string]$Tag, [string]$What) {
    if ($Result.Code -ne 0) { throw "${What}: exit code $($Result.Code)`n$($Result.Output)" }
    $got = Get-Content -Raw -LiteralPath $Exe
    if ($got -ne "fake pi $Tag") { throw "${What}: $Exe holds '$got', expected 'fake pi $Tag'`n$($Result.Output)" }
    Write-Host "ok: $What"
}

try {
    New-FakeRelease v9.9.7 -Corrupt
    New-FakeRelease v9.9.8
    New-FakeRelease v9.9.9
    New-Item -ItemType Directory -Force -Path (Join-Path $site 'latest\download') | Out-Null
    Copy-Item (Join-Path $site 'download\v9.9.9\SHA256SUMS') (Join-Path $site 'latest\download\SHA256SUMS')

    $listener = [Net.Sockets.TcpListener]::new([Net.IPAddress]::Loopback, 0)
    $listener.Start()
    $port = $listener.LocalEndpoint.Port
    $listener.Stop()
    $server = Start-Process python -PassThru -NoNewWindow -RedirectStandardOutput (Join-Path $work 'server.out') `
        -RedirectStandardError (Join-Path $work 'server.err') -ArgumentList '-m', 'http.server', $port, '--bind', '127.0.0.1', '--directory', "`"$site`""
    $env:PI_RELEASES_URL = "http://127.0.0.1:$port"
    foreach ($attempt in 1..50) {
        try { Invoke-WebRequest "$env:PI_RELEASES_URL/latest/download/SHA256SUMS" -UseBasicParsing | Out-Null; break }
        catch { if ($attempt -eq 50) { throw 'http.server did not start' } else { Start-Sleep -Milliseconds 200 } }
    }

    $shells = @('pwsh') + @(Get-Command powershell -ErrorAction SilentlyContinue | ForEach-Object { 'powershell' })
    foreach ($shell in $shells) {
        $bin = Join-Path $work "bin-$shell"
        Assert-Installed (Invoke-Install $shell $bin '') (Join-Path $bin 'pi.exe') v9.9.9 "$shell installs the latest release"
        Assert-Installed (Invoke-Install $shell $bin '9.9.8' -Iex) (Join-Path $bin 'pi.exe') v9.9.8 "$shell via iex installs PI_VERSION over an existing pi"

        $result = Invoke-Install $shell (Join-Path $work "bad-$shell") 'v9.9.7'
        if ($result.Code -eq 0 -or $result.Output -notmatch 'Checksum mismatch') { throw "$shell installed a bad checksum`n$($result.Output)" }
        if (Test-Path (Join-Path $work "bad-$shell\pi.exe")) { throw "$shell left pi.exe after a bad checksum" }
        Write-Host "ok: $shell fails on a bad checksum"
    }

    $other = Join-Path $work 'other'
    New-Item -ItemType Directory -Force -Path $other | Out-Null
    Set-Content -LiteralPath (Join-Path $other 'pi.exe') -Value 'other pi'
    $savedSessionPath = $env:Path
    $env:Path = "$other;$env:Path"
    $result = Invoke-Install pwsh (Join-Path $work 'bin-pwsh') ''
    $env:Path = $savedSessionPath
    if ($result.Output -notmatch 'Another pi comes first') { throw "no warning about another pi on Path`n$($result.Output)" }
    Write-Host 'ok: another pi on Path is reported'

    $env:LOCALAPPDATA = Join-Path $work 'localappdata'
    $default = Join-Path $env:LOCALAPPDATA 'Programs\pi'
    Assert-Installed (Invoke-Install pwsh '' '') (Join-Path $default 'pi.exe') v9.9.9 'the default install dir is LOCALAPPDATA\Programs\pi'
    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    if (($userPath -split ';') -notcontains $default) { throw "the user Path does not list ${default}: $userPath" }
    Write-Host 'ok: the install dir is on the user Path'
} finally {
    $env:LOCALAPPDATA = $savedLocalAppData
    Remove-Item Env:PI_RELEASES_URL, Env:PI_INSTALL_DIR, Env:PI_VERSION -ErrorAction SilentlyContinue
    if ($null -eq $savedPath) { $envKey.DeleteValue('Path', $false) }
    else { $envKey.SetValue('Path', $savedPath, [Microsoft.Win32.RegistryValueKind]::ExpandString) }
    $envKey.Close()
    if ($server) { Stop-Process -Id $server.Id -Force -ErrorAction SilentlyContinue; $server.WaitForExit() }
    Remove-Item -LiteralPath $work -Recurse -Force -ErrorAction SilentlyContinue
}
# A failed child install leaves LASTEXITCODE at 1, which the CI shell would report as a failure.
exit 0
