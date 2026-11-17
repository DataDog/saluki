# Uploads a JUnit report to Datadog Test Optimization from a Windows CI runner. This is the Windows counterpart of
# upload-test-results.sh.
#
# Usage: powershell -File windows-upload-test-results.ps1 -JUnitPath <junit.xml>
#
# The Datadog API key comes from dd-sts, which exchanges the job's GitLab ID token (`DD_STS_OIDC_TOKEN`, with audience
# `dd-sts`) for a short-lived key under the `saluki-test-optimization` policy. dd-sts doesn't ship a Windows CLI, so
# this calls the exchange endpoint that `dd-sts exchange` uses directly.
#
# `datadog-ci` comes from the runner.
#
# This never fails the job: a missing report or a failed upload only prints a warning.

param(
    [Parameter(Mandatory = $true)]
    [string]$JUnitPath
)

Set-StrictMode -Version 3.0
$ErrorActionPreference = "Stop"

$DdStsEndpoint = "https://dd-sts.us1.ddbuild.io/sts/datadog/exchange"
$DdStsPolicy = "saluki-test-optimization"

function Exit-WithWarning {
    param([string]$Message)

    Write-Host "WARNING: ${Message}; not uploading test results."
    exit 0
}

function Get-DatadogCi {
    foreach ($Candidate in @("C:\datadog-ci.exe")) {
        if (Test-Path $Candidate) {
            return $Candidate
        }
    }
    $Command = Get-Command datadog-ci -ErrorAction SilentlyContinue
    if ($Command) {
        return $Command.Source
    }
    return $null
}

function Get-DatadogApiKey {
    param([string]$IdToken)

    # Mirrors the retry policy of `dd-sts exchange`: retry network errors and 502/503/504, up to 3 attempts.
    $Uri = "${DdStsEndpoint}?policy=${DdStsPolicy}"
    $Headers = @{ Authorization = "Bearer ${IdToken}" }
    for ($Attempt = 1; ; $Attempt++) {
        try {
            $Response = Invoke-RestMethod -UseBasicParsing -Method Get -Uri $Uri -Headers $Headers -TimeoutSec 30
            return $Response.api_key
        } catch {
            $StatusCode = $null
            if ($_.Exception.PSObject.Properties["Response"] -and $_.Exception.Response) {
                $StatusCode = [int]$_.Exception.Response.StatusCode
            }
            $Retryable = ($null -eq $StatusCode) -or (@(502, 503, 504) -contains $StatusCode)
            if (-not $Retryable -or $Attempt -ge 3) {
                throw "dd-sts exchange failed (status: $StatusCode): $($_.Exception.Message)"
            }
            $Delay = [math]::Pow(2, $Attempt)
            Write-Host "dd-sts exchange failed (status: $StatusCode), retrying in ${Delay}s..."
            Start-Sleep -Seconds $Delay
        }
    }
}

if (-not (Test-Path $JUnitPath -PathType Leaf)) {
    Exit-WithWarning "JUnit report '${JUnitPath}' not found"
}

$DatadogCi = Get-DatadogCi
if (-not $DatadogCi) {
    Exit-WithWarning "datadog-ci not found"
}

if (-not $env:DD_STS_OIDC_TOKEN) {
    Exit-WithWarning "DD_STS_OIDC_TOKEN not set"
}

# Windows PowerShell 5.1 doesn't enable TLS 1.2 by default.
[Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

try {
    $ApiKey = Get-DatadogApiKey -IdToken $env:DD_STS_OIDC_TOKEN
} catch {
    Exit-WithWarning $_.Exception.Message
}
if (-not $ApiKey) {
    Exit-WithWarning "dd-sts returned no API key"
}

# datadog-ci's git metadata sync passes every recent commit SHA on one command line, which exceeds the Windows command
# line length limit, so skip it. Test results are uploaded either way.
$env:DATADOG_SITE = "datadoghq.com"
$env:DD_API_KEY = $ApiKey
try {
    & $DatadogCi junit upload --service saluki --skip-git-metadata-upload $JUnitPath
    $UploadExitCode = $LASTEXITCODE
} catch {
    $UploadExitCode = -1
    Write-Host $_.Exception.Message
} finally {
    Remove-Item Env:\DD_API_KEY -ErrorAction SilentlyContinue
}

if ($UploadExitCode -ne 0) {
    Exit-WithWarning "datadog-ci junit upload failed (exit code ${UploadExitCode})"
}
exit 0
