$ErrorActionPreference = "Stop"
Set-StrictMode -Version 3.0

$pipeName = "datadog-dogstatsd-integration"
$payload = "integration.named_pipe.invalid`n"

$pipe = [System.IO.Pipes.NamedPipeClientStream]::new(
    ".",
    $pipeName,
    [System.IO.Pipes.PipeDirection]::Out
)

try {
    $pipe.Connect(30000)
    $bytes = [System.Text.Encoding]::UTF8.GetBytes($payload)
    $pipe.Write($bytes, 0, $bytes.Length)
    $pipe.Flush()
} finally {
    $pipe.Dispose()
}
