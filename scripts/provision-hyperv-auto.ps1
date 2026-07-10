<#
.SYNOPSIS
    Hands-free provisioning of the `rooms-host` Hyper-V VM from an Ubuntu
    cloud image -- no interactive installer.

.DESCRIPTION
    Takes a pre-converted Ubuntu cloud-image VHDX (qcow2 -> vhdx via qemu-img;
    see .NOTES), builds a cloud-init NoCloud seed ISO (cidata-labelled, authored
    in WSL) from your local SSH public keys and attaches it as a DVD, creates a
    gen2 VM with nested virtualization, boots it, and polls until the guest
    reports an IPv4 -- then prints the ready-to-use ssh line.

    The pristine base VHDX is never booted: it is copied to a per-VM work
    disk first, so re-provisioning (-Force) always starts from a clean image.

    Run as Administrator. Hyper-V must be enabled. A WSL distro (default
    'Ubuntu') with network access is required to author the seed ISO -- the
    script installs cloud-image-utils there on first use.

.PARAMETER VMName
    Name of the VM. Default: rooms-host.

.PARAMETER BaseVhdx
    Path to the pristine cloud-image VHDX. Default: C:\Hyper-V\rooms-host\os.vhdx.

.PARAMETER Username
    Guest admin user cloud-init creates. Default: mh.

.PARAMETER PubKeyPaths
    SSH public keys authorized for the guest user. Defaults to
    ~\.ssh\id_ed25519.pub and ~\.ssh\id_rooms_host.pub (missing ones are
    skipped; at least one must exist).

.PARAMETER MemoryGB
    Static memory in GB (nested virtualization requires static memory).
    Default: 8.

.PARAMETER VCpus
    Virtual CPUs. Default: 4.

.PARAMETER DiskGB
    Size the work disk is expanded to; cloud-init growpart fills it on first
    boot. Default: 80.

.PARAMETER SwitchName
    Virtual switch. Default: Default Switch (built-in NAT + DHCP).

.PARAMETER Force
    If the VM already exists, tear it down (VM + work disk + seed) and
    recreate from the pristine base.

.EXAMPLE
    .\provision-hyperv-auto.ps1

.EXAMPLE
    .\provision-hyperv-auto.ps1 -MemoryGB 16 -VCpus 8 -Force

.NOTES
    Producing the base VHDX from the official cloud image (once):
      curl -LO https://cloud-images.ubuntu.com/releases/noble/release/ubuntu-24.04-server-cloudimg-amd64.img
      # verify against SHA256SUMS from the same directory, then:
      qemu-img convert -f qcow2 -O vhdx -o subformat=dynamic \
          ubuntu-24.04-server-cloudimg-amd64.img os.vhdx
    (qemu-img via WSL `apt install qemu-utils` works fine on Windows.)

    After the script prints the ssh line: clone the rooms repo in the guest
    and run scripts/setup-rooms-host.sh -- this script's job ends at
    SSH-reachable.
#>

[CmdletBinding()]
param(
    [string]$VMName = "rooms-host",
    [string]$BaseVhdx = "C:\Hyper-V\rooms-host\os.vhdx",
    [string]$Username = "mh",
    [string[]]$PubKeyPaths = @(
        (Join-Path $env:USERPROFILE ".ssh\id_ed25519.pub"),
        (Join-Path $env:USERPROFILE ".ssh\id_rooms_host.pub")
    ),
    [int]$MemoryGB = 8,
    [int]$VCpus = 4,
    [int]$DiskGB = 80,
    [string]$SwitchName = "Default Switch",
    [string]$WslDistro = "Ubuntu",
    [switch]$Force
)

$ErrorActionPreference = "Stop"

function Log([string]$msg) {
    Write-Host "[provision-auto] $msg" -ForegroundColor Cyan
}

function ConvertTo-WslPath([string]$p) {
    # C:\Hyper-V\x -> /mnt/c/Hyper-V/x  (drive letter lowercased, slashes flipped)
    $p = $p -replace '\\', '/'
    if ($p -match '^([A-Za-z]):(.*)$') {
        return "/mnt/$($matches[1].ToLower())$($matches[2])"
    }
    return $p
}

function Require-Admin {
    $current = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = New-Object Security.Principal.WindowsPrincipal($current)
    if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
        throw "this script must run as Administrator (Hyper-V cmdlets require it)"
    }
}

function Require-HyperV {
    $feature = Get-WindowsOptionalFeature -Online -FeatureName Microsoft-Hyper-V-All -ErrorAction SilentlyContinue
    if ($null -eq $feature -or $feature.State -ne "Enabled") {
        throw "Hyper-V is not enabled. Run: Enable-WindowsOptionalFeature -Online -FeatureName Microsoft-Hyper-V -All  (then reboot)"
    }
}

Require-Admin
Require-HyperV

if (-not (Test-Path -LiteralPath $BaseVhdx)) {
    throw "base VHDX not found at: $BaseVhdx (see .NOTES for producing it from the Ubuntu cloud image)"
}

# --- collect SSH public keys -------------------------------------------------
$keys = @()
foreach ($p in $PubKeyPaths) {
    if (Test-Path -LiteralPath $p) {
        $keys += (Get-Content -LiteralPath $p -Raw).Trim()
        Log "authorized key: $p"
    }
}
if ($keys.Count -eq 0) {
    throw "no SSH public keys found (looked at: $($PubKeyPaths -join ', ')); pass -PubKeyPaths"
}

# --- existing VM handling ----------------------------------------------------
$vmRoot = Split-Path -Parent $BaseVhdx
$workVhdx = Join-Path $vmRoot "$VMName-boot.vhdx"
$seedIso  = Join-Path $vmRoot "$VMName-seed.iso"
# Legacy FAT seed disk from the pre-ISO approach; cleaned up if a prior run left one.
$legacySeedVhdx = Join-Path $vmRoot "$VMName-seed.vhdx"

$existing = Get-VM -Name $VMName -ErrorAction SilentlyContinue
if ($existing) {
    if (-not $Force) {
        throw "VM '$VMName' already exists; pass -Force to tear down and recreate"
    }
    Log "removing existing VM '$VMName' (-Force)"
    if ($existing.State -ne "Off") { Stop-VM -Name $VMName -TurnOff -Force }
    Remove-VM -Name $VMName -Force
}
foreach ($artifact in @($workVhdx, $seedIso, $legacySeedVhdx)) {
    if (Test-Path -LiteralPath $artifact) {
        if (-not $Force) { throw "leftover artifact exists: $artifact (pass -Force to overwrite)" }
        # A prior failed run may have left a seed VHD attached; detach before
        # deleting so Remove-Item doesn't hit a sharing violation (no-op for .iso).
        if ($artifact -like '*.vhdx') { Dismount-VHD -Path $artifact -ErrorAction SilentlyContinue }
        Remove-Item -LiteralPath $artifact -Force
    }
}

# --- work disk: pristine base -> boot disk, expanded -------------------------
Log "copying pristine base -> $workVhdx"
Copy-Item -LiteralPath $BaseVhdx -Destination $workVhdx
Log "expanding work disk to ${DiskGB}GB (cloud-init growpart fills it)"
Resize-VHD -Path $workVhdx -SizeBytes ([int64]$DiskGB * 1GB)

# --- cloud-init NoCloud seed (cidata-labelled ISO, built in WSL) --------------
Log "building cloud-init seed ISO: $seedIso"
$keysYaml = ($keys | ForEach-Object { "      - $_" }) -join "`n"
$userData = @"
#cloud-config
hostname: $VMName
users:
  - name: $Username
    groups: [sudo]
    shell: /bin/bash
    sudo: ALL=(ALL) NOPASSWD:ALL
    ssh_authorized_keys:
$keysYaml
ssh_pwauth: false
package_update: true
packages:
  - linux-cloud-tools-virtual
  - linux-tools-virtual
"@
$metaData = @"
instance-id: $VMName-$(Get-Date -Format yyyyMMddHHmmss)
local-hostname: $VMName
"@

# Build the seed as an ISO in WSL rather than by mounting a FAT VHD on the host.
# A locked-down host may enforce BitLocker-To-Go ("deny write to removable
# drives not BitLocker-protected") -- Format-Volume (a management op) succeeds,
# but the file write to the removable seed volume is policy-blocked, and a tiny
# seed volume is too small to BitLocker. Writing the seed files + ISO to the
# fixed C: drive and letting WSL author the ISO sidesteps that entirely;
# cloud-init reads a cidata-labelled CD identically to a cidata FAT volume.
$seedSrc = Join-Path $vmRoot "$VMName-seed-src"
if (Test-Path -LiteralPath $seedSrc) { Remove-Item -Recurse -Force $seedSrc }
New-Item -ItemType Directory -Force -Path $seedSrc | Out-Null
# LF endings: cloud-init YAML must not carry CRLF.
[IO.File]::WriteAllText((Join-Path $seedSrc "user-data"), ($userData -replace "`r`n", "`n"))
[IO.File]::WriteAllText((Join-Path $seedSrc "meta-data"), ($metaData -replace "`r`n", "`n"))

$isoWsl = ConvertTo-WslPath $seedIso
$udWsl  = ConvertTo-WslPath (Join-Path $seedSrc "user-data")
$mdWsl  = ConvertTo-WslPath (Join-Path $seedSrc "meta-data")
# Self-heal the toolchain: install cloud-image-utils (cloud-localds) + genisoimage
# on first use, then author the cidata ISO.
$wslCmd = "command -v cloud-localds >/dev/null || { apt-get update -qq && apt-get install -y -qq cloud-image-utils genisoimage; } >/dev/null 2>&1; cloud-localds '$isoWsl' '$udWsl' '$mdWsl'"
& wsl.exe -d $WslDistro -u root -- bash -lc $wslCmd
if ($LASTEXITCODE -ne 0) {
    throw "WSL seed build failed (exit $LASTEXITCODE); need WSL distro '$WslDistro' with network access for cloud-image-utils"
}
if (-not (Test-Path -LiteralPath $seedIso)) { throw "seed ISO not produced at $seedIso" }
Remove-Item -Recurse -Force $seedSrc -ErrorAction SilentlyContinue
Log "seed ISO built: $seedIso"

# --- VM ------------------------------------------------------------------------
Log "creating gen2 VM '$VMName' (${MemoryGB}GB static, $VCpus vCPU, nested virt)"
$vm = New-VM -Name $VMName -Generation 2 -MemoryStartupBytes ([int64]$MemoryGB * 1GB) `
    -VHDPath $workVhdx -SwitchName $SwitchName
# Nested virtualization requires static memory.
Set-VMMemory -VMName $VMName -DynamicMemoryEnabled $false
Set-VMProcessor -VMName $VMName -Count $VCpus -ExposeVirtualizationExtensions $true
# Ubuntu boots fine with secure boot off; keeping it off avoids template drift.
Set-VMFirmware -VMName $VMName -EnableSecureBoot Off
# The cloud-init seed rides as a DVD (cidata ISO), not a data disk.
Add-VMDvdDrive -VMName $VMName -Path $seedIso
$osDrive = Get-VMHardDiskDrive -VMName $VMName | Where-Object Path -eq $workVhdx
Set-VMFirmware -VMName $VMName -FirstBootDevice $osDrive
Set-VM -Name $VMName -AutomaticCheckpointsEnabled $false

Log "starting VM"
Start-VM -Name $VMName

# --- discover the guest IPv4 and wait for SSH --------------------------------
# Prefer the host ARP/neighbor table (the guest shows up there as soon as it
# DHCPs, early in boot) over Hyper-V KVP (Get-VMNetworkAdapter IPAddresses),
# which only reports once the guest tools install during first-boot cloud-init
# -- slow, and on some cloud images it never lands within a sane window. Match
# on the VM's own MAC so another guest is never mistaken for this one, and accept
# an address only once sshd actually answers.
$macDash = (((Get-VMNetworkAdapter -VMName $VMName).MacAddress -replace '(..)(?=.)', '$1-')).ToUpper()
$timeoutMin = 6
Log "waiting for guest IPv4 (host neighbor table + KVP fallback; up to ~$timeoutMin min)"
$deadline = (Get-Date).AddMinutes($timeoutMin)
$ip = $null
while ((Get-Date) -lt $deadline) {
    Start-Sleep -Seconds 5
    # Fast path: the VM's MAC resolved in the host neighbor table.
    $cand = Get-NetNeighbor -AddressFamily IPv4 -ErrorAction SilentlyContinue |
        Where-Object { $_.LinkLayerAddress -eq $macDash -and $_.IPAddress -match '^\d+\.\d+\.\d+\.\d+$' -and $_.IPAddress -notlike '169.254.*' } |
        Select-Object -First 1 -ExpandProperty IPAddress
    # Fallback: KVP, if the guest tools happen to already be up.
    if (-not $cand) {
        $cand = (Get-VMNetworkAdapter -VMName $VMName).IPAddresses |
            Where-Object { $_ -match '^\d+\.\d+\.\d+\.\d+$' -and $_ -notlike '169.254.*' } |
            Select-Object -First 1
    }
    # Done only once sshd answers -- an ARP entry alone can precede a live sshd.
    if ($cand -and (Test-NetConnection -ComputerName $cand -Port 22 -WarningAction SilentlyContinue).TcpTestSucceeded) {
        $ip = $cand
        break
    }
}

if ($ip) {
    Log "guest up: $ip (ssh port 22 open)"
    Write-Host ""
    Write-Host "  ssh $Username@$ip" -ForegroundColor Green
    Write-Host ""
    Log "next: clone the rooms repo in the guest and run scripts/setup-rooms-host.sh"
}
else {
    Log "no SSH-reachable IPv4 within $timeoutMin min -- the guest may still be booting."
    Log "find it: Get-NetNeighbor -InterfaceAlias 'vEthernet ($SwitchName)' | Where-Object LinkLayerAddress -eq '$macDash'"
    Log "or open the VM console: vmconnect.exe localhost $VMName"
}
