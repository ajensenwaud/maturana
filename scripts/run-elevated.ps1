param(
    [Parameter(Mandatory=$true)]
    [string]$ScriptPath,
    [Parameter(ValueFromRemainingArguments=$true)]
    [string[]]$Arguments = @()
)

$resolved = Resolve-Path $ScriptPath
$argumentList = @(
    "-NoProfile",
    "-ExecutionPolicy", "Bypass",
    "-File", "`"$resolved`""
) + $Arguments

Start-Process powershell -Verb RunAs -ArgumentList $argumentList -Wait
