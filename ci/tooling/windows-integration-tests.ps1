$ErrorActionPreference = "Stop"
Set-StrictMode -Version 3.0

Import-Module (Join-Path $PSScriptRoot "windows-rust-env.psm1") -Force

function Build-WindowsAdpImage {
    $AdpBinary = Join-Path $env:CARGO_TARGET_DIR "$env:BUILD_PROFILE\agent-data-plane.exe"
    if (-not (Test-Path $AdpBinary)) {
        throw "ADP binary not found at ${AdpBinary}"
    }

    $ImageTag = if ($env:WINDOWS_ADP_IMAGE_TAG) { $env:WINDOWS_ADP_IMAGE_TAG } else { "saluki-images/agent-data-plane:testing-windows" }
    $BaseImage = if ($env:WINDOWS_ADP_BASE_IMAGE) { $env:WINDOWS_ADP_BASE_IMAGE } else { "mcr.microsoft.com/windows/servercore:ltsc2022" }
    $ContextDir = Join-Path $env:TEMP "saluki-windows-adp-image"

    Remove-Item -Recurse -Force $ContextDir -ErrorAction SilentlyContinue
    New-Item -ItemType Directory -Force $ContextDir | Out-Null
    Copy-Item -Force $AdpBinary (Join-Path $ContextDir "agent-data-plane.exe")
    Assert-NoDynamicVCRuntimeImports -BinaryPath $AdpBinary
    Copy-Item -Force (Join-Path $RepoRoot "ci\tooling\windows-adp-entrypoint.ps1") (Join-Path $ContextDir "entrypoint.ps1")
    Copy-Item -Force (Join-Path $RepoRoot "test\smp\regression\adp\shared\cert.pem") (Join-Path $ContextDir "ipc_cert.pem")
    [System.IO.File]::WriteAllText((Join-Path $ContextDir "auth_token"), "windows-integration-test-token", [System.Text.Encoding]::ASCII)

    $Dockerfile = @"
# escape=``
FROM ${BaseImage}
WORKDIR C:\adp
RUN New-Item -ItemType Directory -Force C:\ProgramData\Datadog
COPY agent-data-plane.exe C:\adp\agent-data-plane.exe
COPY entrypoint.ps1 C:\adp\entrypoint.ps1
COPY auth_token C:\ProgramData\Datadog\auth_token
COPY ipc_cert.pem C:\ProgramData\Datadog\ipc_cert.pem
ENTRYPOINT ["pwsh", "-NoProfile", "-NonInteractive", "-File", "C:\\adp\\entrypoint.ps1"]
CMD ["-c", "C:\\ProgramData\\Datadog\\datadog.yaml", "run"]
"@
    Set-Content -Path (Join-Path $ContextDir "Dockerfile") -Value $Dockerfile -Encoding ASCII

    Write-Host "[*] Building Windows ADP test image ${ImageTag} from ${BaseImage}..."
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

if (-not $env:BUILD_FEATURES) {
    $env:BUILD_FEATURES = "default"
}
if ($env:BUILD_FEATURES -eq "fips") {
    Initialize-FipsBuildTools -RepoRoot $RepoRoot
    New-VsBuildToolsJunction
}

if (-not (Get-Command docker -ErrorAction SilentlyContinue)) {
    throw "Docker CLI not found in Windows build image. The integration job requires Docker CLI access to the host Docker daemon."
}
Invoke-Native docker version

Write-Host "[*] Building Panoramic and Agent Data Plane for Windows (features=$env:BUILD_FEATURES)..."
# saluki-metadata reads these at build time. They must match the values that
# the Linux Makefile passes through, otherwise ADP's log subagent prefix
# renders as "UNKNOWN" instead of "DATAPLANE".
$env:APP_FULL_NAME = "Agent Data Plane"
$env:APP_SHORT_NAME = "data-plane"
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
Invoke-Native cargo build --profile $env:BUILD_PROFILE --package agent-data-plane --features $env:BUILD_FEATURES

Build-WindowsAdpImage

if (-not $env:PANORAMIC_LOG_DIR) {
    $env:PANORAMIC_LOG_DIR = "integration-logs"
}
New-Item -ItemType Directory -Force $env:PANORAMIC_LOG_DIR | Out-Null

$Panoramic = Join-Path $env:CARGO_TARGET_DIR "release\panoramic.exe"
$TestDir = Join-Path $RepoRoot "test\integration\cases"

Write-Host "[*] Running Windows integration tests..."
$PanoramicArgs = @(
    "run",
    "-d", $TestDir,
    "--runtime", "windows",
    "--no-tui",
    "-p", "4",
    "-l", $env:PANORAMIC_LOG_DIR
)
Invoke-Native -FilePath $Panoramic -Arguments $PanoramicArgs
