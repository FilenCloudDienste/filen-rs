# Installs a chocolatey package a CI job needs and fails unless the tool is present afterwards.
# choco can exit 0 having installed nothing: a feed 504 ends in "Chocolatey installed 0/0
# packages", and the job then runs without the tool. So the install is retried with backoff and
# judged by the tool itself, never by choco's exit code. A tool that is already present is not
# installed again.
#
#   scripts/choco-install.ps1 <winfsp|nasm>

param(
	[Parameter(Mandatory)]
	[ValidateSet('winfsp', 'nasm')]
	[string] $Package
)

$ErrorActionPreference = 'Stop'
# A failed choco run is retried below, not fatal.
$PSNativeCommandUseErrorActionPreference = $false

# WinFsp as filen-sdk-rs/src/rclone/fuse.rs detects it: the InstallDir value of HKLM\SOFTWARE\WinFsp
# in the 32-bit registry view (where its installer writes it), holding bin\winfsp-<arch>.dll.
function Test-WinFsp {
	$hklm = [Microsoft.Win32.RegistryKey]::OpenBaseKey('LocalMachine', 'Registry32')
	$key = $hklm.OpenSubKey('SOFTWARE\WinFsp')
	if ($null -eq $key) {
		return $false
	}
	$dir = $key.GetValue('InstallDir')
	$key.Close()
	if (-not $dir) {
		return $false
	}
	$dll = if ($env:PROCESSOR_ARCHITECTURE -eq 'ARM64') { 'winfsp-a64.dll' } else { 'winfsp-x64.dll' }
	Test-Path -PathType Leaf (Join-Path $dir "bin\$dll")
}

# nasm where the workflows put it on GITHUB_PATH, or already on PATH.
function Test-Nasm {
	(Test-Path -PathType Leaf 'C:\Program Files\NASM\nasm.exe') -or
		($null -ne (Get-Command nasm.exe -ErrorAction SilentlyContinue))
}

function Test-Installed {
	switch ($Package) {
		'winfsp' { Test-WinFsp }
		'nasm' { Test-Nasm }
	}
}

if (Test-Installed) {
	Write-Host "$Package is already installed"
	exit 0
}

$attempts = 5
for ($attempt = 1; $attempt -le $attempts; $attempt++) {
	choco install $Package -y --no-progress
	if (Test-Installed) {
		Write-Host "$Package installed (attempt $attempt of $attempts)"
		exit 0
	}
	if ($attempt -lt $attempts) {
		$delay = 15 * $attempt
		Write-Host "$Package is still missing after attempt $attempt of $attempts (choco exit code $LASTEXITCODE), retrying in $delay s"
		Start-Sleep -Seconds $delay
	}
}

Write-Host "::error::choco did not install $Package after $attempts attempts"
exit 1
