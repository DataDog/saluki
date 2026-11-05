$ErrorActionPreference = "Stop"
Set-StrictMode -Version 3.0

Import-Module (Join-Path $PSScriptRoot "windows-rust-env.psm1") -Force

function Get-WindowsBaseImage {
    if ($env:WINDOWS_ADP_BASE_IMAGE) {
        return $env:WINDOWS_ADP_BASE_IMAGE
    }
    return "mcr.microsoft.com/windows/servercore:ltsc2022"
}

function Build-WindowsAdpImage {
    param([string]$ImageTag)
    $AdpBinary = Join-Path $env:CARGO_TARGET_DIR "$env:BUILD_PROFILE\agent-data-plane.exe"
    if (-not (Test-Path $AdpBinary)) {
        throw "ADP binary not found at ${AdpBinary}"
    }

    $BaseImage = Get-WindowsBaseImage
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

function Build-WindowsToolsImage {
    param([string]$ImageTag)
    $IntakeBinary = Join-Path $env:CARGO_TARGET_DIR "release\datadog-intake.exe"
    Assert-NoDynamicVCRuntimeImports -BinaryPath $IntakeBinary

    $BaseImage = Get-WindowsBaseImage
    $ContextDir = Join-Path $env:TEMP "saluki-windows-tools-image"

    Remove-Item -Recurse -Force $ContextDir -ErrorAction SilentlyContinue
    New-Item -ItemType Directory -Force $ContextDir | Out-Null
    Copy-Item -Force $IntakeBinary (Join-Path $ContextDir "datadog-intake.exe")

    $Dockerfile = @"
# escape=``
FROM ${BaseImage}
COPY datadog-intake.exe C:\tools\datadog-intake.exe
ENTRYPOINT ["C:\\tools\\datadog-intake.exe"]
CMD []
"@
    Set-Content -Path (Join-Path $ContextDir "Dockerfile") -Value $Dockerfile -Encoding ASCII

    Write-Host "[*] Building Windows tools test image ${ImageTag} from ${BaseImage}..."
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
Invoke-Native cargo build --release --package panoramic
Invoke-Native cargo build --profile $env:BUILD_PROFILE --package agent-data-plane
# Build the courier for execution inside Windows containers.
Invoke-Native cargo build --release --package panoramic-courier --bin panoramic-courier
Invoke-Native cargo build --release --package datadog-intake --bin datadog-intake

$ImageTag = if ($env:WINDOWS_ADP_IMAGE_TAG) { $env:WINDOWS_ADP_IMAGE_TAG } else { "saluki-images/agent-data-plane:testing-windows" }
Build-WindowsAdpImage -ImageTag $ImageTag
$ToolsImageTag = "saluki-images/correctness-tools:testing-windows"
Build-WindowsToolsImage -ImageTag $ToolsImageTag

# The isolation mode (process or hyperv) and OS version decide which file operations a running container allows.
Write-Host "[*] Docker isolation and OS version:"
Invoke-Native docker info --format "{{.Isolation}} {{.OperatingSystem}} {{.KernelVersion}}"

$Panoramic = Join-Path $env:CARGO_TARGET_DIR "release\panoramic.exe"
$Courier = Join-Path $env:CARGO_TARGET_DIR "release\panoramic-courier.exe"
$Manifest = Join-Path $env:CARGO_TARGET_DIR "panoramic-artifacts.toml"

Write-Host "[*] Rendering the artifact manifest for ${ImageTag}..."
Invoke-Native -FilePath $Panoramic -Arguments @("artifacts", "render", "--set", "agent-adp-windows-image=$ImageTag", "--set", "tools-windows-image=$ToolsImageTag", "--set", "courier-windows-binary=$Courier", "--output", $Manifest)

# Run both daemon tests and integration cases before reporting a daemon-test failure.
Write-Host "[*] Running Windows container daemon tests..."
$DaemonTestError = $null
try {
    $env:PANORAMIC_TEST_WINDOWS_IMAGE = $ImageTag
    $env:PANORAMIC_TEST_WINDOWS_COURIER = $Courier
    # Pass the arguments as an array: PowerShell consumes a bare `--` when binding function parameters.
    Invoke-Native -FilePath cargo -Arguments @("test", "--release", "-p", "panoramic-core", "--lib", "--", "--ignored", "--test-threads", "1", "windows_container::daemon_tests")
} catch {
    $DaemonTestError = $_
    Write-Host "[!] Windows container daemon tests failed; running the integration cases before reporting the failure."
}

if (-not $env:PANORAMIC_LOG_DIR) {
    $env:PANORAMIC_LOG_DIR = "integration-logs"
}
New-Item -ItemType Directory -Force $env:PANORAMIC_LOG_DIR | Out-Null

$TestDir = Join-Path $RepoRoot "test\integration\cases"

Write-Host "[*] Running Windows integration tests..."
# Parallelism is deliberately lower than the Linux/macOS runs. Every Windows test case gets its
# own `nat` network, and creating those (plus attaching each container's endpoint) goes through
# HNS, which serializes badly: concurrent setups race and the loser gets back
# `hnsCall failed in Win32: The object already exists. (0x1392)`, erroring the test and failing
# the whole job. airlock retries that collision (see `with_hns_retry` in airlock's driver), and
# halving the number of simultaneous racers cuts how often the retry is needed at all. The cost is
# roughly double the test phase -- a few minutes against a job whose wall clock is dominated by
# the build image pull and the cargo build -- so the trade is worth making here.
$PanoramicArgs = @(
    "run",
    "-d", $TestDir,
    "--runtime", "windows",
    "--subject-os", "windows",
    "--artifacts", $Manifest,
    "--no-tui",
    "-p", "4",
    "-l", $env:PANORAMIC_LOG_DIR
)
Invoke-Native -FilePath $Panoramic -Arguments $PanoramicArgs

if ($DaemonTestError) {
    throw $DaemonTestError
}
