# Installs Neyuki from a GitHub release on Windows:
#
#   powershell -ExecutionPolicy Bypass -c "irm https://neyuki.cc/install.ps1 | iex"
#
# The executable goes to %USERPROFILE%\.neyuki\bin (or $env:NEYUKI_HOME\bin),
# which is added to your user PATH. Set $env:NEYUKI_VERSION to install a
# release (v0.12.0) instead of the latest, and $env:NEYUKI_NO_MODIFY_PATH=1
# to leave PATH alone.

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

function Install-Neyuki {
    $repo = 'neyuki-lang/neyuki'
    [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

    # PROCESSOR_ARCHITEW6432 is the real one inside a 32-bit PowerShell.
    $cpu = if ($env:PROCESSOR_ARCHITEW6432) { $env:PROCESSOR_ARCHITEW6432 } else { $env:PROCESSOR_ARCHITECTURE }
    $arch = switch ($cpu) {
        'AMD64' { 'x86_64' }
        'ARM64' { 'aarch64' }
        default { throw "no prebuilt Neyuki for $cpu; build it with ``cargo install neyuki``" }
    }
    $target = "$arch-pc-windows-msvc"

    $version = $env:NEYUKI_VERSION
    if (-not $version) {
        $latest = Invoke-RestMethod -UseBasicParsing "https://api.github.com/repos/$repo/releases/latest"
        $version = $latest.tag_name
    }
    if (-not $version.StartsWith('v')) { $version = "v$version" }

    $name = "neyuki-$version-$target"
    $base = "https://github.com/$repo/releases/download/$version"
    $tmp = Join-Path ([IO.Path]::GetTempPath()) ("neyuki-" + [Guid]::NewGuid())
    New-Item -ItemType Directory -Path $tmp | Out-Null
    try {
        Write-Host "Downloading Neyuki $version for $target"
        $zip = Join-Path $tmp "$name.zip"
        Invoke-WebRequest -UseBasicParsing "$base/$name.zip" -OutFile $zip

        # Releases publish SHA256SUMS since this script exists; older ones
        # are installed without the check.
        $sums = $null
        try { $sums = (Invoke-WebRequest -UseBasicParsing "$base/SHA256SUMS").Content } catch { }
        if ($sums) {
            if ($sums -is [byte[]]) { $sums = [Text.Encoding]::UTF8.GetString($sums) }
            $line = $sums -split "`n" | Where-Object { $_ -match " \*?$([regex]::Escape("$name.zip"))\s*$" } | Select-Object -First 1
            if (-not $line) { throw "SHA256SUMS does not list $name.zip" }
            $expected = ($line -split ' ')[0].ToLowerInvariant()
            $actual = (Get-FileHash -Algorithm SHA256 $zip).Hash.ToLowerInvariant()
            if ($actual -ne $expected) { throw "checksum mismatch for $name.zip" }
        }

        Expand-Archive -Path $zip -DestinationPath $tmp -Force
        $exe = Join-Path $tmp "$name\neyuki.exe"
        if (-not (Test-Path $exe)) { throw "$name.zip has no neyuki.exe" }

        $home_ = if ($env:NEYUKI_HOME) { $env:NEYUKI_HOME } else { Join-Path $env:USERPROFILE '.neyuki' }
        $bin = Join-Path $home_ 'bin'
        New-Item -ItemType Directory -Force -Path $bin | Out-Null
        $dest = Join-Path $bin 'neyuki.exe'
        # A running neyuki.exe cannot be overwritten but can be renamed.
        if (Test-Path $dest) {
            $old = "$dest.old"
            Remove-Item -Force $old -ErrorAction SilentlyContinue
            Move-Item -Force $dest $old
        }
        Copy-Item $exe $dest
        Remove-Item -Force "$dest.old" -ErrorAction SilentlyContinue
        Write-Host "Installed Neyuki $version to $dest"

        if ($env:NEYUKI_NO_MODIFY_PATH -ne '1') {
            $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
            $parts = if ($userPath) { $userPath -split ';' | Where-Object { $_ } } else { @() }
            if ($parts -notcontains $bin) {
                [Environment]::SetEnvironmentVariable('Path', (@($bin) + $parts) -join ';', 'User')
                Write-Host "Added $bin to your user PATH; open a new terminal to use it."
            }
            if (($env:Path -split ';') -notcontains $bin) { $env:Path = "$bin;$env:Path" }
        }
        Write-Host 'Run `neyuki help` to get started.'
    }
    finally {
        Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
    }
}

Install-Neyuki
