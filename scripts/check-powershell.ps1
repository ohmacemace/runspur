[CmdletBinding()]
param()

$ErrorActionPreference = "Stop"
$root = Split-Path $PSScriptRoot -Parent
$scripts = @(Get-Item -LiteralPath (Join-Path $root "install.ps1")) + @(Get-ChildItem -LiteralPath $PSScriptRoot -Filter "*.ps1" -File)
foreach ($script in $scripts) {
    $tokens = $null
    $errors = $null
    $null = [Management.Automation.Language.Parser]::ParseFile($script.FullName, [ref]$tokens, [ref]$errors)
    if ($errors.Count -gt 0) { throw ($errors | ForEach-Object { $_.ToString() } | Out-String) }
}
Write-Host "PowerShell syntax checked: $($scripts.Count) scripts."
