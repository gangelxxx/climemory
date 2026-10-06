$cmTimer = [Diagnostics.Stopwatch]::StartNew()
$env:CM_HOOK_WRAPPER_ID = [Guid]::NewGuid().ToString('N')
$cmWriter = $null
try {
    $cmDir = [IO.Path]::Combine([IO.Path]::GetTempPath(), 'climemory-hook-startup')
    [void][IO.Directory]::CreateDirectory($cmDir)
    if (([IO.File]::GetAttributes($cmDir) -band [IO.FileAttributes]::ReparsePoint) -eq 0) {
        $cmPath = [IO.Path]::Combine($cmDir, ('wrapper-' + $env:CM_HOOK_WRAPPER_ID + '.jsonl'))
        $cmFile = [IO.File]::Open($cmPath, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::ReadWrite)
        $cmWriter = [IO.StreamWriter]::new($cmFile)
        $cmWriter.AutoFlush = $true
    }
} catch {}
function Write-CmStage($cmStage, $cmCode) {
    try {
        if ($null -ne $cmWriter) {
            $cmRow = @{stage=$cmStage; elapsed_ms=$cmTimer.ElapsedMilliseconds; utc=[DateTime]::UtcNow.ToString('o'); pid=$PID; wrapper_id=$env:CM_HOOK_WRAPPER_ID; exit_code=$cmCode}
            $cmWriter.WriteLine(($cmRow | ConvertTo-Json -Compress))
        }
    } catch {}
}
Write-CmStage 'wrapper_enter' $null
$cmExit = 1
try {
    Write-CmStage 'native_start' $null
    $global:LASTEXITCODE = 1
    & '__CM_EXECUTABLE__' hooks codex
    $cmExit = $LASTEXITCODE
    Write-CmStage 'native_return' $cmExit
} finally {
    Write-CmStage 'wrapper_exit' $cmExit
    try { if ($null -ne $cmWriter) { $cmWriter.Dispose() } } catch {}
}
exit $cmExit
