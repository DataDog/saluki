$ErrorActionPreference = "Stop"
Set-StrictMode -Version 3.0

Import-Module (Join-Path $PSScriptRoot "windows-rust-env.psm1") -Force

function Build-WindowsAdpImage {
    param([string]$ImageTag, [string]$BaseImage)
    $AdpBinary = Join-Path $env:CARGO_TARGET_DIR "$env:BUILD_PROFILE\agent-data-plane.exe"
    if (-not (Test-Path $AdpBinary)) {
        throw "ADP binary not found at ${AdpBinary}"
    }

    $ContextDir = Join-Path $env:TEMP "saluki-windows-adp-image"

    Remove-Item -Recurse -Force $ContextDir -ErrorAction SilentlyContinue
    New-Item -ItemType Directory -Force $ContextDir | Out-Null
    Copy-Item -Force $AdpBinary (Join-Path $ContextDir "agent-data-plane.exe")
    Assert-NoDynamicVCRuntimeImports -BinaryPath $AdpBinary
    Copy-Item -Force (Join-Path $RepoRoot "test\smp\regression\adp\shared\cert.pem") (Join-Path $ContextDir "ipc_cert.pem")
    [System.IO.File]::WriteAllText((Join-Path $ContextDir "auth_token"), "windows-integration-test-token", [System.Text.Encoding]::ASCII)

    $Dockerfile = @"
# escape=``
FROM ${BaseImage}
WORKDIR C:\adp
RUN New-Item -ItemType Directory -Force C:\ProgramData\Datadog
COPY agent-data-plane.exe C:\adp\agent-data-plane.exe
COPY auth_token C:\ProgramData\Datadog\auth_token
COPY ipc_cert.pem C:\ProgramData\Datadog\ipc_cert.pem
"@
    Set-Content -Path (Join-Path $ContextDir "Dockerfile") -Value $Dockerfile -Encoding ASCII

    Write-Host "[*] Building Windows ADP test image ${ImageTag} from ${BaseImage}..."
    Invoke-Native docker build --tag $ImageTag $ContextDir
}

# Packages datadog-intake and millstone in one image. Each runs in its own container of a case, from this image. The base
# is the one the ADP image uses, so the image adds two layers to what the runner already holds.
function Build-WindowsToolsImage {
    param([string]$ImageTag, [string]$BaseImage)
    $Tools = foreach ($Name in "datadog-intake", "millstone") {
        $Binary = Join-Path $env:CARGO_TARGET_DIR "release\$Name.exe"
        Assert-NoDynamicVCRuntimeImports -BinaryPath $Binary
        $Binary
    }
    $ContextDir = Join-Path $env:TEMP "saluki-windows-tools-image"

    Remove-Item -Recurse -Force $ContextDir -ErrorAction SilentlyContinue
    New-Item -ItemType Directory -Force $ContextDir | Out-Null
    $Tools | ForEach-Object { Copy-Item -Force $_ $ContextDir }

    $Dockerfile = @"
# escape=``
FROM ${BaseImage}
WORKDIR C:\tools
COPY datadog-intake.exe millstone.exe C:\tools\
"@
    Set-Content -Path (Join-Path $ContextDir "Dockerfile") -Value $Dockerfile -Encoding ASCII

    Write-Host "[*] Building Windows tools image ${ImageTag} from ${BaseImage}..."
    Invoke-Native docker build --tag $ImageTag $ContextDir
}

$RepoRoot = Resolve-Path (Join-Path $PSScriptRoot "..\..")
Set-Location $RepoRoot

if (-not $env:BUILD_PROFILE) {
    # Default to `release` so local invocations work without an explicit override; CI sets
    # BUILD_PROFILE via the workflow `variables:` block (`release` on dev, `optimized-release`
    # on tagged releases).
    $env:BUILD_PROFILE = "release"
}

Initialize-RustEnvironment -RepoRoot $RepoRoot

if (-not (Get-Command docker -ErrorAction SilentlyContinue)) {
    throw "Docker CLI not found in Windows build image. The integration job requires Docker CLI access to the host Docker daemon."
}
Invoke-Native docker version

Write-Host "[*] Building Panoramic and Agent Data Plane for Windows..."
# Windows PowerShell 5.1 doesn't have Get-Date's -AsUTC switch (added in PS 7.1).
$env:APP_BUILD_TIME = (Get-Date).ToUniversalTime().ToString("yyyy-MM-ddTHH:mm:ssZ")
if (-not $env:APP_GIT_HASH) {
    $env:APP_GIT_HASH = if ($env:CI_COMMIT_SHA) {
        $env:CI_COMMIT_SHA.Substring(0, [Math]::Min(7, $env:CI_COMMIT_SHA.Length))
    } else {
        try { (git rev-parse --short HEAD) } catch { "not-in-git" }
    }
}
# panoramic is the test harness, not the system under test, so it stays on the release profile
# regardless of $env:BUILD_PROFILE. ADP tracks $env:BUILD_PROFILE so a tagged release pipeline
# (BUILD_PROFILE=optimized-release in .gitlab-ci.yml) tests the same binary it ships, mirroring
# the linux/darwin flows.
Invoke-Native cargo build --release --package panoramic --package panoramic-courier
# The Windows container backend copies the courier into every container, from where panoramic looks for it.
$Courier = Join-Path $env:CARGO_TARGET_DIR "release\courier.exe"
Assert-NoDynamicVCRuntimeImports -BinaryPath $Courier
$CourierDir = Join-Path $RepoRoot "target\x86_64-pc-windows-msvc\release"
New-Item -ItemType Directory -Force $CourierDir | Out-Null
Copy-Item -Force $Courier $CourierDir
Invoke-Native cargo build --profile $env:BUILD_PROFILE --package agent-data-plane

# Both images share this base, so the tools image shares layers with the ADP image.
$BaseImage = if ($env:WINDOWS_ADP_BASE_IMAGE) { $env:WINDOWS_ADP_BASE_IMAGE } else { "mcr.microsoft.com/windows/servercore:ltsc2022" }
$ImageTag = if ($env:WINDOWS_ADP_IMAGE_TAG) { $env:WINDOWS_ADP_IMAGE_TAG } else { "saluki-images/agent-data-plane:testing-windows" }
Build-WindowsAdpImage -ImageTag $ImageTag -BaseImage $BaseImage

# The tools image has the tag the participant types name, so no --set is needed.
$ToolsTimer = [System.Diagnostics.Stopwatch]::StartNew()
Invoke-Native cargo build --release --bin datadog-intake --bin millstone
$ToolsBuildTime = $ToolsTimer.Elapsed
$ToolsTimer.Restart()
Build-WindowsToolsImage -ImageTag "saluki-images/correctness-tools:testing-windows" -BaseImage $BaseImage
Write-Host ("[*] Windows tools image: cargo build {0:N1}s, docker build {1:N1}s" -f $ToolsBuildTime.TotalSeconds, $ToolsTimer.Elapsed.TotalSeconds)

if (-not $env:PANORAMIC_LOG_DIR) {
    $env:PANORAMIC_LOG_DIR = "integration-logs"
}
New-Item -ItemType Directory -Force $env:PANORAMIC_LOG_DIR | Out-Null

$Panoramic = Join-Path $env:CARGO_TARGET_DIR "release\panoramic.exe"
$TestDir = Join-Path $RepoRoot "test\integration\cases"

Write-Host "[*] Running Windows integration tests..."
# Parallelism is deliberately lower than the Linux/macOS runs. Every Windows test case gets its
# own `nat` network, and creating those (plus attaching each container's endpoint) goes through
# HNS, which serializes badly: concurrent setups race and the loser gets back
# `hnsCall failed in Win32: The object already exists. (0x1392)`, erroring the test and failing
# the whole job. airlock retries that collision (see airlock's `retry` module), and
# halving the number of simultaneous racers cuts how often the retry is needed at all. The cost is
# roughly double the test phase -- a few minutes against a job whose wall clock is dominated by
# the build image pull and the cargo build -- so the trade is worth making here.
$PanoramicArgs = @(
    "run",
    "-d", $TestDir,
    "--subject-os", "windows",
    "--set", "saluki-images/agent-data-plane:testing-windows=$ImageTag",
    "--no-tui",
    "-p", "4",
    "-l", $env:PANORAMIC_LOG_DIR
)
Invoke-Native -FilePath $Panoramic -Arguments $PanoramicArgs
