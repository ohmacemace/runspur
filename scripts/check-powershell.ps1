[CmdletBinding()]
param()

$ErrorActionPreference = "Stop"
$scripts = @(Get-ChildItem -LiteralPath $PSScriptRoot -Filter "*.ps1" -File)
foreach ($script in $scripts) {
    $tokens = $null
    $errors = $null
    $null = [Management.Automation.Language.Parser]::ParseFile($script.FullName, [ref]$tokens, [ref]$errors)
    if ($errors.Count -gt 0) { throw ($errors | ForEach-Object { $_.ToString() } | Out-String) }
}
Write-Host "PowerShell syntax checked: $($scripts.Count) scripts."
