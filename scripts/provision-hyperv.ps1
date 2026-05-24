<#
.SYNOPSIS
    Provisions the `rooms-host` Hyper-V VM for running Firecracker microVMs.

.DESCRIPTION
    Creates a gen2 Hyper-V VM with nested virtualization enabled, attaches an
    Ubuntu Server ISO for interactive install, and prints the next steps.

    Run as Administrator. Hyper-V must be enabled (Windows Features -> Hyper-V).

.PARAMETER VMName
    Name of the VM. Default: rooms-host.

.PARAMETER IsoPath
    Path to an Ubuntu Server ISO. Required. Download from
    https://ubuntu.com/download/server (24.04 LTS recommended).

.PARAMETER MemoryGB
    Startup memory in GB. Default: 8.

.PARAMETER VCpus
    Number of virtual CPUs. Default: 4.

.PARAMETER DiskGB
    Dynamic VHDX size in GB. Default: 80.

.PARAMETER VMRoot
    Where to put VM files. Default: C:\Hyper-V\rooms-host.

.PARAMETER SwitchName
    Virtual switch to attach. Default: Default Switch (built-in NAT).

.EXAMPLE
    .\provision-hyperv.ps1 -IsoPath "C:\Users\me\Downloads\ubuntu-24.04.iso"

.EXAMPLE
    .\provision-hyperv.ps1 -VMName rooms-host -IsoPath C:\iso\ubuntu.iso -MemoryGB 16 -VCpus 8

.NOTES
    After the VM boots and Ubuntu is installed:
      1. Shut down the VM cleanly from inside Ubuntu (`sudo poweroff`).
      2. The script already enabled nested virt -- verify with:
           Get-VMProcessor -VMName rooms-host | Format-List ExposeVirtualizationExtensions
      3. Restart the VM, SSH in, run scripts/setup-rooms-host.sh.
#>

[CmdletBinding()]
param(
    [string]$VMName = "rooms-host",
    [Parameter(Mandatory = $true)]
    [string]$IsoPath,
    [int]$MemoryGB = 8,
    [int]$VCpus = 4,
    [int]$DiskGB = 80,
    [string]$VMRoot = "C:\Hyper-V\rooms-host",
    [string]$SwitchName = "Default Switch"
)

$ErrorActionPreference = "Stop"

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

function Require-Iso {
    if (-not (Test-Path -LiteralPath $IsoPath)) {
        throw "ISO not found at: $IsoPath"
    }
}

function Ensure-VMRoot {
    if (-not (Test-Path -LiteralPath $VMRoot)) {
        New-Item -ItemType Directory -Path $VMRoot -Force | Out-Null
        Write-Host "created VM root: $VMRoot"
    }
}

function VM-Exists {
    $existing = Get-VM -Name $VMName -ErrorAction SilentlyContinue
    return $null -ne $existing
}

# --- main ---

Require-Admin
Require-HyperV
Require-Iso
Ensure-VMRoot

if (VM-Exists) {
    throw "VM '$VMName' already exists. Remove it first (Remove-VM -Name $VMName -Force) or pick a different -VMName."
}

$vhdxPath = Join-Path $VMRoot "$VMName.vhdx"

Write-Host "creating VM '$VMName'..."
Write-Host "  memory:    ${MemoryGB}GB startup, dynamic"
Write-Host "  vcpus:     $VCpus"
Write-Host "  disk:      ${DiskGB}GB dynamic at $vhdxPath"
Write-Host "  switch:    $SwitchName"
Write-Host "  iso:       $IsoPath"
Write-Host ""

# Create the VM (gen2 supports UEFI + secure boot toggle + better Linux support).
New-VM -Name $VMName `
    -Generation 2 `
    -MemoryStartupBytes (${MemoryGB} * 1GB) `
    -NewVHDPath $vhdxPath `
    -NewVHDSizeBytes (${DiskGB} * 1GB) `
    -SwitchName $SwitchName `
    -Path $VMRoot | Out-Null

# Disable secure boot -- Ubuntu Server installer works either way, but
# disabling avoids the "Microsoft UEFI CA" cert requirement.
Set-VMFirmware -VMName $VMName -EnableSecureBoot Off

# Set vCPUs.
Set-VMProcessor -VMName $VMName -Count $VCpus

# Enable nested virtualization (required for /dev/kvm inside the guest).
Set-VMProcessor -VMName $VMName -ExposeVirtualizationExtensions $true

# Allocate dynamic memory range (min 2GB, max 2x startup).
Set-VMMemory -VMName $VMName `
    -DynamicMemoryEnabled $true `
    -MinimumBytes 2GB `
    -StartupBytes (${MemoryGB} * 1GB) `
    -MaximumBytes (${MemoryGB} * 2 * 1GB)

# Attach the Ubuntu ISO as a DVD drive.
Add-VMDvdDrive -VMName $VMName -Path $IsoPath

# Set boot order: DVD first, then VHDX.
$dvd = Get-VMDvdDrive -VMName $VMName
$hd = Get-VMHardDiskDrive -VMName $VMName
Set-VMFirmware -VMName $VMName -BootOrder $dvd, $hd

# Enable MAC address spoofing on the network adapter so the TAP bridge
# inside the guest works (Firecracker microVMs route through this).
Get-VMNetworkAdapter -VMName $VMName | Set-VMNetworkAdapter -MacAddressSpoofing On

# Sanity-check nested virt landed.
$np = Get-VMProcessor -VMName $VMName
if (-not $np.ExposeVirtualizationExtensions) {
    throw "nested virtualization did NOT enable on '$VMName' -- your CPU may not support it, or Hyper-V refused. Check Get-VMProcessor output."
}

Write-Host ""
Write-Host "VM '$VMName' created successfully."
Write-Host ""
Write-Host "next steps:"
Write-Host "  1. Start the VM and connect:"
Write-Host "       Start-VM -Name $VMName"
Write-Host "       vmconnect.exe localhost $VMName"
Write-Host ""
Write-Host "  2. Walk through the Ubuntu Server installer (~15 min):"
Write-Host "       - default partition layout is fine (use entire disk)"
Write-Host "       - create a user 'rooms' with a password you'll remember"
Write-Host "       - install the OpenSSH server when prompted"
Write-Host "       - reboot when finished"
Write-Host ""
Write-Host "  3. After reboot, find the VM's IP:"
Write-Host "       Get-VMNetworkAdapter -VMName $VMName | Select-Object IPAddresses"
Write-Host "     SSH in:"
Write-Host "       ssh rooms@<ip>"
Write-Host ""
Write-Host "  4. From inside the VM, clone this repo and run the setup script:"
Write-Host "       git clone https://github.com/itsHabib/rooms.git ~/rooms"
Write-Host "       bash ~/rooms/scripts/setup-rooms-host.sh"
Write-Host ""
