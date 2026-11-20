<#
.SYNOPSIS
    Installs git-graft on Windows (PowerShell 5.1 or PowerShell 7).

.DESCRIPTION
    Downloads the release zip and SHA256SUMS over TLS 1.2+, verifies the SHA-256,
    extracts git-graft.exe to $env:LOCALAPPDATA\Programs\git-graft\bin and adds that
    directory to the *user* PATH if it is missing. No administrator rights are used.

    irm https://raw.githubusercontent.com/mprync/git-graft/main/install.ps1 | iex

    curl.exe -fsSL https://raw.githubusercontent.com/mprync/git-graft/main/install.ps1 -o install.ps1
    powershell -ExecutionPolicy Bypass -File install.ps1 [-Version 0.1.0] [-InstallDir <dir>]

.PARAMETER Version
    Release to install (for example 0.1.0). Default: latest. Env: GIT_GRAFT_VERSION.

.PARAMETER InstallDir
    Install directory. Env: GIT_GRAFT_INSTALL_DIR.

.PARAMETER BaseUrl
    Directory URL holding SHA256SUMS and the zip files instead of GitHub Releases.
    https:// or file:// (used by smoke tests). Env: GIT_GRAFT_DOWNLOAD_BASE.

.PARAMETER NoPathUpdate
    Do not modify the user PATH.

.NOTES
    Requires Git 2.46+ on PATH at runtime. The whole script is inside a function that
    is invoked on the last line, so a truncated download never runs partially.
#>

# Change the repository here; everything else derives from it.
$script:GitGraftRepo = 'mprync/git-graft'

function Install-GitGraft {
    [CmdletBinding()]
    param(
        [string] $Version = $env:GIT_GRAFT_VERSION,
        [string] $InstallDir = $env:GIT_GRAFT_INSTALL_DIR,
        [string] $BaseUrl = $env:GIT_GRAFT_DOWNLOAD_BASE,
        [switch] $NoPathUpdate
    )

    $ErrorActionPreference = 'Stop'
    $minGit = [version]'2.46'
    $repo = $script:GitGraftRepo

    # Windows PowerShell 5.1 defaults to older TLS versions; PowerShell 7 already uses TLS 1.2+.
    [Net.ServicePointManager]::SecurityProtocol = `
        [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

    $isWindowsHost = $true
    $isWindowsVariable = Get-Variable -Name IsWindows -ErrorAction SilentlyContinue
    if ($null -ne $isWindowsVariable) { $isWindowsHost = [bool] $isWindowsVariable.Value }
    if (-not $isWindowsHost) {
        throw 'install.ps1 supports Windows only. On macOS and Linux use install.sh.'
    }

    # Architecture: prefer the real OS architecture over the one of a 32-bit/emulated host.
    $arch = $env:PROCESSOR_ARCHITEW6432
    if (-not $arch) { $arch = $env:PROCESSOR_ARCHITECTURE }
    if (-not $arch) {
        try { $arch = [string][Runtime.InteropServices.RuntimeInformation]::OSArchitecture } catch { $arch = '' }
    }
    switch -Regex ($arch) {
        '^(AMD64|X64)$' { $target = 'x86_64-pc-windows-msvc'; break }
        '^ARM64$' { $target = 'aarch64-pc-windows-msvc'; break }
        default { throw "Unsupported Windows architecture: '$arch'." }
    }

    if ($Version) { $Version = $Version.TrimStart('v') }
    if ($Version -and $Version -notmatch '^[0-9A-Za-z.+-]+$') { throw "Invalid version: $Version" }
    if (-not $InstallDir) { $InstallDir = Join-Path $env:LOCALAPPDATA 'Programs\git-graft\bin' }

    if ($BaseUrl) {
        $BaseUrl = $BaseUrl.TrimEnd('/')
    } elseif ($Version) {
        $BaseUrl = "https://github.com/$repo/releases/download/v$Version"
    } else {
        $BaseUrl = "https://github.com/$repo/releases/latest/download"
    }
    if ($BaseUrl -notmatch '^(https|file)://') { throw "Refusing to download from a non-https URL: $BaseUrl" }

    function Get-Remote([string] $Url, [string] $OutFile) {
        if ($Url -like 'file://*') {
            # Local directory override, used by smoke tests.
            Copy-Item -LiteralPath ([Uri]$Url).LocalPath -Destination $OutFile -Force
        } else {
            Invoke-WebRequest -Uri $Url -OutFile $OutFile -UseBasicParsing
        }
    }

    $work = Join-Path ([IO.Path]::GetTempPath()) ("git-graft-install-" + [Guid]::NewGuid().ToString('N'))
    $stagedFile = $null
    $previousProgress = $ProgressPreference
    $ProgressPreference = 'SilentlyContinue'
    try {
        New-Item -ItemType Directory -Path $work | Out-Null
        Write-Host "Installing git-graft for $target"

        # SHA256SUMS lists "<hash>  <file>"; it also tells us which version is the latest.
        $sumsPath = Join-Path $work 'SHA256SUMS'
        Get-Remote "$BaseUrl/SHA256SUMS" $sumsPath
        $entries = foreach ($line in Get-Content -LiteralPath $sumsPath) {
            if ($line -match '^([0-9A-Fa-f]{64})\s+\*?(\S+)$') {
                [pscustomobject]@{ Hash = $Matches[1]; Name = $Matches[2] }
            }
        }
        if ($Version) {
            $wanted = "git-graft-$Version-$target.zip"
            $matched = @($entries | Where-Object { $_.Name -eq $wanted })
        } else {
            $matched = @($entries | Where-Object { $_.Name -like "git-graft-*-$target.zip" })
        }
        if ($matched.Count -eq 0) { throw "SHA256SUMS has no entry for target $target$(if ($Version) { " version $Version" })." }
        if ($matched.Count -gt 1) { throw "SHA256SUMS has more than one entry for target $target." }
        $asset = $matched[0].Name
        $expected = $matched[0].Hash
        if ($asset -match '[\\/]') { throw "Unexpected file name in SHA256SUMS: $asset" }

        $zipPath = Join-Path $work $asset
        Get-Remote "$BaseUrl/$asset" $zipPath
        $actual = (Get-FileHash -LiteralPath $zipPath -Algorithm SHA256).Hash
        if ($actual -ne $expected) {
            throw "Checksum mismatch for $asset (expected $($expected.ToLower()), got $($actual.ToLower())); nothing was installed."
        }
        Write-Host "Verified SHA-256 of $asset"

        # SHA256SUMS comes from the same release as the archive. When the GitHub
        # CLI is installed and signed in, also check the archive's signed build
        # provenance from this repository's release workflow.
        $gh = Get-Command gh -ErrorAction SilentlyContinue
        if ($gh -and $BaseUrl.StartsWith('https://github.com/')) {
            & $gh.Source auth status *> $null
            if ($LASTEXITCODE -eq 0) {
                & $gh.Source attestation verify $zipPath --repo $repo *> $null
                if ($LASTEXITCODE -ne 0) {
                    throw "Build provenance check failed for $asset; nothing was installed."
                }
                Write-Host 'Verified build provenance'
            }
        }

        $extractDir = Join-Path $work 'extract'
        Expand-Archive -LiteralPath $zipPath -DestinationPath $extractDir -Force
        $stem = [IO.Path]::GetFileNameWithoutExtension($asset)
        $source = Join-Path (Join-Path $extractDir $stem) 'git-graft.exe'
        if (-not (Test-Path -LiteralPath $source)) { $source = Join-Path $extractDir 'git-graft.exe' }
        if (-not (Test-Path -LiteralPath $source)) { throw "git-graft.exe not found inside $asset." }

        # Stage beside the destination, then rename, so an interrupted install never leaves a partial binary.
        New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
        $stagedFile = Join-Path $InstallDir (".git-graft.exe." + [Guid]::NewGuid().ToString('N'))
        Copy-Item -LiteralPath $source -Destination $stagedFile -Force
        $destination = Join-Path $InstallDir 'git-graft.exe'
        try {
            Move-Item -LiteralPath $stagedFile -Destination $destination -Force
        } catch {
            throw "Cannot replace $destination (is git-graft running?): $($_.Exception.Message)"
        }
        $stagedFile = $null
        Write-Host "Installed $destination"
        $versionText = & $destination --version 2>$null
        if ($LASTEXITCODE -eq 0 -and $versionText) { Write-Host $versionText }

        # Git is a runtime dependency; warn but do not fail.
        $git = Get-Command git -ErrorAction SilentlyContinue
        if (-not $git) {
            Write-Warning "git was not found on PATH; git-graft needs Git $minGit or newer at runtime."
        } else {
            $gitText = (& git --version 2>$null) -join ' '
            if ($gitText -match '(\d+)\.(\d+)(\.\d+)?') {
                if ([version]"$($Matches[1]).$($Matches[2])" -lt $minGit) {
                    Write-Warning "Found $gitText; git-graft needs Git $minGit or newer at runtime."
                }
            }
        }

        if (-not $NoPathUpdate) { Add-UserPath $InstallDir }
        Write-Host ''
        Write-Host 'Run: git graft -h'
    } finally {
        $ProgressPreference = $previousProgress
        if ($stagedFile -and (Test-Path -LiteralPath $stagedFile)) {
            Remove-Item -LiteralPath $stagedFile -Force -ErrorAction SilentlyContinue
        }
        if (Test-Path -LiteralPath $work) {
            Remove-Item -LiteralPath $work -Recurse -Force -ErrorAction SilentlyContinue
        }
    }
}

# Adds a directory to the user (never machine) PATH when absent, keeping REG_EXPAND_SZ intact.
function Add-UserPath([string] $Directory) {
    $normalized = $Directory.TrimEnd('\')
    $key = [Microsoft.Win32.Registry]::CurrentUser.CreateSubKey('Environment')
    try {
        $current = [string]$key.GetValue('Path', '', [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
        $parts = @($current -split ';' | Where-Object { $_ })
        $present = $parts | Where-Object {
            [Environment]::ExpandEnvironmentVariables($_).TrimEnd('\') -ieq $normalized
        }
        if ($present) { return }
        $updated = (@($parts) + $Directory) -join ';'
        $key.SetValue('Path', $updated, [Microsoft.Win32.RegistryValueKind]::ExpandString)
    } finally {
        $key.Close()
    }
    # Make the current session see it, then tell Explorer so new terminals pick it up.
    if (($env:Path -split ';') -notcontains $Directory) { $env:Path = "$env:Path;$Directory" }
    try {
        if (-not ('GitGraft.NativeMethods' -as [type])) {
            Add-Type -Namespace GitGraft -Name NativeMethods -MemberDefinition @'
[System.Runtime.InteropServices.DllImport("user32.dll", SetLastError = true, CharSet = System.Runtime.InteropServices.CharSet.Auto)]
public static extern System.IntPtr SendMessageTimeout(System.IntPtr hWnd, uint Msg, System.UIntPtr wParam, string lParam, uint fuFlags, uint uTimeout, out System.UIntPtr lpdwResult);
'@
        }
        $result = [UIntPtr]::Zero
        [void][GitGraft.NativeMethods]::SendMessageTimeout([IntPtr]0xffff, 0x1A, [UIntPtr]::Zero, 'Environment', 2, 5000, [ref]$result)
    } catch {
        Write-Verbose "Could not broadcast the environment change: $($_.Exception.Message)"
    }
    Write-Host "Added $Directory to your user PATH. Open a new terminal to use it."
}

try {
    Install-GitGraft @args
} catch {
    Write-Host "git-graft install: error: $($_.Exception.Message)" -ForegroundColor Red
    # Under `irm | iex` there is no script file and `exit` would close the user's shell.
    if ($PSCommandPath) { exit 1 }
}
