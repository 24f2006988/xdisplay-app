<#
.SYNOPSIS
    Allows tablets on your Wi-Fi to reach XDisplay (Windows Firewall rules).
.DESCRIPTION
    Windows blocks inbound connections by default, so a tablet can neither find the PC
    (UDP 59101) nor stream from it (TCP 59100) until these rules exist. The rules are limited
    to the local subnet and to Private/Domain networks, so XDisplay is never reachable from the
    internet or from networks you have marked "Public".

    Run it once. It asks for administrator rights by itself. USB needs none of this.
.PARAMETER Remove
    Deletes the rules again.
#>
[CmdletBinding()]
param([switch]$Remove)

$tcpRule = "XDisplay Host (Wi-Fi stream, TCP 59100)"
$udpRule = "XDisplay Host (Wi-Fi discovery, UDP 59101)"
$legacyRule = "XDisplay Host Stream"

$isAdmin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole(
    [Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $isAdmin) {
    Write-Host "Requesting administrator rights..." -ForegroundColor Yellow
    $argList = @("-NoProfile", "-ExecutionPolicy", "Bypass", "-File", "`"$PSCommandPath`"")
    if ($Remove) { $argList += "-Remove" }
    Start-Process powershell.exe -Verb RunAs -ArgumentList $argList -Wait
    exit
}

foreach ($name in @($tcpRule, $udpRule, $legacyRule)) {
    Remove-NetFirewallRule -DisplayName $name -ErrorAction SilentlyContinue
}

if ($Remove) {
    Write-Host "XDisplay firewall rules removed." -ForegroundColor Green
    Start-Sleep -Seconds 2
    exit
}

New-NetFirewallRule -DisplayName $tcpRule -Direction Inbound -Action Allow -Protocol TCP -LocalPort 59100 `
    -Profile Private, Domain -RemoteAddress LocalSubnet | Out-Null
New-NetFirewallRule -DisplayName $udpRule -Direction Inbound -Action Allow -Protocol UDP -LocalPort 59101 `
    -Profile Private, Domain -RemoteAddress LocalSubnet | Out-Null
Write-Host "Firewall rules added (TCP 59100 + UDP 59101, local subnet, Private/Domain networks)." -ForegroundColor Green

# The usual reason Wi-Fi still fails afterwards: the network is classed as Public.
$public = Get-NetConnectionProfile -ErrorAction SilentlyContinue | Where-Object { $_.NetworkCategory -eq "Public" }
if ($public) {
    Write-Host ""
    Write-Host "Note: these connections are marked 'Public', where the rules do not apply:" -ForegroundColor Yellow
    $public | ForEach-Object { Write-Host "  - $($_.Name) ($($_.InterfaceAlias))" }
    Write-Host "If your home or office Wi-Fi is listed, switch it to Private:" -ForegroundColor Yellow
    Write-Host "  Settings > Network & internet > Wi-Fi > <your network> > Network profile type > Private"
}
Start-Sleep -Seconds 3
