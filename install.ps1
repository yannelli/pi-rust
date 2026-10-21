# Install pi (pi-rust) on Windows: irm https://raw.githubusercontent.com/yannelli/pi-rust/main/install.ps1 | iex
# Settings: PI_VERSION (default: latest), PI_INSTALL_DIR (default: %LOCALAPPDATA%\Programs\pi), PI_RELEASES_URL.
# Runs in a script block so `irm | iex` leaves no variables or preferences behind in the session.

& {
    $ErrorActionPreference = 'Stop'
    $ProgressPreference = 'SilentlyContinue'
    $repoUrl = 'https://github.com/yannelli/pi-rust'

    function Get-PiFile([string]$Url, [string]$OutFile, [string]$Hint) {
        try {
            Invoke-WebRequest -Uri $Url -OutFile $OutFile -UseBasicParsing
        } catch {
            throw "Could not download $Url.$Hint $($_.Exception.Message)"
        }
    }

    if ($PSVersionTable.PSEdition -eq 'Core' -and -not $IsWindows) {
        throw 'install.ps1 is for Windows. On Linux and macOS, run: curl -fsSL https://raw.githubusercontent.com/yannelli/pi-rust/main/install.sh | sh'
    }
    if ($PSVersionTable.PSVersion.Major -lt 6) {
        [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
    }

    # The machine value names the real CPU, also when this PowerShell runs under x64 emulation on arm64.
    $arch = [Environment]::GetEnvironmentVariable('PROCESSOR_ARCHITECTURE', 'Machine')
    if (-not $arch) { $arch = $env:PROCESSOR_ARCHITECTURE }
    switch ($arch) {
        'AMD64' { $target = 'x86_64-pc-windows-msvc' }
        'ARM64' { $target = 'aarch64-pc-windows-msvc' }
        default { throw "No pi release for Windows on $arch. Releases cover x64 and arm64." }
    }

    $base = if ($env:PI_RELEASES_URL) { $env:PI_RELEASES_URL.TrimEnd('/') } else { "$repoUrl/releases" }
    $version = if ($env:PI_VERSION) { $env:PI_VERSION } else { 'latest' }
    if ($env:PI_INSTALL_DIR) {
        $dir = $env:PI_INSTALL_DIR
    } elseif ($env:LOCALAPPDATA) {
        $dir = Join-Path $env:LOCALAPPDATA 'Programs\pi'
    } else {
        throw 'LOCALAPPDATA is not set; set PI_INSTALL_DIR.'
    }
    try {
        $dir = (New-Item -ItemType Directory -Force -Path $dir).FullName.TrimEnd('\')
    } catch {
        throw "Could not create ${dir}. Set PI_INSTALL_DIR to a directory you can write to."
    }

    $tmp = Join-Path ([IO.Path]::GetTempPath()) ('pi-install-' + [Guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path $tmp | Out-Null
    try {
        if ($version -eq 'latest') {
            $sumsUrl = "$base/latest/download/SHA256SUMS"
            $wanted = $null
        } else {
            $sumsUrl = "$base/download/v$($version.TrimStart('v'))/SHA256SUMS"
            $wanted = "pi-v$($version.TrimStart('v'))-$target.zip"
        }
        $sumsFile = Join-Path $tmp 'SHA256SUMS'
        Get-PiFile $sumsUrl $sumsFile " Check PI_VERSION and the releases at $repoUrl/releases."
        $sums = @{}
        foreach ($line in Get-Content -LiteralPath $sumsFile) {
            if ($line -match '^([0-9a-fA-F]{64})\s+\*?(\S+)\s*$') { $sums[$Matches[2]] = $Matches[1].ToLowerInvariant() }
        }
        $archive = if ($wanted) { $wanted } else { $sums.Keys | Where-Object { $_ -like "pi-v*-$target.zip" } | Select-Object -First 1 }
        if (-not $archive -or -not $sums.ContainsKey($archive)) {
            throw "The release has no pi archive for $target (SHA256SUMS: $sumsUrl)"
        }
        $tag = $archive.Substring(3, $archive.Length - 3 - "-$target.zip".Length)

        Write-Host "Downloading pi $tag for $target"
        $zip = Join-Path $tmp $archive
        Get-PiFile "$base/download/$tag/$archive" $zip
        $actual = (Get-FileHash -LiteralPath $zip -Algorithm SHA256).Hash.ToLowerInvariant()
        if ($actual -ne $sums[$archive]) {
            throw "Checksum mismatch for ${archive}: expected $($sums[$archive]), got $actual"
        }
        Expand-Archive -LiteralPath $zip -DestinationPath (Join-Path $tmp 'unpacked') -Force
        $exe = Join-Path $tmp "unpacked\pi-$tag-$target\pi.exe"
        if (-not (Test-Path -LiteralPath $exe -PathType Leaf)) { throw "$archive does not contain pi-$tag-$target\pi.exe" }

        $dest = Join-Path $dir 'pi.exe'
        $staged = Join-Path $dir ".pi.exe.install-$PID"
        try {
            Copy-Item -LiteralPath $exe -Destination $staged -Force
            # A running pi.exe cannot be replaced but can be moved; pi deletes this folder when it starts.
            if (Test-Path -LiteralPath $dest) {
                $quarantine = Join-Path $dir '.pi-update-quarantine'
                New-Item -ItemType Directory -Force -Path $quarantine | Out-Null
                Move-Item -LiteralPath $dest -Destination (Join-Path $quarantine "$([DateTime]::UtcNow.Ticks)-$PID-pi.exe")
                try { Remove-Item -LiteralPath $quarantine -Recurse -Force } catch { }
            }
            Move-Item -LiteralPath $staged -Destination $dest
        } catch {
            Remove-Item -LiteralPath $staged -Force -ErrorAction SilentlyContinue
            throw "Could not install pi to ${dest}: $($_.Exception.Message)"
        }
        Write-Host "Installed pi $tag to $dest"
    } finally {
        Remove-Item -LiteralPath $tmp -Recurse -Force -ErrorAction SilentlyContinue
    }

    # Edit the registry value directly so %VARIABLE% entries in the user Path stay unexpanded.
    $key = [Microsoft.Win32.Registry]::CurrentUser.CreateSubKey('Environment')
    try {
        $userPath = $key.GetValue('Path', '', [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
        $entries = @($userPath -split ';' | Where-Object { $_ })
        $listed = $entries | Where-Object { [Environment]::ExpandEnvironmentVariables($_).TrimEnd('\') -eq $dir }
        if (-not $listed) {
            $key.SetValue('Path', (($entries + $dir) -join ';'), [Microsoft.Win32.RegistryValueKind]::ExpandString)
            # Changing any user variable this way broadcasts WM_SETTINGCHANGE, so new terminals see the new Path.
            [Environment]::SetEnvironmentVariable('PI_INSTALL_REFRESH', '1', 'User')
            [Environment]::SetEnvironmentVariable('PI_INSTALL_REFRESH', $null, 'User')
            Write-Host "Added $dir to your user Path. Open a new terminal to use pi."
        }
    } finally {
        $key.Close()
    }
    if (-not (($env:Path -split ';' | ForEach-Object { $_.TrimEnd('\') }) -contains $dir)) { $env:Path = "$env:Path;$dir" }

    $first = Get-Command pi -CommandType Application, ExternalScript -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($first -and $first.Source -ne $dest) {
        Write-Warning "Another pi comes first on Path: $($first.Source) (for example the npm package). Remove it, or move $dir before it in your Path."
    }
    Write-Host 'Update later with: pi update --self'
}
