# Qualify a built ashe-worker.exe: its stamped revision, input validation,
# and telemetry export, each run with an explicit synthetic input allowlist.
param(
    [Parameter(Mandatory = $true)][string]$Executable,
    [Parameter(Mandatory = $true)][string]$Revision
)
$ErrorActionPreference = 'Stop'

$work = Join-Path ([System.IO.Path]::GetTempPath()) ("ashe-worker-qualify-" + [guid]::NewGuid())
New-Item -ItemType Directory -Path $work | Out-Null

function Invoke-Worker([string[]]$Arguments, [hashtable]$Inputs) {
    foreach ($name in [Environment]::GetEnvironmentVariables().Keys) {
        if ($name -match '^(ASHE_WORKER_|OTEL_)') {
            [Environment]::SetEnvironmentVariable($name, $null)
        }
    }
    foreach ($entry in $Inputs.GetEnumerator()) {
        [Environment]::SetEnvironmentVariable($entry.Key, $entry.Value)
    }
    $output = Join-Path $work ("output-" + [guid]::NewGuid() + ".txt")
    $process = Start-Process -FilePath $Executable -ArgumentList $Arguments -NoNewWindow -Wait `
        -PassThru -RedirectStandardOutput $output
    [pscustomobject]@{ Code = $process.ExitCode; Output = (Get-Content -Raw -LiteralPath $output) }
}

function Assert-Result($Name, $Result, [int]$Code, [string]$Pattern) {
    if ($Result.Code -ne $Code -or $Result.Output -notmatch $Pattern) {
        throw "qualification '$Name' failed: exit $($Result.Code), output: $($Result.Output)"
    }
    Write-Output "qualification '$Name' passed"
}

try {
    $overlay = Join-Path $work 'config.env'
    $protected = Join-Path $work 'protected.env'
    Set-Content -LiteralPath $overlay -Encoding ascii -Value @(
        'ASHE_WORKER_PROFILE=production',
        "ASHE_WORKER_DATA_DIR=$(Join-Path $work 'data')",
        'ASHE_WORKER_LLM_BASE_URL=http://127.0.0.1:9/v1',
        'ASHE_WORKER_GRAMMAR_MODEL=synthetic-grammar',
        'ASHE_WORKER_QUESTION_MODEL=synthetic-question',
        'ASHE_WORKER_JOURNAL_MODEL=synthetic-journal',
        'ASHE_WORKER_SERVER_URL=http://127.0.0.1:9',
        'OTEL_EXPORTER_OTLP_ENDPOINT=http://127.0.0.1:9'
    )
    Set-Content -LiteralPath $protected -Encoding ascii -Value @(
        "ASHE_WORKER_LLM_API_KEY='synthetic-llm-key'",
        "ASHE_WORKER_SERVER_TOKEN='synthetic-server-token'",
        "OTEL_EXPORTER_OTLP_HEADERS='authorization=synthetic'"
    )
    $user = [System.Security.Principal.WindowsIdentity]::GetCurrent().Name
    & icacls.exe $protected /inheritance:r /grant:r "${user}:F" | Out-Null
    if ($LASTEXITCODE -ne 0) { throw 'icacls failed to protect the synthetic protected file' }
    $files = @{ ASHE_WORKER_OVERLAY_FILE = $overlay; ASHE_WORKER_PROTECTED_FILE = $protected }

    Assert-Result 'revision' (Invoke-Worker @('--version') @{}) 0 "^ashe-worker $([regex]::Escape($Revision))\r?\n$"
    Assert-Result 'inputs accepted' (Invoke-Worker @('--check-inputs') $files) 0 'inputs: ok'
    Assert-Result 'missing protected file refused' `
        (Invoke-Worker @('--check-inputs') @{ ASHE_WORKER_OVERLAY_FILE = $overlay; ASHE_WORKER_PROTECTED_FILE = (Join-Path $work 'absent.env') }) `
        2 'ASHE_WORKER_LLM_API_KEY'
    $console = $files + @{ OTEL_TRACES_EXPORTER = 'console'; OTEL_METRICS_EXPORTER = 'console'; OTEL_LOGS_EXPORTER = 'console' }
    Assert-Result 'telemetry export' (Invoke-Worker @('--qualify-observability') $console) 0 '(?s)ashe\.worker\.qualification\.canary.*ashe\.worker\.qualification\.canary\.count'
}
finally {
    Remove-Item -Recurse -Force -LiteralPath $work
}
