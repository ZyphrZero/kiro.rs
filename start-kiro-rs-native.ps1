# PowerShell Native Startup Script for kiro-rs

$bin_path = "c:\Users\User\kiro\target\release\kiro-rs.exe"
$config_path = "c:\Users\User\kiro\data\config.json"
$credentials_path = "c:\Users\User\kiro\data\credentials.json"

Write-Host "Stopping any existing kiro-rs native processes..."
Stop-Process -Name "kiro-rs" -Force -ErrorAction SilentlyContinue

# Verify binary exists
if (-not (Test-Path $bin_path)) {
    Write-Error "Error: kiro-rs.exe not found at $bin_path. Please build it first."
    exit 1
}

Write-Host "Starting kiro-rs natively in the background via WMI..."
# Run in background with stdout/stderr directed to logs
$log_dir = "c:\Users\User\kiro\data"
if (-not (Test-Path $log_dir)) {
    New-Item -ItemType Directory -Force -Path $log_dir | Out-Null
}

$log_file = "$log_dir\kiro-rs-native.log"
$log_err_file = "$log_dir\kiro-rs-native-error.log"

# Use WMI/CIM process creation to escape the parent job session tree
$command = "powershell.exe -Command `"Start-Process -FilePath '$bin_path' -ArgumentList '-c', '$config_path', '--credentials', '$credentials_path' -RedirectStandardOutput '$log_file' -RedirectStandardError '$log_err_file' -NoNewWindow`""
$res = Invoke-CimMethod -ClassName Win32_Process -MethodName Create -Arguments @{ CommandLine = $command }

if ($res.ReturnValue -eq 0) {
    Write-Host "Kiro.rs gateway started successfully via WMI! Process ID: $($res.ProcessId)"
} else {
    Write-Error "Failed to start kiro-rs via WMI. Return value: $($res.ReturnValue)"
    exit 1
}

Write-Host "Logs are redirecting to: $log_file"
Write-Host "API Endpoint: http://localhost:8990/v1/messages"
Write-Host "Admin Web UI: http://localhost:8990/admin"
