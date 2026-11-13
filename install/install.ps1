# Installs elorus for this user on Windows: the newest release's .exe, checked against the
# release's SHA256SUMS, then `elorus install` (%LOCALAPPDATA%\Programs\elorus, the user PATH,
# Apps & Features). Run it in PowerShell as
#   irm https://github.com/marianJeez/Elorus-CLI/releases/latest/download/install.ps1 | iex
# ELORUS_DOWNLOAD_URL fetches the files from elsewhere instead.
# CEILING: the checksums come from the same release; the installed elorus checks the signature on
# every update.

# A script block, so `irm | iex` leaves no variables behind; errors throw, as `exit` would close
# the user's shell.
& {
    $ErrorActionPreference = 'Stop'
    # Windows PowerShell draws a progress bar per chunk, which makes a download crawl.
    $ProgressPreference = 'SilentlyContinue'
    # Windows PowerShell 5 may not offer TLS 1.2, which GitHub requires.
    [Net.ServicePointManager]::SecurityProtocol =
        [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

    $base = $env:ELORUS_DOWNLOAD_URL
    if (-not $base) { $base = 'https://github.com/marianJeez/Elorus-CLI/releases/latest/download' }

    # A 32-bit PowerShell on 64-bit Windows names the machine in PROCESSOR_ARCHITEW6432.
    $arch = $env:PROCESSOR_ARCHITEW6432
    if (-not $arch) { $arch = $env:PROCESSOR_ARCHITECTURE }
    # Windows 11 on ARM runs the x64 build under emulation.
    if ($arch -ne 'AMD64' -and $arch -ne 'ARM64') {
        throw "elorus: no release runs on $arch Windows yet; there is a build for Windows x64"
    }
    $file = 'elorus-x86_64-pc-windows-msvc.exe'

    $tmp = Join-Path ([IO.Path]::GetTempPath()) ('elorus-' + [Guid]::NewGuid())
    New-Item -ItemType Directory -Path $tmp | Out-Null
    try {
        $exe = Join-Path $tmp $file
        $sums = Join-Path $tmp 'SHA256SUMS'
        Write-Host "elorus: downloading $file"
        Invoke-WebRequest -UseBasicParsing -Uri "$base/$file" -OutFile $exe
        Invoke-WebRequest -UseBasicParsing -Uri "$base/SHA256SUMS" -OutFile $sums

        $want = Get-Content $sums | ForEach-Object {
            $hex, $name = $_ -split '\s+', 2
            if ($name -and $name.TrimStart('*') -eq $file) { $hex.ToLower() }
        } | Select-Object -First 1
        $got = (Get-FileHash -Algorithm SHA256 -Path $exe).Hash.ToLower()
        if (-not $want -or $want -ne $got) {
            throw "elorus: $file does not match SHA256SUMS; nothing was installed"
        }

        & $exe install
        if ($LASTEXITCODE -ne 0) { throw 'elorus: the install did not finish' }
    } finally {
        Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
    }
}
