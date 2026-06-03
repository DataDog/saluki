$ErrorActionPreference = "Stop"
Set-StrictMode -Version 3.0

function Invoke-Native {
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

    $ToolchainMatch = Select-String -Path "rust-toolchain.toml" -Pattern 'channel\s*=\s*"([^"]+)"' | Select-Object -First 1
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

function Copy-WindowsRuntimeDlls {
    param(
        [Parameter(Mandatory = $true)]
        [string]$Destination
    )

    $SearchRoots = @(
        "C:\Windows\System32",
        "C:\BuildTools",
        "C:\Program Files",
        "C:\Program Files (x86)"
    )
    $DllNames = @("vcruntime140.dll", "vcruntime140_1.dll", "msvcp140.dll")

    foreach ($DllName in $DllNames) {
        $Found = $null
        foreach ($Root in $SearchRoots) {
            if (Test-Path $Root) {
                $Found = Get-ChildItem -Path $Root -Recurse -Filter $DllName -ErrorAction SilentlyContinue |
                    Select-Object -First 1
                if ($Found) {
                    break
                }
            }
        }

        if ($Found) {
            Write-Host "[*] Found runtime DLL ${DllName} at $($Found.FullName)"
            Copy-Item -Force $Found.FullName (Join-Path $Destination $DllName)
        } else {
            Write-Host "[*] Runtime DLL ${DllName} not found in build container."
        }
    }
}

function Build-WindowsAdpImage {
    $AdpBinary = Join-Path $env:CARGO_TARGET_DIR "release\agent-data-plane.exe"
    if (-not (Test-Path $AdpBinary)) {
        throw "ADP binary not found at ${AdpBinary}"
    }

    $ImageTag = if ($env:WINDOWS_ADP_IMAGE_TAG) { $env:WINDOWS_ADP_IMAGE_TAG } else { "saluki-images/agent-data-plane:testing-windows" }
    $BaseImage = if ($env:WINDOWS_ADP_BASE_IMAGE) { $env:WINDOWS_ADP_BASE_IMAGE } else { "mcr.microsoft.com/windows/servercore:ltsc2022" }
    $ContextDir = Join-Path $env:TEMP "saluki-windows-adp-image"

    Remove-Item -Recurse -Force $ContextDir -ErrorAction SilentlyContinue
    New-Item -ItemType Directory -Force $ContextDir | Out-Null
    Copy-Item -Force $AdpBinary (Join-Path $ContextDir "agent-data-plane.exe")
    Copy-WindowsRuntimeDlls -Destination $ContextDir
    Copy-Item -Force (Join-Path $RepoRoot "ci\tooling\windows-adp-entrypoint.ps1") (Join-Path $ContextDir "entrypoint.ps1")
    Copy-Item -Force (Join-Path $RepoRoot "test\smp\regression\adp\shared\cert.pem") (Join-Path $ContextDir "ipc_cert.pem")
    [System.IO.File]::WriteAllText((Join-Path $ContextDir "auth_token"), "windows-integration-test-token", [System.Text.Encoding]::ASCII)

    $Dockerfile = @"
# escape=``
FROM ${BaseImage}
WORKDIR C:\adp
RUN New-Item -ItemType Directory -Force C:\ProgramData\Datadog
COPY *.dll C:\adp\
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

function Remove-DockerContainerIfExists {
    param(
        [Parameter(Mandatory = $true)]
        [string]$ContainerName
    )

    $previousErrorActionPreference = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    docker rm -f $ContainerName *> $null
    $ErrorActionPreference = $previousErrorActionPreference
}

function Test-WindowsAdpImage {
    param(
        [Parameter(Mandatory = $true)]
        [string]$ImageTag
    )

    $ContainerName = "saluki-windows-adp-preflight"
    Remove-DockerContainerIfExists -ContainerName $ContainerName

    Write-Host "[*] Preflight: starting ${ImageTag} directly..."
    $DockerRunArgs = @(
        "run",
        "--detach",
        "--name", $ContainerName,
        "-e", "DD_API_KEY=test-api-key",
        "-e", "DD_HOSTNAME=windows-integration-preflight",
        "-e", "DD_DATA_PLANE__STANDALONE_MODE=true",
        "-e", "DD_DATA_PLANE__DOGSTATSD__ENABLED=true",
        "-e", "DD_LOG_TO_CONSOLE=true",
        "-e", "DD_DISABLE_FILE_LOGGING=true",
        $ImageTag
    )
    Invoke-Native -FilePath docker -Arguments $DockerRunArgs

    Start-Sleep -Seconds 10

    Write-Host "[*] Preflight: docker logs ${ContainerName}"
    docker logs $ContainerName

    $State = docker inspect $ContainerName --format '{{json .State}}'
    Write-Host "[*] Preflight: container state: ${State}"

    $Status = docker inspect $ContainerName --format '{{.State.Status}}'
    if ($Status -ne "running") {
        $ExitCode = docker inspect $ContainerName --format '{{.State.ExitCode}}'
        Remove-DockerContainerIfExists -ContainerName $ContainerName
        throw "Windows ADP preflight container exited early with status=${Status}, exit_code=${ExitCode}."
    }

    Remove-DockerContainerIfExists -ContainerName $ContainerName
}

$RepoRoot = Resolve-Path (Join-Path $PSScriptRoot "..\..")
Set-Location $RepoRoot

Initialize-RustEnvironment

if (-not (Get-Command docker -ErrorAction SilentlyContinue)) {
    throw "Docker CLI not found in Windows build image. The integration job requires Docker CLI access to the host Docker daemon."
}
Invoke-Native docker version

Write-Host "[*] Building Panoramic and Agent Data Plane for Windows..."
Invoke-Native cargo build --release --package panoramic --package agent-data-plane

$WindowsAdpImage = if ($env:WINDOWS_ADP_IMAGE_TAG) { $env:WINDOWS_ADP_IMAGE_TAG } else { "saluki-images/agent-data-plane:testing-windows" }
Build-WindowsAdpImage
Test-WindowsAdpImage -ImageTag $WindowsAdpImage

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
