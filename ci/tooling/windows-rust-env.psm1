# Shared PowerShell helpers used by every Windows CI script that needs to drive cargo:
# windows-unit-tests.ps1, windows-integration-tests.ps1, and windows-build-adp.ps1.
#
# The functions here are intentionally narrow and free of saluki-specific knowledge so each
# script can layer its own build/test/package logic on top. Import via:
#
#   Import-Module (Join-Path $PSScriptRoot "windows-rust-env.psm1") -Force
#
# `-Force` matters: GitLab caches the docker layer with this module embedded, and a stale
# already-loaded module would otherwise mask edits during iteration.

Set-StrictMode -Version 3.0

function Invoke-Native {
    # Invokes a native executable and throws if the exit code is non-zero.
    #
    # GOTCHA: this is implicitly an advanced function (any function with [Parameter()]
    # attributes is). PowerShell auto-injects common parameters including `-Verbose` and
    # `-Confirm`, which can be shortened to `-v` and `-C` (or any unique prefix). Single-
    # letter native flags that collide with those prefixes (e.g. `nasm -v`, `tar -C`) get
    # eaten by PS parameter binding before reaching the executable. For those calls, use
    # the call operator `&` directly (e.g. `& nasm -v`) instead of Invoke-Native; `&`
    # doesn't apply PS parameter binding. Multi-character flags like `--version` are safe.
    param(
        [Parameter(Mandatory = $true)]
        [string]$FilePath,

        [Parameter(ValueFromRemainingArguments = $true)]
        [string[]]$Arguments
    )

    & $FilePath @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "Command failed with exit code ${LASTEXITCODE}: ${FilePath} $($Arguments -join ' ')"
    }
}

function Add-PathEntry {
    param(
        [Parameter(Mandatory = $true)]
        [string]$Path
    )

    if ((Test-Path $Path) -and (-not ($env:Path -split ';' | Where-Object { $_ -eq $Path }))) {
        $env:Path = "${Path};${env:Path}"
    }
}

function Ensure-Protoc {
    param(
        [Parameter(Mandatory = $true)]
        [string]$Version,

        [Parameter(Mandatory = $true)]
        [string]$ExpectedSha256,

        [Parameter(Mandatory = $true)]
        [string]$InstallRoot
    )

    $ProtocBin = Join-Path $InstallRoot "bin\protoc.exe"
    if (-not (Test-Path $ProtocBin)) {
        Write-Host "[*] protoc not found; installing protoc ${Version}..."
        $Archive = Join-Path $env:TEMP "protoc-${Version}-win64.zip"
        $ExtractRoot = Join-Path $env:TEMP "protoc-${Version}-win64"
        Remove-Item -Recurse -Force $ExtractRoot -ErrorAction SilentlyContinue
        New-Item -ItemType Directory -Force $InstallRoot | Out-Null
        Invoke-WebRequest -UseBasicParsing -Uri "https://github.com/protocolbuffers/protobuf/releases/download/v${Version}/protoc-${Version}-win64.zip" -OutFile $Archive

        $ActualSha256 = (Get-FileHash -Algorithm SHA256 -Path $Archive).Hash.ToLowerInvariant()
        if ($ActualSha256 -ne $ExpectedSha256.ToLowerInvariant()) {
            throw "protoc archive checksum mismatch: expected ${ExpectedSha256}, got ${ActualSha256}"
        }

        Expand-Archive -Path $Archive -DestinationPath $ExtractRoot -Force
        Copy-Item -Recurse -Force (Join-Path $ExtractRoot "*") $InstallRoot
    }

    $env:PROTOC = $ProtocBin
    Add-PathEntry (Join-Path $InstallRoot "bin")
    Invoke-Native $ProtocBin --version
}

function Initialize-RustEnvironment {
    param(
        [Parameter(Mandatory = $true)]
        [string]$RepoRoot
    )

    $OriginalCargoHome = $env:CARGO_HOME
    if (-not $env:WINDOWS_CI_CARGO_HOME) {
        $env:WINDOWS_CI_CARGO_HOME = Join-Path $RepoRoot ".ci-cache\cargo"
    }
    if (-not $env:RUSTUP_HOME) {
        $env:RUSTUP_HOME = Join-Path $RepoRoot ".ci-cache\rustup"
    }
    if (-not $env:CARGO_TARGET_DIR) {
        $env:CARGO_TARGET_DIR = Join-Path $RepoRoot "target\windows-ci"
    }
    if (-not $env:PROTOC_VERSION) {
        $env:PROTOC_VERSION = "29.3"
    }
    if (-not $env:PROTOC_SHA256) {
        $env:PROTOC_SHA256 = "57ea59e9f551ad8d71ffaa9b5cfbe0ca1f4e720972a1db7ec2d12ab44bff9383"
    }
    if (-not $env:WINDOWS_CI_PROTOC_HOME) {
        $env:WINDOWS_CI_PROTOC_HOME = Join-Path $RepoRoot ".ci-cache\protoc\$env:PROTOC_VERSION"
    }

    New-Item -ItemType Directory -Force $env:WINDOWS_CI_CARGO_HOME | Out-Null
    New-Item -ItemType Directory -Force $env:WINDOWS_CI_PROTOC_HOME | Out-Null
    New-Item -ItemType Directory -Force $env:RUSTUP_HOME | Out-Null
    New-Item -ItemType Directory -Force $env:CARGO_TARGET_DIR | Out-Null

    if ($OriginalCargoHome) {
        Add-PathEntry (Join-Path $OriginalCargoHome "bin")
    }
    Add-PathEntry (Join-Path $env:USERPROFILE ".cargo\bin")

    if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
        throw "Rust toolchain not found in Windows build image. Install Rust in the image instead of downloading rustup at job runtime."
    }
    if (-not (Get-Command rustup -ErrorAction SilentlyContinue)) {
        throw "rustup not found in Windows build image. Install rustup in the image instead of downloading it at job runtime."
    }

    $ToolchainMatch = Select-String -Path (Join-Path $RepoRoot "rust-toolchain.toml") -Pattern 'channel\s*=\s*"([^"]+)"' | Select-Object -First 1
    if (-not $ToolchainMatch) {
        throw "Unable to determine Rust toolchain from rust-toolchain.toml"
    }
    $Toolchain = $ToolchainMatch.Matches[0].Groups[1].Value

    Write-Host "[*] Ensuring Rust toolchain ${Toolchain} is installed..."
    Invoke-Native rustup toolchain install --profile minimal $Toolchain
    Invoke-Native rustup default $Toolchain
    Invoke-Native rustup show
    Invoke-Native cargo --version

    $env:CARGO_HOME = $env:WINDOWS_CI_CARGO_HOME
    Add-PathEntry (Join-Path $env:CARGO_HOME "bin")
    Ensure-Protoc -Version $env:PROTOC_VERSION -ExpectedSha256 $env:PROTOC_SHA256 -InstallRoot $env:WINDOWS_CI_PROTOC_HOME
}

function Install-CachedZipTool {
    # Generic helper used by the Ensure-* tool installers below: downloads $Url to a temp
    # archive, verifies its SHA256 against $ExpectedSha256, extracts into $InstallRoot, and
    # prepends $BinSubdir (relative to $InstallRoot) to PATH.
    param(
        [Parameter(Mandatory = $true)]
        [string]$Name,

        [Parameter(Mandatory = $true)]
        [string]$Url,

        [Parameter(Mandatory = $true)]
        [string]$ExpectedSha256,

        [Parameter(Mandatory = $true)]
        [string]$InstallRoot,

        [Parameter(Mandatory = $true)]
        [string]$ProbeRelativePath,

        # Empty string means "the binary is at the install root" (e.g. ninja-win.zip extracts
        # ninja.exe flat). [AllowEmptyString()] is required because [Parameter(Mandatory)]
        # combined with [string] rejects empty strings by default.
        [Parameter(Mandatory = $true)]
        [AllowEmptyString()]
        [string]$BinSubdir
    )

    $Probe = Join-Path $InstallRoot $ProbeRelativePath
    if (-not (Test-Path $Probe)) {
        Write-Host "[*] $Name not found at $Probe; downloading..."
        $Archive = Join-Path $env:TEMP ("$Name-" + [System.Guid]::NewGuid().ToString("N") + ".zip")
        Invoke-WebRequest -UseBasicParsing -Uri $Url -OutFile $Archive

        $ActualSha256 = (Get-FileHash -Algorithm SHA256 -Path $Archive).Hash.ToLowerInvariant()
        if ($ActualSha256 -ne $ExpectedSha256.ToLowerInvariant()) {
            throw "$Name archive checksum mismatch: expected $ExpectedSha256, got $ActualSha256"
        }

        New-Item -ItemType Directory -Force $InstallRoot | Out-Null
        Expand-Archive -Path $Archive -DestinationPath $InstallRoot -Force
        Remove-Item -Force $Archive

        if (-not (Test-Path $Probe)) {
            throw "$Name install layout unexpected: $Probe still missing after extract"
        }
    }

    Add-PathEntry (Join-Path $InstallRoot $BinSubdir)
}

function Initialize-FipsBuildTools {
    # Installs the native build tools that aws-lc-fips-sys requires on Windows but that the
    # LTSC2022 buildimage doesn't ship: NASM (assembler), Go (build tooling), Ninja (CMake
    # generator). aws-lc-fips-sys explicitly does NOT support prebuilt-NASM shortcuts (unlike
    # aws-lc-sys), so all three must be installed before `cargo build --features fips` runs.
    # See https://aws.github.io/aws-lc-rs/requirements/windows.html.
    #
    # Installs are cached under $RepoRoot\.ci-cache\<tool>\<version>\ and the FIPS build job's
    # `cache:` block in .gitlab/windows.yml persists those paths between runs, so the cold-
    # download cost is paid once per runner.
    param(
        [Parameter(Mandatory = $true)]
        [string]$RepoRoot
    )

    # Pinned versions and SHA256s. Bump together when refreshing; checksums are mandatory so a
    # tampered or upstream-deleted release fails fast.
    $NasmVersion  = "2.16.03"
    $NasmSha256   = "3ee4782247bcb874378d02f7eab4e294a84d3d15f3f6ee2de2f47a46aa7226e6"
    $GoVersion    = "1.23.10"
    $GoSha256     = "3b533bbe63e73732bf19b8facc9160417e97d13eb174dfe58a213c6d0dee0010"
    $NinjaVersion = "1.12.1"
    $NinjaSha256  = "f550fec705b6d6ff58f2db3c374c2277a37691678d6aba463adcbb129108467a"

    Install-CachedZipTool `
        -Name "nasm" `
        -Url "https://www.nasm.us/pub/nasm/releasebuilds/$NasmVersion/win64/nasm-$NasmVersion-win64.zip" `
        -ExpectedSha256 $NasmSha256 `
        -InstallRoot (Join-Path $RepoRoot ".ci-cache\nasm\$NasmVersion") `
        -ProbeRelativePath "nasm-$NasmVersion\nasm.exe" `
        -BinSubdir "nasm-$NasmVersion"

    Install-CachedZipTool `
        -Name "go" `
        -Url "https://go.dev/dl/go$GoVersion.windows-amd64.zip" `
        -ExpectedSha256 $GoSha256 `
        -InstallRoot (Join-Path $RepoRoot ".ci-cache\go\$GoVersion") `
        -ProbeRelativePath "go\bin\go.exe" `
        -BinSubdir "go\bin"

    Install-CachedZipTool `
        -Name "ninja" `
        -Url "https://github.com/ninja-build/ninja/releases/download/v$NinjaVersion/ninja-win.zip" `
        -ExpectedSha256 $NinjaSha256 `
        -InstallRoot (Join-Path $RepoRoot ".ci-cache\ninja\$NinjaVersion") `
        -ProbeRelativePath "ninja.exe" `
        -BinSubdir ""

    # Smoke-test each binary to confirm it actually executes (PATH wired up, runtime deps
    # present). Use the call operator `&` instead of Invoke-Native because nasm's `-v` flag
    # collides with PowerShell's auto-injected `-Verbose` common parameter on advanced
    # functions (functions with [Parameter()] attributes); `&` doesn't apply PS param
    # binding to executables.
    & nasm -v
    if ($LASTEXITCODE -ne 0) { throw "nasm smoke test failed (exit $LASTEXITCODE)" }
    & go version
    if ($LASTEXITCODE -ne 0) { throw "go smoke test failed (exit $LASTEXITCODE)" }
    & ninja --version
    if ($LASTEXITCODE -ne 0) { throw "ninja smoke test failed (exit $LASTEXITCODE)" }
}

Export-ModuleMember -Function Invoke-Native, Add-PathEntry, Ensure-Protoc, Initialize-RustEnvironment, Install-CachedZipTool, Initialize-FipsBuildTools
