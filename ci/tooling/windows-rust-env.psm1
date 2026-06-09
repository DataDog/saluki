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
    # libclang is needed by bindgen, which aws-lc-fips-sys runs at build time on
    # x86_64-pc-windows-msvc (no pre-generated bindings ship for that target). Use the
    # toolchain-only `clang+llvm-...-x86_64-pc-windows-msvc.tar.xz` archive (~845MB) rather
    # than the full LLVM-Windows installer; smaller and no NSIS. tar.exe on LTSC2022 (bsdtar)
    # CAN'T decompress xz internally -- it shells out to `xz -d` which isn't installed -- so
    # use 7-Zip in two passes (.tar.xz -> .tar -> tree). 7-Zip is pre-installed by the
    # Datadog buildimage's phase1 install_7zip.ps1 at c:\program files\7-zip\.
    $LlvmVersion  = "19.1.7"
    $LlvmSha256   = "b4557b4f012161f56a2f5d9e877ab9635cafd7a08f7affe14829bd60c9d357f0"

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

    # LLVM/libclang. Distributed only as tar.xz on Windows (no zip), so it doesn't fit the
    # Install-CachedZipTool shape. Inline download/verify/extract follows the same pattern.
    $LlvmExtractedDir = "clang+llvm-$LlvmVersion-x86_64-pc-windows-msvc"
    $LlvmInstallRoot  = Join-Path $RepoRoot ".ci-cache\llvm\$LlvmVersion"
    $LlvmBin          = Join-Path $LlvmInstallRoot "$LlvmExtractedDir\bin"
    $LibClangProbe    = Join-Path $LlvmBin "libclang.dll"
    if (-not (Test-Path $LibClangProbe)) {
        Write-Host "[*] libclang not found at $LibClangProbe; downloading LLVM $LlvmVersion (~845MB)..."
        $XzWork = Join-Path $env:TEMP ("llvm-xz-" + [System.Guid]::NewGuid().ToString("N"))
        New-Item -ItemType Directory -Force $XzWork | Out-Null
        try {
            $XzArchive = Join-Path $XzWork "src.tar.xz"
            Invoke-WebRequest -UseBasicParsing `
                -Uri "https://github.com/llvm/llvm-project/releases/download/llvmorg-$LlvmVersion/$LlvmExtractedDir.tar.xz" `
                -OutFile $XzArchive

            $ActualSha256 = (Get-FileHash -Algorithm SHA256 -Path $XzArchive).Hash.ToLowerInvariant()
            if ($ActualSha256 -ne $LlvmSha256.ToLowerInvariant()) {
                throw "LLVM archive checksum mismatch: expected $LlvmSha256, got $ActualSha256"
            }

            New-Item -ItemType Directory -Force $LlvmInstallRoot | Out-Null
            # Two-pass 7-Zip: first decompress .tar.xz -> .tar in $XzWork, then extract the
            # .tar tree into the install root. `&` instead of Invoke-Native because some 7z
            # short flags (-y, -bd) would collide with PS common parameter prefixes.
            & 7z x -bd -y "-o$XzWork" $XzArchive
            if ($LASTEXITCODE -ne 0) { throw "7z xz decompress failed (exit $LASTEXITCODE)" }
            $InnerTar = Join-Path $XzWork "src.tar"
            if (-not (Test-Path $InnerTar)) {
                throw "Expected $InnerTar after xz decompress, but it's missing"
            }
            & 7z x -bd -y "-o$LlvmInstallRoot" $InnerTar
            if ($LASTEXITCODE -ne 0) { throw "7z tar extract failed (exit $LASTEXITCODE)" }
        } finally {
            Remove-Item -Recurse -Force $XzWork -ErrorAction SilentlyContinue
        }

        if (-not (Test-Path $LibClangProbe)) {
            throw "LLVM install layout unexpected: $LibClangProbe still missing after extract"
        }
    }
    # bindgen finds libclang via the LIBCLANG_PATH env var; setting it (rather than relying
    # on PATH) is the documented bindgen contract.
    $env:LIBCLANG_PATH = $LlvmBin
    Add-PathEntry $LlvmBin

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
    & clang --version
    if ($LASTEXITCODE -ne 0) { throw "clang smoke test failed (exit $LASTEXITCODE)" }
}

function Initialize-MsvcEnvironment {
    # aws-lc-fips-sys's CMake builder shells out to vcvarsall.bat to discover the MSVC
    # toolchain environment (PATH, INCLUDE, LIB, etc.). Vanilla `docker run` against the
    # LTSC2022 buildimage doesn't activate that environment, so any cmake-using crate fails
    # with "vcvarsall.bat not found." even though VS BuildTools is installed.
    #
    # Run vcvarsall.bat in cmd, capture its post-execution environment via `set`, and apply
    # the result to the current PowerShell session so subsequent cargo invocations inherit
    # it. This is the standard Windows-CI pattern documented in MS's own dev-cmd-prompt
    # tooling.
    #
    # Non-FIPS builds (aws-lc-rs) work without this because that crate ships pre-generated
    # bindings + asm for x86_64-pc-windows-msvc and never invokes cmake; only FIPS builds
    # need the MSVC environment activated.
    param(
        [string]$Arch = "x64"
    )

    # Known buildimage VS BuildTools install root (from install_vstudio.ps1 in
    # DataDog/datadog-agent-buildimages). vswhere isn't reliable for non-default install
    # paths, so we hard-pin the known location.
    $VcvarsallPath = "C:\devtools\vstudio\VC\Auxiliary\Build\vcvarsall.bat"
    if (-not (Test-Path $VcvarsallPath)) {
        throw "vcvarsall.bat not found at expected buildimage path: $VcvarsallPath"
    }

    Write-Host "[*] Activating MSVC environment via $VcvarsallPath $Arch"
    # `set` (no args) prints all env vars one per line as NAME=VALUE. Wrap vcvarsall in
    # quotes to handle the space in `Program Files`-style paths even though our path doesn't
    # need it; harmless either way.
    $cmdline = "`"$VcvarsallPath`" $Arch >NUL && set"
    $output = & cmd.exe /c $cmdline
    if ($LASTEXITCODE -ne 0) {
        throw "vcvarsall.bat $Arch failed (exit $LASTEXITCODE)"
    }
    foreach ($line in $output) {
        if ($line -match '^([^=]+)=(.*)$') {
            Set-Item -Path "env:$($Matches[1])" -Value $Matches[2]
        }
    }
    if (-not $env:VCINSTALLDIR) {
        throw "vcvarsall.bat ran but VCINSTALLDIR is still unset; environment import failed"
    }
    Write-Host "[*] MSVC environment activated: VCINSTALLDIR=$env:VCINSTALLDIR"
}

Export-ModuleMember -Function Invoke-Native, Add-PathEntry, Ensure-Protoc, Initialize-RustEnvironment, Install-CachedZipTool, Initialize-FipsBuildTools, Initialize-MsvcEnvironment
